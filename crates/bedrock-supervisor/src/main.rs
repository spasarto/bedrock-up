use bedrock_supervisor::args::SupervisorArgs;
use bedrock_supervisor::supervisor::{self, SupervisorConfig};
use clap::Parser;

fn main() {
    init_logger();

    let args = SupervisorArgs::parse();
    let config = SupervisorConfig::from(&args);

    let exit_code = match supervisor::run(config, |line: &str| println!("{line}")) {
        Ok(()) => 0,
        Err(e) => {
            log::error!("{e}");
            1
        }
    };
    std::process::exit(exit_code);
}

fn init_logger() {
    env_logger::Builder::new()
        .filter_level(log::LevelFilter::Info)
        .init();
}
