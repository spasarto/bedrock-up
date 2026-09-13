mod args;

use args::UpdateArgs;
use bedrock_up::{CheckOutcome, UpdateConfig, UpdateError, UpdateOutcome, check};
use clap::{CommandFactory, Parser};

const ALREADY_CURRENT: i32 = 0;
const ERROR: i32 = 1;
const UPDATE_APPLIED: i32 = 2;

fn main() {
    init_logger();

    let args = UpdateArgs::try_parse();
    let exit_code = match args {
        Ok(args) => match run(&args.into()) {
            Ok(true) => UPDATE_APPLIED,
            Ok(false) => ALREADY_CURRENT,
            Err(e) => {
                log::error!("Error: {}", e);
                ERROR
            }
        },
        Err(_) => {
            UpdateArgs::command().print_help().unwrap();
            ERROR
        }
    };
    std::process::exit(exit_code);
}

/// Runs the full check -> download -> apply pipeline. Returns whether new
/// files landed on disk (`true`) or the server was already current (`false`).
fn run(config: &UpdateConfig) -> Result<bool, UpdateError> {
    let update = match check(config)? {
        CheckOutcome::UpToDate { .. } => return Ok(false),
        CheckOutcome::UpdateAvailable(update) => update,
    };

    let staged = update.download()?;
    match staged.apply(config)? {
        UpdateOutcome::Updated | UpdateOutcome::UpdatedPendingRestart => Ok(true),
    }
}

/// `env_logger` normally prefixes lines with a timestamp and level, but the
/// CLI's documented output is bare text on stdout — match that as closely as
/// practical while still routing the library's `log` calls somewhere.
fn init_logger() {
    use std::io::Write;

    env_logger::Builder::new()
        .filter_level(log::LevelFilter::Info)
        .format(|buf, record| writeln!(buf, "{}", record.args()))
        .target(env_logger::Target::Stdout)
        .init();
}
