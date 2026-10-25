#![forbid(unsafe_code)]
#![deny(warnings, clippy::pedantic, clippy::nursery)]

mod backend;
mod cli;
mod commands;
mod config;
mod discovery;
mod error;
mod git;
mod integrity;
mod paths;
mod prompts;
mod run_dir;
mod workflow;

use std::process::ExitCode;

use clap::Parser;

use crate::cli::Cli;
/// Parses CLI arguments and runs one configured workflow.
pub fn run() -> ExitCode {
    let cli = Cli::parse();
    match workflow::run(&cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("spec-loop: {error}");
            ExitCode::FAILURE
        }
    }
}
