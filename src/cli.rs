use clap::{Args, Parser, Subcommand};

/// Command-line options for the generic specification workflow.
#[derive(Clone, Debug, Parser)]
#[command(
    name = "spec-loop",
    version,
    about = "Implement repository specs through a guarded agent loop",
    long_about = "Run the workflow from the current directory, which must contain `.spec-loop.toml`. Specs are discovered from configured filesystem globs; Git is not required. Schema v3 defaults to the built-in OpenAI Responses backend, which requires `agent.model` and `spec-loop auth login`; it uses ChatGPT OAuth and one automatic POSIX shell tool. Set `agent.backend = \"pi\"` to use Pi and preserve its effective resources. Repository instructions and selected specification remain authoritative.",
    after_help = "Generic role retries and process timeouts are unlimited by default; `completion.max_passes` configures the generic role retry cap, while built-in provider retries are limited to three per request. Built-in shell runs automatically with same-user permissions and is not sandboxed; commands may access or transmit available credentials and secrets. Checks and lifecycle actions use argv arrays, never a shell. Review-context commands run through selected-backend tools; default is `git diff`. Run logs remain outside the project directory and may contain sensitive shell output."
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

impl Cli {
    pub fn validate(&self) -> crate::error::Result<()> {
        if (self.is_review() || self.is_plan()) && self.spec.is_none() {
            return Err(crate::error::SpecLoopError::Config(
                "--review and --plan require --spec ID".to_owned(),
            ));
        }
        Ok(())
    }

    pub const fn is_review(&self) -> bool {
        self.mode.review
    }

    pub const fn is_plan(&self) -> bool {
        self.mode.plan
    }

    pub const fn is_dry_run(&self) -> bool {
        self.execution.dry_run
    }

    pub const fn continue_requested(&self) -> bool {
        self.completion.continue_run
    }
}
