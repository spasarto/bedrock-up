//! Spawns and supervises a single `bedrock_server` (or stand-in) child
//! process: builds the right launch command for the platform, relays its
//! console in both directions, and can stop it politely — warn players on a
//! countdown, send `stop`, wait, and kill only if it doesn't exit in time.
//!
//! Bedrock Dedicated Server has no RCON; stdin is the only control channel,
//! and only the parent that spawned the child owns that channel.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::args::ServerKind;

#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    #[error("a server process is already running out of {}", path.display())]
    AlreadyRunning { path: PathBuf },

    #[error("failed to spawn server process: {0}")]
    Spawn(#[source] io::Error),

    #[error("I/O error talking to the server process: {0}")]
    Io(#[from] io::Error),
}

#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub server_path: PathBuf,
    pub kind: ServerKind,
    /// Overrides the default executable for `kind` when set.
    pub server_exe: Option<String>,
    /// Extra arguments appended after the executable's own defaults.
    pub extra_args: Vec<String>,
}

/// How a [`Server::graceful_stop`] ended.
#[derive(Debug)]
pub enum StopOutcome {
    /// The process exited on its own within the timeout.
    Exited(ExitStatus),
    /// The process did not exit in time and was killed.
    Killed,
}

/// A running server process, with its console relayed and its stdin
/// available for commands like `say` and `stop`.
pub struct Server {
    child: Child,
    stdin: ChildStdin,
}

impl Server {
    /// Spawns the configured server. Refuses to start if a process is
    /// already running out of `config.server_path` — two servers writing
    /// the same world is data loss, not a race worth risking.
    ///
    /// `on_line` receives every line the child writes to stdout or stderr,
    /// interleaved, so the caller can relay or log the console.
    pub fn spawn(
        config: &ServerConfig,
        on_line: impl Fn(&str) + Clone + Send + 'static,
    ) -> Result<Self, ServerError> {
        let running = bedrock_up::process::find_server_processes(&config.server_path);
        if !running.is_empty() {
            return Err(ServerError::AlreadyRunning {
                path: config.server_path.clone(),
            });
        }

        let mut command = build_command(config);
        command
            .current_dir(&config.server_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = command.spawn().map_err(ServerError::Spawn)?;
        let stdin = child.stdin.take().expect("child spawned with piped stdin");
        let stdout = child
            .stdout
            .take()
            .expect("child spawned with piped stdout");
        let stderr = child
            .stderr
            .take()
            .expect("child spawned with piped stderr");

        let stdout_sink = on_line.clone();
        thread::spawn(move || relay(stdout, stdout_sink));
        thread::spawn(move || relay(stderr, on_line));

        Ok(Server { child, stdin })
    }

    /// Writes a line to the child's stdin, e.g. `say <message>` or `stop`.
    pub fn send_line(&mut self, line: &str) -> io::Result<()> {
        writeln!(self.stdin, "{line}")?;
        self.stdin.flush()
    }

    /// Non-blocking check for whether the child has exited.
    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.child.try_wait()
    }

    fn wait_timeout(&mut self, timeout: Duration) -> io::Result<Option<ExitStatus>> {
        let start = Instant::now();
        loop {
            if let Some(status) = self.child.try_wait()? {
                return Ok(Some(status));
            }
            if start.elapsed() >= timeout {
                return Ok(None);
            }
            thread::sleep(Duration::from_millis(50));
        }
    }

    fn kill_and_wait(&mut self) -> io::Result<ExitStatus> {
        self.child.kill()?;
        self.child.wait()
    }

    /// Warns players on a countdown (`warn_at`, seconds before stop, e.g.
    /// `[60, 30, 10]`), sends `stop`, and waits up to `stop_timeout` for the
    /// process to exit on its own — a clean `stop` lets the world save.
    /// Kills it if it hasn't exited by then; whatever files it still held
    /// open are the update's staging path's problem, not this one's.
    ///
    /// An empty `warn_at` skips the countdown and stops immediately.
    pub fn graceful_stop(
        &mut self,
        warn_at: &[u64],
        stop_timeout: Duration,
    ) -> io::Result<StopOutcome> {
        let mut remaining = warn_at.to_vec();
        remaining.sort_unstable_by(|a, b| b.cmp(a));

        for pair in remaining.windows(2) {
            let (current, next) = (pair[0], pair[1]);
            self.send_line(&format!("say Server restarting for update in {current}s"))?;
            thread::sleep(Duration::from_secs(current - next));
        }
        if let Some(&last) = remaining.last() {
            self.send_line(&format!("say Server restarting for update in {last}s"))?;
            thread::sleep(Duration::from_secs(last));
        }

        self.send_line("stop")?;

        match self.wait_timeout(stop_timeout)? {
            Some(status) => Ok(StopOutcome::Exited(status)),
            None => {
                log::warn!("server did not stop within {stop_timeout:?} of `stop`; killing it");
                self.kill_and_wait()?;
                Ok(StopOutcome::Killed)
            }
        }
    }
}

fn relay<R: Read>(pipe: R, sink: impl Fn(&str)) {
    for line in BufReader::new(pipe).lines() {
        match line {
            Ok(line) => sink(&line),
            Err(_) => break,
        }
    }
}

fn build_command(config: &ServerConfig) -> Command {
    let mut command = match &config.server_exe {
        Some(exe) => Command::new(exe),
        None => match config.kind {
            ServerKind::Windows | ServerKind::PreviewWindows => Command::new("bedrock_server.exe"),
            ServerKind::Linux | ServerKind::PreviewLinux => {
                let mut c = Command::new("./bedrock_server");
                c.env("LD_LIBRARY_PATH", ".");
                c
            }
            ServerKind::ServerJar => {
                let mut c = Command::new("java");
                c.arg("-jar").arg("server.jar");
                c
            }
        },
    };
    command.args(&config.extra_args);
    command
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_command_prefers_server_exe_override() {
        let config = ServerConfig {
            server_path: PathBuf::from("."),
            kind: ServerKind::Linux,
            server_exe: Some("custom-server".to_string()),
            extra_args: vec!["--foo".to_string()],
        };

        let command = build_command(&config);

        assert_eq!(command.get_program(), "custom-server");
        assert_eq!(command.get_args().collect::<Vec<_>>(), vec!["--foo"]);
    }

    #[test]
    fn build_command_defaults_by_kind() {
        let jar = build_command(&ServerConfig {
            server_path: PathBuf::from("."),
            kind: ServerKind::ServerJar,
            server_exe: None,
            extra_args: vec![],
        });
        assert_eq!(jar.get_program(), "java");
        assert_eq!(
            jar.get_args().collect::<Vec<_>>(),
            vec!["-jar", "server.jar"]
        );

        let windows = build_command(&ServerConfig {
            server_path: PathBuf::from("."),
            kind: ServerKind::Windows,
            server_exe: None,
            extra_args: vec![],
        });
        assert_eq!(windows.get_program(), "bedrock_server.exe");

        let linux = build_command(&ServerConfig {
            server_path: PathBuf::from("."),
            kind: ServerKind::Linux,
            server_exe: None,
            extra_args: vec![],
        });
        assert_eq!(linux.get_program(), "./bedrock_server");
    }
}
