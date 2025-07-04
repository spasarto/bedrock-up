mod args;
use args::UpdateArgs;
mod updater;
use clap::{CommandFactory, Parser};
use updater::update;

fn main() {
    let args = UpdateArgs::try_parse();
    match args {
        Ok(args) => {
            update(args);
        }
        Err(_) => {
            UpdateArgs::command().print_help().unwrap();
        }
    }
}
