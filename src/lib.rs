#![forbid(unsafe_code)]
#![deny(warnings, clippy::pedantic, clippy::nursery)]

mod auth;
mod backend;
mod cli;
mod commands;
mod config;
mod discovery;
mod error;
mod integrity;
mod paths;
mod prompts;
mod protocol;
mod run_dir;
mod runtime;
mod workflow;

use std::process::ExitCode;

use clap::Parser;

use crate::cli::{AuthSubcommand, Cli, CliCommand};
/// Parses CLI arguments and runs one configured workflow or auth command.
pub fn run() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Some(CliCommand::Auth(arguments)) => {
            let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let cancellation_flag = std::sync::Arc::clone(&cancelled);
            let handler = ctrlc::set_handler(move || {
                cancellation_flag.store(true, std::sync::atomic::Ordering::Relaxed);
            });
            match handler {
                Ok(()) => {
                    let command = match arguments.command {
                        AuthSubcommand::Login {
                            account,
                            new_account,
                        } => auth::AuthCommand::Login {
                            account,
                            new_account,
                        },
                        AuthSubcommand::Status => auth::AuthCommand::Status,
                        AuthSubcommand::Logout { account } => auth::AuthCommand::Logout { account },
                    };
                    auth::run(command, &cancelled)
                }
                Err(error) => Err(error::SpecLoopError::Process(format!(
                    "install interrupt handler: {error}"
                ))),
            }
        }
        None => cli.validate().and_then(|()| workflow::run(&cli)),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("spec-loop: {error}");
            ExitCode::FAILURE
        }
    }
}
