use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use bedrock_supervisor::args::SupervisorArgs;
use bedrock_supervisor::server::{Server, ServerConfig, StopOutcome};
use clap::Parser;

fn main() {
    init_logger();

    let args = SupervisorArgs::parse();
    let config = ServerConfig::from(&args);

    let mut server = match Server::spawn(&config, |line| println!("{line}")) {
        Ok(server) => server,
        Err(e) => {
            log::error!("failed to start the server: {e}");
            std::process::exit(1);
        }
    };
    log::info!("server started from {}", config.server_path.display());

    let stop_requested = install_ctrlc_handler();

    loop {
        if stop_requested.load(Ordering::SeqCst) {
            log::info!("shutdown requested, stopping the server gracefully");
            match server.graceful_stop(&args.warn_at_seconds(), args.stop_timeout()) {
                Ok(StopOutcome::Exited(status)) => log::info!("server exited: {status}"),
                Ok(StopOutcome::Killed) => {
                    log::warn!("server did not stop in time and was killed")
                }
                Err(e) => log::error!("error stopping the server: {e}"),
            }
            break;
        }

        match server.try_wait() {
            Ok(Some(status)) => {
                log::info!("server exited on its own: {status}");
                break;
            }
            Ok(None) => {}
            Err(e) => {
                log::error!("error polling the server process: {e}");
                break;
            }
        }

        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Ctrl+C must mean *stop the child gracefully, then exit* — never instant
/// death, which would drop the world without saving.
fn install_ctrlc_handler() -> Arc<AtomicBool> {
    let flag = Arc::new(AtomicBool::new(false));
    let handler_flag = flag.clone();
    ctrlc::set_handler(move || handler_flag.store(true, Ordering::SeqCst))
        .expect("failed to install the Ctrl+C handler");
    flag
}

fn init_logger() {
    env_logger::Builder::new()
        .filter_level(log::LevelFilter::Info)
        .init();
}
