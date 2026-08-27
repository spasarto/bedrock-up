mod args;
use args::UpdateArgs;
mod updater;
use clap::{CommandFactory, Parser};
use updater::{UpdateOutcome, update};

const ALREADY_CURRENT: i32 = 0;
const ERROR: i32 = 1;
const UPDATE_APPLIED: i32 = 2;

fn main() {
    let args = UpdateArgs::try_parse();
    let exit_code = match args {
        Ok(args) => match update(args) {
            Ok(UpdateOutcome::Updated) => UPDATE_APPLIED,
            Ok(UpdateOutcome::AlreadyCurrent) => ALREADY_CURRENT,
            Err(e) => {
                eprintln!("Error: {}", e);
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
