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
    long_about = "Run from a project root containing `.spec-loop.toml`. The profile controls specification discovery, agent selection, checks, and optional lifecycle actions.",
    after_help = "Agent tools and configured commands run with your user's permissions and are not sandboxed. Use external sandboxing tools for isolation."
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
    #[command(
        about = "Manage built-in ChatGPT authorization.",
        long_about = r"Authentication commands do not load a repository profile. Set SPEC_LOOP_AUTH_DIR to an absolute directory that directly contains auth.json; it takes precedence over platform defaults and XDG_CONFIG_HOME. When unset, Linux and other non-macOS/non-Windows systems use $XDG_CONFIG_HOME/spec-loop (or $HOME/.config/spec-loop), macOS uses ~/Library/Application Support/Spec Loop, and Windows uses %APPDATA%\Spec Loop. Empty and relative overrides are rejected, and the directory must remain outside the active project. Changing locations does not migrate credentials. If the operating-system credential store is unavailable, auth.json may contain plaintext credentials; a custom location does not encrypt them or isolate them from same-user processes."
    )]
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
    #[command(
        long_about = "Authorize with ChatGPT. SPEC_LOOP_AUTH_DIR may select the absolute directory directly containing auth.json; it must remain outside the active project. When unset, platform defaults apply. A custom location does not encrypt credentials or isolate them from same-user processes; the file fallback may contain plaintext credentials."
    )]
    Login {
        /// Reauthorize a saved registration ID.
        #[arg(long, conflicts_with = "new_account", value_name = "REGISTRATION_ID")]
        account: Option<String>,
        /// Start a distinct dynamic client registration.
        #[arg(long, conflicts_with = "account")]
        new_account: bool,
    },
    /// Show local authorization and direct-use permission state.
    #[command(
        long_about = "Show local authorization state without loading a project profile. The registry path is selected by SPEC_LOOP_AUTH_DIR when set, otherwise by platform defaults. Custom storage must remain outside the active project; a file fallback may contain plaintext credentials and is not isolated from same-user processes."
    )]
    Status,
    /// Remove selected or active Spec Loop credentials.
    #[command(
        long_about = "Remove credentials only from the selected local registry. SPEC_LOOP_AUTH_DIR selects the absolute directory directly containing auth.json; when unset, platform defaults apply. Storage must remain outside the active project. A custom location does not encrypt credentials or isolate them from same-user processes."
    )]
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
