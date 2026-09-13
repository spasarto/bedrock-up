//! Stand-in for `bedrock_server` in tests. CI can run neither Windows nor
//! Linux Mojang binaries, so this stub gives the fake-server suite something
//! real to spawn: it prints a startup banner, echoes every stdin line back
//! to stdout (so console relay is testable in both directions), and exits 0
//! on `stop` — unless `--ignore-stop` is passed, to exercise the
//! stop-timeout-then-kill path.

use std::io::{self, BufRead, Write};

fn main() {
    let ignore_stop = std::env::args().any(|arg| arg == "--ignore-stop");

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
