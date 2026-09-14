//! Stand-in for `bedrock_server` in tests. CI can run neither Windows nor
//! Linux Mojang binaries, so this stub gives the fake-server suite something
//! real to spawn: it prints a startup banner, echoes every stdin line back
//! to stdout (so console relay is testable in both directions), and exits 0
//! on `stop` — unless `--ignore-stop` is passed, to exercise the
//! stop-timeout-then-kill path. `--crash-after-ms=<N>` exits(1) on its own
//! after N milliseconds, to exercise the supervisor's crash/backoff/restart
//! path without waiting on `stop` at all.

use std::io::{self, BufRead, Write};
use std::time::Duration;

fn main() {
    let ignore_stop = std::env::args().any(|arg| arg == "--ignore-stop");
    let crash_after_ms = std::env::args().find_map(|arg| {
        arg.strip_prefix("--crash-after-ms=")
            .and_then(|ms| ms.parse::<u64>().ok())
    });

    if let Some(ms) = crash_after_ms {
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(ms));
            std::process::exit(1);
        });
    }

    println!("[INFO] Starting Server");
    println!("[INFO] Version 1.20.0");
    io::stdout().flush().ok();

    let stdin = io::stdin();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        println!("{line}");
        io::stdout().flush().ok();

        if line.trim() == "stop" && !ignore_stop {
            println!("[INFO] Server stopped.");
            io::stdout().flush().ok();
            return;
        }
    }
}
