use std::path::PathBuf;

use bedrock_supervisor::args::{SupervisorArgs, TriggerUpdateArgs};
use bedrock_supervisor::control;
use bedrock_supervisor::supervisor::{self, SupervisorConfig};
use clap::Parser;

fn main() {
    init_logger();

    // `trigger-update` is a lightweight client, not a supervisor of its own;
    // dispatch on the first token by hand rather than a `clap` subcommand, so
    // the default no-subcommand invocation keeps using `SupervisorArgs::parse()`
    // exactly as before (including its own help/error/exit-code handling).
    let mut raw = std::env::args();
    raw.next(); // argv[0]
    let is_trigger_update = raw.next().as_deref() == Some("trigger-update");

    let exit_code = if is_trigger_update {
        match TriggerUpdateArgs::try_parse_from(
            std::iter::once("bedrock-supervisor trigger-update".to_string()).chain(raw),
        ) {
            Ok(args) => run_trigger_update(&args),
            Err(e) => {
                e.print().ok();
                1
            }
        }
    } else {
        let args = SupervisorArgs::parse();
        let config = SupervisorConfig::from(&args);

        match supervisor::run(config, |line: &str| println!("{line}")) {
            Ok(()) => 0,
            Err(e) => {
                log::error!("{e}");
                1
            }
        }
    };
    std::process::exit(exit_code);
}

/// Connects to a running supervisor's control socket and asks it to check
/// for an update now, printing its response.
fn run_trigger_update(args: &TriggerUpdateArgs) -> i32 {
    let server_path = PathBuf::from(shellexpand::tilde(&args.server_path).to_string());

    match control::send_command(&server_path, "trigger-update") {
        Ok(response) => {
            let ok = response.starts_with("ok");
            println!("{response}");
            if ok { 0 } else { 1 }
        }
        Err(e) => {
            eprintln!("Error: {e}");
            1
        }
    }
}

fn init_logger() {
    env_logger::Builder::new()
        .filter_level(log::LevelFilter::Info)
        .init();
}
