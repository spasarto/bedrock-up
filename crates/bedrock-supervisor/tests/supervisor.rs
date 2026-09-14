//! Exercises the `drive` event loop against `fake-bedrock-server`, covering
//! what `tests/server.rs` doesn't: crash detection, backoff, restart, and
//! shutdown as seen through the loop itself rather than `Server` directly.

use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use bedrock_supervisor::args::ServerKind;
use bedrock_supervisor::server::{Server, ServerConfig};
use bedrock_supervisor::supervisor::{Event, SupervisorConfig, drive};
use bedrock_up::{DownloadType, UpdateConfig};

fn fake_server_exe() -> String {
    env!("CARGO_BIN_EXE_fake-bedrock-server").to_string()
}

/// `check`/`download`/`apply` are never exercised by these tests (the ticker
/// is left disabled), so the values here are placeholders.
fn dummy_update_config() -> UpdateConfig {
    UpdateConfig {
        download_type: DownloadType::Linux,
        server_path: PathBuf::from("."),
        cache_path: PathBuf::from("does-not-matter.json"),
        exclude: vec![],
        force: false,
    }
}

#[test]
fn crash_triggers_a_restart_and_shutdown_still_exits_cleanly() {
    let dir = tempfile::TempDir::new().unwrap();
    let config = SupervisorConfig {
        server: ServerConfig {
            server_path: dir.path().to_path_buf(),
            kind: ServerKind::Linux,
            server_exe: Some(fake_server_exe()),
            extra_args: vec!["--crash-after-ms=300".to_string()],
        },
        update: dummy_update_config(),
        warn_at: vec![],
        stop_timeout: Duration::from_secs(5),
        check_interval: Duration::ZERO,
        update_on_start: false,
    };

    let lines = Arc::new(Mutex::new(Vec::new()));
    let sink = lines.clone();
    let on_line = move |line: &str| sink.lock().unwrap().push(line.to_string());

    let server = Server::spawn(&config.server, on_line.clone()).expect("failed to spawn");

    let (tx, rx) = mpsc::channel();
    let shutdown_tx = tx.clone();
    thread::spawn(move || {
        // Long enough to observe the first crash (~300ms) and the restart
        // that follows after the first backoff delay (1s), but short enough
        // to land before the restarted server's own crash timer fires.
        thread::sleep(Duration::from_millis(1500));
        shutdown_tx.send(Event::Shutdown).ok();
    });

    let result = drive(server, rx, &config, on_line);
    assert!(result.is_ok(), "drive returned an error: {result:?}");

    let banners = lines
        .lock()
        .unwrap()
        .iter()
        .filter(|line| line.contains("Starting Server"))
        .count();
    assert!(
        banners >= 2,
        "expected at least one restart after the crash, saw {banners} startup banner(s)"
    );
}

#[test]
fn shutdown_stops_a_healthy_server_cleanly() {
    let dir = tempfile::TempDir::new().unwrap();
    let config = SupervisorConfig {
        server: ServerConfig {
            server_path: dir.path().to_path_buf(),
            kind: ServerKind::Linux,
            server_exe: Some(fake_server_exe()),
            extra_args: vec![],
        },
        update: dummy_update_config(),
        warn_at: vec![],
        stop_timeout: Duration::from_secs(5),
        check_interval: Duration::ZERO,
        update_on_start: false,
    };

    let server = Server::spawn(&config.server, |_line: &str| {}).expect("failed to spawn");

    let (tx, rx) = mpsc::channel();
    tx.send(Event::Shutdown).unwrap();

    let result = drive(server, rx, &config, |_line: &str| {});
    assert!(result.is_ok(), "drive returned an error: {result:?}");
}
