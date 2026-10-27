use clap::{Args, Parser, Subcommand};

use crate::output::ColorMode;

#[cfg(test)]
mod tests;

/// Command-line options for the generic specification workflow.
#[derive(Clone, Debug, Parser)]
#[command(
    name = "spec-loop",
    version,
    about = "Implement repository specs through a guarded agent loop",
    long_about = "Run the workflow from the current directory, which must contain `.spec-loop.toml`. Specs are discovered from configured filesystem globs; Git is not required. Schema v3 defaults to the built-in OpenAI Responses backend, which requires `agent.model` and `spec-loop auth login`; it uses ChatGPT OAuth and one automatic POSIX shell tool. Set `agent.backend = \"pi\"` to use Pi and preserve its effective resources. Repository instructions and selected specification remain authoritative.",
    after_help = "Workflow role results are readable validated summaries: implementation shows status and summary; plan author shows readiness; reviews show findings; audits show classifications; clarifications show citations. Text previews are limited to 160 Unicode scalar values. `--verbose` shows every validated typed field without truncation, never raw JSON or provider transcripts.\n\n`--color <auto|always|never>` defaults to `auto` and styles stdout and stderr independently only when each stream is a terminal, `TERM` is not `dumb`, and `NO_COLOR` is absent or empty. `always` forces styling; `never` disables it. Explicit status labels remain without color. Both options apply only to workflow invocations, not auth.\n\nGeneric role retries and process timeouts are unlimited by default; `completion.max_passes` configures the generic role retry cap, while built-in provider retries are limited to three per request. Built-in shell runs automatically with same-user permissions and is not sandboxed; commands may access or transmit available credentials and secrets. Checks and lifecycle actions use argv arrays, never a shell. Review-context commands run through selected-backend tools; default is `git diff`. Run logs remain outside the project directory and may contain sensitive shell output."
)]
pub struct Cli {
    /// Run profile-independent authentication commands.
    #[command(subcommand)]
    pub command: Option<CliCommand>,

    /// Select one eligible specification identifier.
    #[arg(long, value_name = "ID")]
    pub spec: Option<String>,

    #[command(flatten)]
    mode: ModeOptions,

    #[command(flatten)]
    execution: ExecutionOptions,

    #[command(flatten)]
    completion: CompletionOptions,

    /// Show every validated typed role-result field, without truncation or raw JSON.
    #[arg(long)]
    verbose: bool,

    /// Control workflow terminal styling (default: auto).
    #[arg(long, value_enum, value_name = "WHEN")]
    color: Option<ColorMode>,
}

#[derive(Args, Clone, Debug, Default)]
struct ModeOptions {
    /// Run checks and review against current workspace without implementation first.
    #[arg(long, conflicts_with = "plan")]
    review: bool,

    /// Author and review a plan only.
    #[arg(long, conflicts_with = "review")]
    plan: bool,
}

#[derive(Args, Clone, Debug, Default)]
struct ExecutionOptions {
    /// Print resolved work without invoking agents or commands.
    #[arg(long)]
    dry_run: bool,
}

#[derive(Args, Clone, Debug, Default)]
struct CompletionOptions {
    /// Continue to later eligible specifications after this one completes.
    #[arg(long = "continue")]
    continue_run: bool,
}

#[derive(Clone, Debug, Subcommand)]
pub enum CliCommand {
    /// Manage `OpenAI` `ChatGPT` authorization without loading a project profile.
    Auth(AuthArgs),
}

#[derive(Args, Clone, Debug)]
pub struct AuthArgs {
    #[command(subcommand)]
    pub command: AuthSubcommand,
}

#[derive(Clone, Debug, Subcommand)]
pub enum AuthSubcommand {
    /// Authorize Spec Loop with `ChatGPT` in a browser.
    Login {
        /// Reauthorize a saved registration ID.
        #[arg(long, conflicts_with = "new_account", value_name = "REGISTRATION_ID")]
        account: Option<String>,
        /// Start a distinct dynamic client registration.
        #[arg(long, conflicts_with = "account")]
        new_account: bool,
    },
    /// Show local authorization and direct-use permission state.
    Status,
    /// Remove selected or active Spec Loop credentials.
    Logout {
        /// Opaque registration ID to remove locally.
        #[arg(long, value_name = "REGISTRATION_ID")]
        account: Option<String>,
    },
}

/// Mutually exclusive workflow operation selected by CLI options.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkflowMode {
    /// Run implementation and its configured review workflow.
    Implement,
    /// Review the selected specification against the current workspace.
    Review,
    /// Author and review a plan for the selected specification.
    Plan,
}

impl WorkflowMode {
    /// Returns the user-facing label used in dry-run output.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Implement => "implementation",
            Self::Review => "review",
            Self::Plan => "plan",
        }
    }
}

impl Cli {
    pub fn validate(&self) -> crate::error::Result<()> {
        if !matches!(self.workflow_mode()?, WorkflowMode::Implement) && self.spec.is_none() {
            return Err(crate::error::SpecLoopError::Config(
                "--review and --plan require --spec ID".to_owned(),
            ));
        }
        Ok(())
    }

    pub fn validate_auth_options(&self) -> crate::error::Result<()> {
        if self.verbose || self.color.is_some() {
            return Err(crate::error::SpecLoopError::Config(
                "--verbose and --color apply to workflow invocations, not auth".to_owned(),
            ));
        }
        Ok(())
    }

    pub fn workflow_mode(&self) -> crate::error::Result<WorkflowMode> {
        match (self.mode.review, self.mode.plan) {
            (false, false) => Ok(WorkflowMode::Implement),
            (true, false) => Ok(WorkflowMode::Review),
            (false, true) => Ok(WorkflowMode::Plan),
            (true, true) => Err(crate::error::SpecLoopError::Config(
                "--review and --plan cannot be used together".to_owned(),
            )),
        }
    }

    pub const fn is_dry_run(&self) -> bool {
        self.execution.dry_run
    }

    pub const fn continue_requested(&self) -> bool {
        self.completion.continue_run
    }

    pub const fn verbose(&self) -> bool {
        self.verbose
    }

    pub const fn color_mode(&self) -> ColorMode {
        match self.color {
            Some(mode) => mode,
            None => ColorMode::Auto,
        }
    }
}
