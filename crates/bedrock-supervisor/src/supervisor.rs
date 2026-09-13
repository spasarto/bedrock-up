//! The supervisor run loop: spawns and babysits the server, relays its
//! console, checks for updates on a timer, and coordinates the graceful
//! stop/apply/restart cycle when one is found.
//!
//! Threads, not async: `reqwest::blocking` (via `bedrock_up::check`) is
//! already the shape everything here needs, and async would only be
//! coordination tax. Producer threads (stdin, the update-check ticker, the
//! signal handler) feed a single `mpsc::Receiver<Event>` that [`drive`]
//! drains; child-exit detection is a poll on the receive timeout rather than
//! a fourth thread, since `Child::try_wait` already needs the same `&mut
//! Server` the rest of the loop uses.
//!
//! Console output is a pure relay — nothing here parses it. No decision this
//! loop makes depends on the contents of a console line.

use std::io::{self, BufRead, IsTerminal};
use std::process::ExitStatus;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

use bedrock_up::{CheckOutcome, UpdateConfig, UpdateError, UpdateOutcome, check};

use crate::server::{Server, ServerConfig, ServerError, StopOutcome};

/// How often the run loop wakes up to poll for child exit when nothing else
/// is coming in on the channel.
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// How long a restarted server must stay up before a subsequent crash is
/// treated as a fresh failure streak rather than a continuation of the last
/// one.
const STABLE_UPTIME: Duration = Duration::from_secs(60);

/// Consecutive crashes (within [`STABLE_UPTIME`] of each other) before the
/// supervisor gives up restarting and exits instead of spinning forever.
const MAX_CONSECUTIVE_FAILURES: u32 = 6;

