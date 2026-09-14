//! A local control socket so `bedrock-supervisor trigger-update` can nudge a
//! running supervisor into an on-demand update check without touching its
//! stdin, which is reserved for relaying commands to the child's console.
//!
//! Named pipe on Windows, Unix domain socket on Linux — `interprocess`'s
//! [`GenericNamespaced`] local-socket name abstracts the two behind one API,
//! so there is no platform-specific code here and, unlike a filesystem-path
//! socket, nothing left behind to clean up if the process dies uncleanly.
//!
//! The socket carries a tiny line-based protocol: one command line in, one
//! response line out, then the connection closes. Nothing here needs more
//! than that.

use std::hash::{Hash, Hasher};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::Path;
use std::sync::mpsc::Sender;
use std::thread;

use interprocess::local_socket::prelude::*;
use interprocess::local_socket::{GenericNamespaced, ListenerOptions, Name};

use crate::supervisor::Event;

#[derive(Debug, thiserror::Error)]
pub enum ControlError {
    #[error("failed to determine the control socket name for {}: {source}", path.display())]
    Name { path: std::path::PathBuf, source: io::Error },

    #[error("failed to connect to the control socket: {0}")]
    Connect(#[source] io::Error),

    #[error("I/O error talking to the control socket: {0}")]
    Io(#[from] io::Error),

    #[error("the supervisor closed the connection without responding")]
    NoResponse,
}

/// Derives a socket name unique to `server_path`, so multiple supervisors —
/// one per server directory, per the plan's rule against sharing one world
/// between two instances — never collide, and `trigger-update` finds the
/// right one just by being pointed at the same `--server-path` the running
/// supervisor was started with.
fn socket_name(server_path: &Path) -> Result<Name<'static>, ControlError> {
    let canonical = std::fs::canonicalize(server_path).map_err(|source| ControlError::Name {
        path: server_path.to_path_buf(),
        source,
    })?;

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    canonical.hash(&mut hasher);
    let name = format!("bedrock-supervisor-{:x}", hasher.finish());

    name.to_ns_name::<GenericNamespaced>()
        .map_err(|source| ControlError::Name {
            path: server_path.to_path_buf(),
            source,
        })
}

/// Binds the control socket and spawns a thread to service it, forwarding
/// recognized commands to the run loop as [`Event`]s over `tx`.
///
/// Failing to bind is logged and otherwise ignored rather than treated as
/// fatal — a supervisor that can't offer on-demand triggers should still
/// babysit the server and check for updates on its normal schedule.
pub fn spawn_listener(server_path: &Path, tx: Sender<Event>) {
    let name = match socket_name(server_path) {
        Ok(name) => name,
        Err(e) => {
            log::warn!("control socket disabled: {e}");
            return;
        }
    };

    let listener = match ListenerOptions::new().name(name).create_sync() {
        Ok(listener) => listener,
        Err(e) => {
            log::warn!("control socket disabled: failed to bind: {e}");
            return;
        }
    };

    log::info!("control socket ready; `bedrock-supervisor trigger-update` can reach it");

    thread::spawn(move || {
        for connection in listener.incoming() {
            match connection {
                Ok(conn) => handle_connection(conn, &tx),
                Err(e) => log::warn!("control socket: failed to accept a connection: {e}"),
            }
        }
    });
}

/// Reads one command line, acts on it, and writes back one response line.
fn handle_connection(conn: impl Read + Write, tx: &Sender<Event>) {
    let mut reader = BufReader::new(conn);
    let mut line = String::new();
    if let Err(e) = reader.read_line(&mut line) {
        log::warn!("control socket: failed to read a command: {e}");
        return;
    }

    let response: &str = match line.trim() {
        "trigger-update" => {
            if tx.send(Event::CheckUpdate).is_ok() {
                log::info!("update check triggered via the control socket");
                "ok\n"
            } else {
                "error: the supervisor run loop is gone\n"
            }
        }
        other => {
            log::warn!("control socket: unrecognized command {other:?}");
            "error: unrecognized command\n"
        }
    };

    let mut conn = reader.into_inner();
    if let Err(e) = conn.write_all(response.as_bytes()) {
        log::warn!("control socket: failed to write a response: {e}");
    }
}

/// Connects to a running supervisor's control socket for `server_path`,
/// sends `command`, and returns its one-line response with the trailing
/// newline trimmed.
pub fn send_command(server_path: &Path, command: &str) -> Result<String, ControlError> {
    let name = socket_name(server_path)?;
    let mut conn = LocalSocketStream::connect(name).map_err(ControlError::Connect)?;
    writeln!(conn, "{command}")?;

    let mut reader = BufReader::new(conn);
    let mut response = String::new();
    reader.read_line(&mut response)?;
    if response.is_empty() {
        return Err(ControlError::NoResponse);
    }

    Ok(response.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn trigger_update_reaches_the_run_loop() {
        let dir = tempfile::TempDir::new().unwrap();
        let (tx, rx) = mpsc::channel();
        spawn_listener(dir.path(), tx);

        // The listener thread binds asynchronously; give it a moment.
        std::thread::sleep(Duration::from_millis(100));

        let response = send_command(dir.path(), "trigger-update").unwrap();
        assert_eq!(response, "ok");

        let event = rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(matches!(event, Event::CheckUpdate));
    }

    #[test]
    fn unrecognized_command_gets_an_error_response() {
        let dir = tempfile::TempDir::new().unwrap();
        let (tx, _rx) = mpsc::channel();
        spawn_listener(dir.path(), tx);
        std::thread::sleep(Duration::from_millis(100));

        let response = send_command(dir.path(), "do-a-barrel-roll").unwrap();
        assert!(response.starts_with("error"));
    }

    #[test]
    fn send_command_fails_when_nothing_is_listening() {
        let dir = tempfile::TempDir::new().unwrap();

        let result = send_command(dir.path(), "trigger-update");

        assert!(matches!(result, Err(ControlError::Connect(_))));
    }

    #[test]
    fn socket_name_is_stable_for_the_same_path() {
        let dir = tempfile::TempDir::new().unwrap();

        let a = socket_name(dir.path()).unwrap();
        let b = socket_name(dir.path()).unwrap();

        assert_eq!(a, b);
    }
}
