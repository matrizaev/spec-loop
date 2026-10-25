use clap::{Args, Parser};

/// Command-line options for the generic specification workflow.
#[derive(Clone, Debug, Parser)]
#[command(
    name = "spec-loop",
    version,
    about = "Implement repository specs through a guarded agent loop",
    long_about = "Run the workflow from the current directory, which must contain `.spec-loop.toml`. Specs are discovered from configured filesystem globs; Git is not required. Pi preserves its effective tools, extensions, MCP, skills, trust, context, and inherited environment. Repository instructions and selected specification remain authoritative.",
    after_help = "Retries and process timeouts are unlimited by default; completion.max_passes configures the retry cap for every Pi role. Checks and lifecycle actions use argv arrays from the profile, never a shell, and run from the current directory. Review-context command runs through reviewer Pi tools and defaults to `git diff`; configure another command for non-Git projects. Run logs remain outside the project directory in a system temporary directory."
)]
pub struct Cli {
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