#[derive(Debug, thiserror::Error)]
pub enum SupervisorError {
    #[error(transparent)]
    Server(#[from] ServerError),

    #[error("server crashed {attempts} times in a row; giving up")]
    GaveUp { attempts: u32 },
}

/// Everything the run loop needs: how to launch the server, how to check for
/// and apply updates, and the policy knobs around both.
pub struct SupervisorConfig {
    pub server: ServerConfig,
    pub update: UpdateConfig,
    /// Countdown warnings before `stop`, most distant first. Empty stops
    /// immediately.
    pub warn_at: Vec<u64>,
    pub stop_timeout: Duration,
    /// How often to check for updates. `Duration::ZERO` disables the ticker
    /// entirely, leaving a supervisor that only babysits and restarts.
    pub check_interval: Duration,
    /// Check for and apply an update before the first start, instead of
    /// waiting for the first tick.
    pub update_on_start: bool,
}

/// Events driving [`drive`]. `ChildExited` is synthesized locally from a
/// poll rather than sent by a dedicated thread — see the module docs.
pub enum Event {
    /// A line read from our own stdin, to be relayed to the child's.
    Command(String),
    ChildExited(ExitStatus),
    CheckUpdate,
    Shutdown,
}

/// Starts the server, wires up the producer threads (stdin relay, the
/// update-check ticker, the shutdown signal handler), and runs until a clean
/// shutdown or an unrecoverable crash loop.
///
/// `on_console_line` receives every line the server writes to stdout or
/// stderr; the caller decides where it goes (stdout, a log file, ...).
pub fn run(
    config: SupervisorConfig,
    on_console_line: impl Fn(&str) + Clone + Send + 'static,
) -> Result<(), SupervisorError> {
    if config.update_on_start
        && let Err(e) = update_before_start(&config.update)
    {
        log::warn!("update-on-start check failed; starting on the existing build: {e}");
    }

    let server = Server::spawn(&config.server, on_console_line.clone())?;
    log::info!("server started from {}", config.server.server_path.display());

    let (tx, rx) = mpsc::channel();
    spawn_stdin_relay(tx.clone());
    spawn_ticker(tx.clone(), config.check_interval);
    install_signal_handler(tx);

    drive(server, rx, &config, on_console_line)
}

/// The event loop itself, separated from [`run`] so tests can drive it with
/// a synthetic channel instead of real threads and signals.
pub fn drive(
    mut server: Server,
    rx: Receiver<Event>,
    config: &SupervisorConfig,
    on_console_line: impl Fn(&str) + Clone + Send + 'static,
) -> Result<(), SupervisorError> {
    let mut backoff = Backoff::new();
    let mut last_spawn = Instant::now();

    loop {
        let event = match rx.recv_timeout(POLL_INTERVAL) {
            Ok(event) => event,
            Err(mpsc::RecvTimeoutError::Timeout) => match server.try_wait() {
                Ok(Some(status)) => Event::ChildExited(status),
                Ok(None) => continue,
                Err(e) => {
                    log::error!("error polling the server process: {e}");
                    continue;
                }
            },
            // All senders dropped; nothing left to drive the loop.
            Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
        };

        match event {
            Event::Command(line) => {
                if let Err(e) = server.send_line(&line) {
                    log::error!("failed to send command to the server: {e}");
                }
            }
            Event::ChildExited(status) => {
                log::warn!("server exited unexpectedly: {status}");

                if last_spawn.elapsed() > STABLE_UPTIME {
                    backoff.reset();
                }
                let delay = match backoff.next_delay() {
                    Some(delay) => delay,
                    None => {
                        return Err(SupervisorError::GaveUp {
                            attempts: backoff.attempts(),
                        });
                    }
                };

                log::info!("restarting the server in {delay:?}");
                thread::sleep(delay);
                server = Server::spawn(&config.server, on_console_line.clone())?;
                last_spawn = Instant::now();
            }
            Event::CheckUpdate => {
                if apply_update_cycle(&mut server, config, on_console_line.clone())? {
                    last_spawn = Instant::now();
                }
            }
            Event::Shutdown => {
                log::info!("shutdown requested, stopping the server gracefully");
                match server.graceful_stop(&config.warn_at, config.stop_timeout) {
                    Ok(StopOutcome::Exited(status)) => log::info!("server exited: {status}"),
                    Ok(StopOutcome::Killed) => {
                        log::warn!("server did not stop in time and was killed")
                    }
                    Err(e) => log::error!("error stopping the server: {e}"),
                }
                return Ok(());
            }
        }
    }
}

/// Runs one check → download → warn/stop → apply → restart cycle. A failed
/// check or download is logged and left for the next tick; the server is
/// never taken down for a check that failed. Returns whether the server was
/// restarted, so the caller can reset its uptime clock.
fn apply_update_cycle(
    server: &mut Server,
    config: &SupervisorConfig,
    on_console_line: impl Fn(&str) + Clone + Send + 'static,
) -> Result<bool, SupervisorError> {
    let update = match check(&config.update) {
        Ok(CheckOutcome::UpToDate { version }) => {
            log::info!("already on the latest version: {version}");
            return Ok(false);
        }
        Ok(CheckOutcome::UpdateAvailable(update)) => update,
        Err(e) => {
            log::warn!("update check failed, will retry next tick: {e}");
            return Ok(false);
        }
    };

    log::info!("update available: {}; downloading", update.version());
    let staged = match update.download() {
        Ok(staged) => staged,
        Err(e) => {
            log::warn!("update download failed, will retry next tick: {e}");
            return Ok(false);
        }
    };

    log::info!("stopping the server to apply the update");
    match server.graceful_stop(&config.warn_at, config.stop_timeout) {
        Ok(StopOutcome::Exited(status)) => log::info!("server exited for update: {status}"),
        Ok(StopOutcome::Killed) => {
            log::warn!("server did not stop in time for the update and was killed")
        }
        Err(e) => log::error!("error stopping the server for the update: {e}"),
    }

    // The server is down either way now, so the update is applied and the
    // server restarted regardless of how the stop went; `apply_update`'s
    // in-use staging covers files a killed process still held open.
    match staged.apply(&config.update) {
        Ok(UpdateOutcome::Updated) => log::info!("update applied"),
        Ok(UpdateOutcome::UpdatedPendingRestart) => {
            log::info!("update applied; the restart picks it up")
        }
        Err(e) => log::error!("failed to apply the staged update: {e}"),
    }

    *server = Server::spawn(&config.server, on_console_line)?;
    log::info!("server restarted after the update check");
    Ok(true)
}

/// Checks for and applies an update before the server has ever started, so
/// there is no countdown, no graceful stop, and no in-use staging to worry
/// about. A failed check never blocks startup — log it and start on
/// whatever build is on disk.
fn update_before_start(config: &UpdateConfig) -> Result<(), UpdateError> {
    let update = match check(config)? {
        CheckOutcome::UpToDate { version } => {
            log::info!("already on the latest version: {version}");
            return Ok(());
        }
        CheckOutcome::UpdateAvailable(update) => update,
    };

    log::info!("update available: {}; applying before start", update.version());
    let staged = update.download()?;
    match staged.apply(config)? {
        UpdateOutcome::Updated | UpdateOutcome::UpdatedPendingRestart => {
            log::info!("update applied before start");
        }
    }
    Ok(())
}

/// Relays lines from our stdin to the child's, so the operator console still
/// works when run interactively. Skipped entirely when stdin is not a TTY,
/// as under a service manager, rather than spinning on EOF.
fn spawn_stdin_relay(tx: Sender<Event>) {
    if !io::stdin().is_terminal() {
        log::info!("stdin is not a terminal; the interactive console is disabled");
        return;
    }

    thread::spawn(move || {
        let stdin = io::stdin();
        for line in stdin.lock().lines() {
            let Ok(line) = line else { break };
            if tx.send(Event::Command(line)).is_err() {
                break;
            }
        }
    });
}

/// Fires `Event::CheckUpdate` on a fixed interval. A zero interval disables
/// the ticker, leaving a supervisor that only babysits and restarts.
fn spawn_ticker(tx: Sender<Event>, interval: Duration) {
    if interval.is_zero() {
        log::info!("automatic update checks are disabled (--check-interval 0)");
        return;
    }

    thread::spawn(move || {
        loop {
            thread::sleep(interval);
            if tx.send(Event::CheckUpdate).is_err() {
                break;
            }
        }
    });
}

/// Ctrl+C (and, on Unix, SIGTERM) must mean *stop the child gracefully, then
/// exit* — never instant death, which would drop the world without saving.
fn install_signal_handler(tx: Sender<Event>) {
    if let Err(e) = ctrlc::set_handler(move || {
        let _ = tx.send(Event::Shutdown);
    }) {
        log::error!("failed to install the shutdown signal handler: {e}");
    }
}

/// Exponential backoff for unexpected child exits, with a give-up threshold
/// so a server that crashes on every launch doesn't spin forever.
struct Backoff {
    attempts: u32,
}

impl Backoff {
    fn new() -> Self {
        Backoff { attempts: 0 }
    }

