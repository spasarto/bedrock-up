//! Exercises `Server` against `fake-bedrock-server`, the stub that stands in
//! for `bedrock_server` since CI can run neither the Windows nor Linux
//! Mojang binary.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bedrock_supervisor::args::ServerKind;
use bedrock_supervisor::server::{Server, ServerConfig, StopOutcome};

fn fake_server_exe() -> String {
    env!("CARGO_BIN_EXE_fake-bedrock-server").to_string()
}

fn spawn_fake(server_path: &Path, extra_args: Vec<String>) -> (Server, Arc<Mutex<Vec<String>>>) {
    let lines = Arc::new(Mutex::new(Vec::new()));
    let sink = lines.clone();
    let config = ServerConfig {
        server_path: server_path.to_path_buf(),
        kind: ServerKind::Linux,
        server_exe: Some(fake_server_exe()),
        extra_args,
    };

    let server = Server::spawn(&config, move |line: &str| {
        sink.lock().unwrap().push(line.to_string());
    })
    .expect("failed to spawn fake-bedrock-server");

    (server, lines)
}

#[test]
fn spawn_warn_stop_exits_cleanly() {
    let dir = tempfile::TempDir::new().unwrap();
    let (mut server, lines) = spawn_fake(dir.path(), vec![]);

    let outcome = server
        .graceful_stop(&[1], Duration::from_secs(5))
        .expect("graceful_stop failed");

    match outcome {
        StopOutcome::Exited(status) => assert!(status.success()),
        StopOutcome::Killed => panic!("expected the fake server to exit cleanly on `stop`"),
    }

    std::thread::sleep(Duration::from_millis(100));
    let seen = lines.lock().unwrap().join("\n");
    assert!(
        seen.contains("restarting for update in 1s"),
        "expected a warning line, got: {seen}"
    );
    assert!(
        seen.contains("stop"),
        "expected the echoed stop command, got: {seen}"
    );
}

#[test]
fn unresponsive_server_is_killed_after_stop_timeout() {
    let dir = tempfile::TempDir::new().unwrap();
    let (mut server, _lines) = spawn_fake(dir.path(), vec!["--ignore-stop".to_string()]);

    let outcome = server
        .graceful_stop(&[], Duration::from_millis(300))
        .expect("graceful_stop failed");

    assert!(matches!(outcome, StopOutcome::Killed));
}

#[test]
fn console_relay_works_in_both_directions() {
    let dir = tempfile::TempDir::new().unwrap();
    let (mut server, lines) = spawn_fake(dir.path(), vec![]);

    server.send_line("hello supervisor").unwrap();
    std::thread::sleep(Duration::from_millis(200));

    let seen = lines.lock().unwrap().join("\n");
    assert!(
        seen.contains("hello supervisor"),
        "expected our line echoed back through the relay, got: {seen}"
    );

    server
        .graceful_stop(&[], Duration::from_secs(5))
        .expect("graceful_stop failed");
}

#[test]
fn refuses_to_start_when_a_server_is_already_running_in_the_directory() {
    // Spawn a real fake-bedrock-server, then point a second config at the
    // directory its executable actually lives in — the shared build output
    // directory — so the second spawn must see it running there. (The
    // calling test process itself is excluded from this check, so it can't
    // be used to fake an "already running" hit anymore.)
    let dir = tempfile::TempDir::new().unwrap();
    let (mut running, _lines) = spawn_fake(dir.path(), vec![]);

    let exe_dir: PathBuf = Path::new(&fake_server_exe())
        .parent()
        .unwrap()
        .to_path_buf();

    let config = ServerConfig {
        server_path: exe_dir,
        kind: ServerKind::Linux,
        server_exe: Some(fake_server_exe()),
        extra_args: vec![],
    };

    let result = Server::spawn(&config, |_line: &str| {});

    assert!(
        result.is_err(),
        "expected spawn to refuse an already-running directory"
    );

    running
        .graceful_stop(&[], Duration::from_secs(5))
        .expect("graceful_stop failed");
}
