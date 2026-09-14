mod args;

use args::{ApplyArgs, DownloadArgs, UpdateArgs};
use bedrock_up::{CheckOutcome, StagedUpdate, UpdateConfig, UpdateError, UpdateOutcome, check};
use clap::{CommandFactory, Parser};

const ALREADY_CURRENT: i32 = 0;
const ERROR: i32 = 1;
const UPDATE_APPLIED: i32 = 2;

fn main() {
    init_logger();

    // `download` and `apply` are optional standalone subcommands layered on
    // top of the original flat CLI (see docs/supervisor-plan.md, Phase 5);
    // dispatch on the first token by hand rather than folding them into
    // `UpdateArgs` as a `clap` subcommand, so the default no-subcommand
    // invocation — the documented, unchanged contract — keeps parsing
    // exactly as it always has.
    let mut raw = std::env::args();
    raw.next(); // argv[0]; subcommands get a fixed, friendly name instead (see below).
    let exit_code = match raw.next().as_deref() {
        Some("download") => {
            match DownloadArgs::try_parse_from(std::iter::once("bedrock-up download".to_string()).chain(raw)) {
                Ok(args) => run_download(&args),
                Err(e) => {
                    e.print().ok();
                    ERROR
                }
            }
        }
        Some("apply") => {
            match ApplyArgs::try_parse_from(std::iter::once("bedrock-up apply".to_string()).chain(raw)) {
                Ok(args) => run_apply(&args),
                Err(e) => {
                    e.print().ok();
                    ERROR
                }
            }
        }
        _ => match UpdateArgs::try_parse() {
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
        },
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

/// Checks for an update and, if one exists, downloads it to `--stage-path`
/// for a later `apply` — the server is never touched. Exit codes mirror the
/// full pipeline: `0` nothing new, `2` staged, `1` error.
fn run_download(args: &DownloadArgs) -> i32 {
    let config = UpdateConfig::from(args);

    let update = match check(&config) {
        Ok(CheckOutcome::UpToDate { .. }) => return ALREADY_CURRENT,
        Ok(CheckOutcome::UpdateAvailable(update)) => update,
        Err(e) => {
            log::error!("Error: {e}");
            return ERROR;
        }
    };

    let version = update.version().to_string();
    let staged = match update.download() {
        Ok(staged) => staged,
        Err(e) => {
            log::error!("Error: {e}");
            return ERROR;
        }
    };

    if let Err(e) = staged.persist(&args.stage_path()) {
        log::error!("Error: {e}");
        return ERROR;
    }

    log::info!(
        "Staged version {version} at {}; run `bedrock-up apply` to install it.",
        args.stage_path().display()
    );
    UPDATE_APPLIED
}

/// Applies whatever `bedrock-up download` staged at `--stage-path`.
/// Filesystem only and fast; stop the server first. Exit codes: `0` nothing
/// staged, `2` applied, `1` error.
fn run_apply(args: &ApplyArgs) -> i32 {
    let staged = match StagedUpdate::load(&args.stage_path()) {
        Ok(Some(staged)) => staged,
        Ok(None) => {
            log::info!("No staged update at {}.", args.stage_path().display());
            return ALREADY_CURRENT;
        }
        Err(e) => {
            log::error!("Error: {e}");
            return ERROR;
        }
    };

    match staged.apply(&UpdateConfig::from(args)) {
        Ok(UpdateOutcome::Updated | UpdateOutcome::UpdatedPendingRestart) => UPDATE_APPLIED,
        Err(e) => {
            log::error!("Error: {e}");
            ERROR
        }
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