    /// Records a crash and returns how long to wait before restarting, or
    /// `None` once [`MAX_CONSECUTIVE_FAILURES`] is exceeded.
    fn next_delay(&mut self) -> Option<Duration> {
        self.attempts += 1;
        if self.attempts > MAX_CONSECUTIVE_FAILURES {
            return None;
        }
        let shift = (self.attempts - 1).min(6);
        Some(Duration::from_secs((1u64 << shift).min(60)))
    }

    fn reset(&mut self) {
        self.attempts = 0;
    }

    fn attempts(&self) -> u32 {
        self.attempts
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_up_to_a_cap() {
        let mut backoff = Backoff::new();
        assert_eq!(backoff.next_delay(), Some(Duration::from_secs(1)));
        assert_eq!(backoff.next_delay(), Some(Duration::from_secs(2)));
        assert_eq!(backoff.next_delay(), Some(Duration::from_secs(4)));
        assert_eq!(backoff.next_delay(), Some(Duration::from_secs(8)));
        assert_eq!(backoff.next_delay(), Some(Duration::from_secs(16)));
        assert_eq!(backoff.next_delay(), Some(Duration::from_secs(32)));
    }

    #[test]
    fn backoff_gives_up_after_max_consecutive_failures() {
        let mut backoff = Backoff::new();
        for _ in 0..MAX_CONSECUTIVE_FAILURES {
            assert!(backoff.next_delay().is_some());
        }
        assert_eq!(backoff.next_delay(), None);
        assert_eq!(backoff.attempts(), MAX_CONSECUTIVE_FAILURES + 1);
    }

    #[test]
    fn backoff_reset_starts_the_sequence_over() {
        let mut backoff = Backoff::new();
        backoff.next_delay();
        backoff.next_delay();
        backoff.reset();
        assert_eq!(backoff.next_delay(), Some(Duration::from_secs(1)));
    }
}
