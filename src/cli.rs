use clap::{Args, Parser};

/// Command-line options for the generic specification workflow.
#[derive(Clone, Debug, Parser)]
#[command(
    name = "spec-loop",
    version,
    about = "Implement repository specs through a guarded agent loop",
    long_about = "Run the repository-root `.spec-loop.toml` workflow against current Git worktree. Requires Git, Pi, a valid profile, and an existing approved plan for implementation; use --plan to author one. Ordinary staged, unstaged, and untracked changes are preserved and reviewed. Repository instructions and selected specification remain authoritative.",
    after_help = "Retries and process timeouts are unlimited by default; completion.max_passes configures the retry cap for every Pi role. Checks use argv arrays from the profile, never a shell. Execution prints stage names and child command argv before launch; Pi prompt text is omitted. Review/audit prompts request no mutations; user-configured Pi resources remain available. Configured manual acceptance gates remain pending and prevent commit or continuation. Run logs remain outside the repository in a system temporary directory."
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
    /// Resume from combined Git-visible changes at checks and review.
    #[arg(long, conflicts_with = "plan")]
    review: bool,

    /// Author and review a plan only.
    #[arg(long, conflicts_with_all = ["review", "commit"])]
    plan: bool,
}

#[derive(Args, Clone, Debug, Default)]
struct ExecutionOptions {
    /// Print resolved work without invoking agents or mutating Git.
    #[arg(long)]
    dry_run: bool,
}

#[derive(Args, Clone, Debug, Default)]
struct CompletionOptions {
    /// Authorize a standard Git commit after review approval.
    #[arg(long)]
    commit: bool,

    /// Continue to another plan, or after an authorized implementation commit.
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
        if self.is_plan() && self.commit_requested() {
            return Err(crate::error::SpecLoopError::Config(
                "--plan cannot be combined with --commit".to_owned(),
            ));
        }
        if self.continue_requested() && !self.is_plan() && !self.commit_requested() {
            return Err(crate::error::SpecLoopError::Config(
                "--continue requires --commit in implementation mode".to_owned(),
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

    pub const fn commit_requested(&self) -> bool {
        self.completion.commit
    }

    pub const fn continue_requested(&self) -> bool {
        self.completion.continue_run
    }
}
