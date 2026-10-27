use std::io::{IsTerminal, Write};

use clap::ValueEnum;

use crate::error::{Result, io_error};
use crate::protocol::{
    self, Actionability, AuditResult, EvidenceResult, ImplementationAuditResponse,
    ImplementationClarificationResponse, ImplementationResponse, ImplementationReviewResponse,
    PlanAuditResponse, PlanAuthorResponse, PlanClarificationResponse, PlanReviewResponse,
    Readiness, ReviewResult,
};

#[cfg(test)]
mod tests;

/// Selects styling behavior for formatter-owned workflow output.
#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum ColorMode {
    /// Enable styling only for terminal streams when terminal color is allowed.
    Auto,
    /// Force formatter styling even for redirected streams.
    Always,
    /// Disable formatter styling.
    Never,
}

/// Sanitizes and presents workflow output using one resolved stream color policy.
#[derive(Clone, Debug)]
pub struct Output {
    stdout_color: bool,
    stderr_color: bool,
    verbose: bool,
}

impl Output {
    /// Resolves color behavior independently for stdout and stderr.
    pub fn new(mode: ColorMode, verbose: bool) -> Self {
        Self::with_environment(
            mode,
            verbose,
            std::io::stdout().is_terminal(),
            std::io::stderr().is_terminal(),
            std::env::var_os("TERM").as_deref(),
            std::env::var_os("NO_COLOR").as_deref(),
        )
    }

    fn with_environment(
        mode: ColorMode,
        verbose: bool,
        stdout_terminal: bool,
        stderr_terminal: bool,
        term: Option<&std::ffi::OsStr>,
        no_color: Option<&std::ffi::OsStr>,
    ) -> Self {
        let auto = term != Some(std::ffi::OsStr::new("dumb"))
            && no_color.is_none_or(std::ffi::OsStr::is_empty);
        let enabled = |terminal| match mode {
            ColorMode::Auto => terminal && auto,
            ColorMode::Always => true,
            ColorMode::Never => false,
        };
        Self {
            stdout_color: enabled(stdout_terminal),
            stderr_color: enabled(stderr_terminal),
            verbose,
        }
    }

    /// Creates a formatter that emits plain text without ANSI styling.
    pub const fn plain() -> Self {
        Self {
            stdout_color: false,
            stderr_color: false,
            verbose: false,
        }
    }

    /// Sanitizes and writes human-readable text to stdout.
    pub fn stdout(&self, text: &str) -> Result<()> {
        let rendered = sanitize(text);
        let styled = style_status(&rendered, self.stdout_color);
        write_stdout(&styled)
    }

    /// Sanitizes and writes human-readable diagnostics to stderr.
    pub fn stderr(&self, text: &str) -> Result<()> {
        let rendered = sanitize(text);
        let styled = style_status(&rendered, self.stderr_color);
        let stderr = std::io::stderr();
        let mut lock = stderr.lock();
        writeln!(lock, "{styled}")
            .and_then(|()| lock.flush())
            .map_err(|source| io_error("write workflow error", "<stderr>", source))
    }

    /// Writes a flushed progress or retry message to stdout.
    pub fn progress(&self, text: &str) -> Result<()> {
        self.stdout(&format!("{text}\n"))
    }

    /// Writes a cyan stage heading and dimmed secondary lines.
    pub fn stage(&self, text: &str) -> Result<()> {
        write_stdout(&render_stage(text, self.stdout_color))
    }

    /// Writes indented, dimmed secondary metadata to stdout.
    pub fn metadata(&self, text: &str) -> Result<()> {
        write_stdout(&render_metadata(text, self.stdout_color))
    }

    /// Presents a validated typed response in concise or verbose structured form.
    pub fn role_result<T: TypedResult>(&self, value: &T) -> Result<()> {
        let rendered = sanitize(&format!("\n{}\n", value.render(self.verbose)));
        let mut lines = rendered.split_inclusive('\n');
        let mut styled = String::new();
        if let Some(heading) = lines.next() {
            styled.push_str(&style(heading, "\x1b[1;36m", self.stdout_color));
        }
        for line in lines {
            styled.push_str(&style_status(line, self.stdout_color));
        }
        write_stdout(&styled)
    }

    /// Returns a sanitized text preview bounded by Unicode scalar count.
    pub fn display_text(text: &str, limit: usize) -> String {
        preview(text, limit)
    }
}

fn render_stage(text: &str, enabled: bool) -> String {
    let sanitized = sanitize(text);
    let (heading, details) = sanitized
        .split_once('\n')
        .unwrap_or((sanitized.as_str(), ""));
    let mut rendered = format!(
        "\n{}\n",
        style(&format!("[RUNNING] {heading}"), "\x1b[1;36m", enabled)
    );
    rendered.push_str(&render_metadata_lines(details, enabled, false));
    rendered
}

fn render_metadata(text: &str, enabled: bool) -> String {
    render_indented_metadata(&sanitize(text), enabled)
}

fn render_indented_metadata(text: &str, enabled: bool) -> String {
    render_metadata_lines(text, enabled, true)
}

fn render_metadata_lines(text: &str, enabled: bool, indent: bool) -> String {
    let mut rendered = String::new();
    for line in text.split_inclusive('\n') {
        let line = if indent {
            format!("  {line}")
        } else {
            line.to_owned()
        };
        rendered.push_str(&style(&line, "\x1b[2m", enabled));
    }
    if !text.is_empty() && !text.ends_with('\n') {
        rendered.push('\n');
    }
    rendered
}

fn style_status(text: &str, enabled: bool) -> String {
    if !enabled {
        return text.to_owned();
    }
    let mut rendered = String::new();
    for line in text.split_inclusive('\n') {
        let style_code = if line.contains("[ERROR]") || line.contains("[FAIL]") {
            Some("\x1b[31m")
        } else if line.contains("[BLOCKED]") || line.contains("[RETRY]") || line.contains("[WARN]")
        {
            Some("\x1b[33m")
        } else if line.contains("[PASS]") {
            Some("\x1b[32m")
        } else if line.starts_with("  ") || line.starts_with("Run logs") || line.starts_with("Log:")
        {
            Some("\x1b[2m")
        } else {
            None
        };
        if let Some(code) = style_code {
            rendered.push_str(&style(line, code, true));
        } else {
            rendered.push_str(line);
        }
    }
    rendered
}

fn write_stdout(text: &str) -> Result<()> {
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    write!(lock, "{text}")
        .and_then(|()| lock.flush())
        .map_err(|source| io_error("write workflow output", "<stdout>", source))
}

fn style(text: &str, code: &str, enabled: bool) -> String {
    if enabled {
        format!("{code}{text}\x1b[0m")
    } else {
        text.to_owned()
    }
}

/// Renders a successfully validated protocol response without serializing it as JSON.
pub trait TypedResult {
    /// Renders all displayable fields, applying concise previews unless verbose.
    fn render(&self, verbose: bool) -> String;
}

impl TypedResult for PlanAuthorResponse {
    fn render(&self, verbose: bool) -> String {
        let mut lines = vec![format!("Plan author — {}", readiness_name(&self.result))];
        push_field(&mut lines, "Role", &self.role, verbose);
        push_field(
            &mut lines,
            "Protocol version",
            &self.protocol_version.to_string(),
            verbose,
        );
        match &self.result {
            Readiness::ReadyForReview => lines.push("  [PASS] Plan ready for review".to_owned()),
            Readiness::Blocked { reason } => lines.push(format!(
                "  [BLOCKED] Reason: {}",
                field_text(reason, verbose)
            )),
        }
        lines.join("\n")
    }
}

impl TypedResult for ImplementationResponse {
    fn render(&self, verbose: bool) -> String {
        let mut lines = vec![format!("Implementation — {}", readiness_name(&self.result))];
        push_field(&mut lines, "Role", &self.role, verbose);
        push_field(
            &mut lines,
            "Protocol version",
            &self.protocol_version.to_string(),
            verbose,
        );
        lines.push(format!("  Summary: {}", field_text(&self.summary, verbose)));
        if let Readiness::Blocked { reason } = &self.result {
            lines.push(format!(
                "  [BLOCKED] Reason: {}",
                field_text(reason, verbose)
            ));
        } else {
            lines.push("  [PASS] Ready for review".to_owned());
        }
        lines.join("\n")
    }
}

macro_rules! impl_review_result {
    ($ty:ty, $heading:literal) => {
        impl TypedResult for $ty {
            fn render(&self, verbose: bool) -> String {
                let mut lines = vec![format!("{} — {}", $heading, review_name(&self.result))];
                push_field(&mut lines, "Role", &self.role, verbose);
                push_field(
                    &mut lines,
                    "Protocol version",
                    &self.protocol_version.to_string(),
                    verbose,
                );
                match &self.result {
                    ReviewResult::Blocked { reason } => lines.push(format!(
                        "  [BLOCKED] Reason: {}",
                        field_text(reason, verbose)
                    )),
                    ReviewResult::NoIssues { findings } | ReviewResult::Issues { findings } => {
                        lines.push(format!(
                            "  [{}] Findings: {}",
                            if findings.is_empty() { "PASS" } else { "WARN" },
                            findings.len()
                        ));
                        if findings.is_empty() {
                            lines.push("  No findings".to_owned());
                        }
                        for finding in findings {
                            lines.push(format!(
                                "  Finding #{} — {}",
                                finding.id,
                                field_text(&finding.location, verbose)
                            ));
                            lines.push(format!(
                                "    Requirement: {}",
                                field_text(&finding.requirement, verbose)
                            ));
                            lines.push(format!(
                                "    Violation: {}",
                                field_text(&finding.violation, verbose)
                            ));
                            if verbose {
                                lines
                                    .push(format!("    Evidence: {}", sanitize(&finding.evidence)));
                                lines.push(format!(
                                    "    Proposed correction: {}",
                                    sanitize(&finding.proposed_correction)
                                ));
                            }
                        }
                    }
                }
                lines.join("\n")
            }
        }
    };
}

impl_review_result!(PlanReviewResponse, "Plan review");
impl_review_result!(ImplementationReviewResponse, "Review");

macro_rules! impl_audit_result {
    ($ty:ty, $heading:literal) => {
        impl TypedResult for $ty {
            fn render(&self, verbose: bool) -> String {
                let mut lines = vec![format!("{} — {}", $heading, audit_name(&self.result))];
                push_field(&mut lines, "Role", &self.role, verbose);
                push_field(
                    &mut lines,
                    "Protocol version",
                    &self.protocol_version.to_string(),
                    verbose,
                );
                match &self.result {
                    AuditResult::Blocked { reason } => lines.push(format!(
                        "  [BLOCKED] Reason: {}",
                        field_text(reason, verbose)
                    )),
                    AuditResult::Complete { classifications } => {
                        lines.push("  [PASS] Audit complete".to_owned());
                        if classifications.is_empty() {
                            lines.push("  No findings".to_owned());
                        }
                        for item in classifications {
                            lines.push(format!(
                                "  Finding #{} — {} / {}",
                                item.finding_id,
                                classification_name(item.classification),
                                actionability_name(item.actionability)
                            ));
                            if verbose {
                                lines.push(format!("    Rationale: {}", sanitize(&item.rationale)));
                            }
                        }
                    }
                }
                lines.join("\n")
            }
        }
    };
}

impl_audit_result!(PlanAuditResponse, "Plan audit");
impl_audit_result!(ImplementationAuditResponse, "Audit");

macro_rules! impl_evidence_result {
    ($ty:ty, $heading:literal) => {
        impl TypedResult for $ty {
            fn render(&self, verbose: bool) -> String {
                let mut lines = vec![format!("{} — {}", $heading, evidence_name(&self.result))];
                push_field(&mut lines, "Role", &self.role, verbose);
                push_field(
                    &mut lines,
                    "Protocol version",
                    &self.protocol_version.to_string(),
                    verbose,
                );
                match &self.result {
                    EvidenceResult::Blocked { reason } => lines.push(format!(
                        "  [BLOCKED] Reason: {}",
                        field_text(reason, verbose)
                    )),
                    EvidenceResult::Complete { evidence } => {
                        lines.push("  [PASS] Evidence clarification complete".to_owned());
                        if evidence.is_empty() {
                            lines.push("  No citations".to_owned());
                        }
                        for entry in evidence {
                            lines.push(format!(
                                "  Finding #{} — {} citation(s)",
                                entry.finding_id,
                                entry.citations.len()
                            ));
                            if entry.citations.is_empty() {
                                lines.push("    No citations".to_owned());
                            }
                            for citation in &entry.citations {
                                lines.push(format!(
                                    "    {} — {}",
                                    field_text(&citation.path, verbose),
                                    field_text(&citation.location, verbose)
                                ));
                                if verbose {
                                    lines.push(format!(
                                        "      Excerpt: {}",
                                        sanitize(&citation.excerpt)
                                    ));
                                    lines.push(format!(
                                        "      Relevance: {}",
                                        sanitize(&citation.relevance)
                                    ));
                                }
                            }
                        }
                    }
                }
                lines.join("\n")
            }
        }
    };
}

impl_evidence_result!(PlanClarificationResponse, "Plan clarification");
impl_evidence_result!(ImplementationClarificationResponse, "Clarification");

const fn readiness_name(value: &Readiness) -> &'static str {
    match value {
        Readiness::ReadyForReview => "ready for review",
        Readiness::Blocked { .. } => "blocked",
    }
}
const fn review_name(value: &ReviewResult) -> &'static str {
    match value {
        ReviewResult::NoIssues { .. } => "no issues",
        ReviewResult::Issues { .. } => "findings",
        ReviewResult::Blocked { .. } => "blocked",
    }
}
const fn audit_name(value: &AuditResult) -> &'static str {
    match value {
        AuditResult::Complete { .. } => "complete",
        AuditResult::Blocked { .. } => "blocked",
    }
}
const fn evidence_name(value: &EvidenceResult) -> &'static str {
    match value {
        EvidenceResult::Complete { .. } => "complete",
        EvidenceResult::Blocked { .. } => "blocked",
    }
}
const fn classification_name(value: protocol::Classification) -> &'static str {
    match value {
        protocol::Classification::Supported => "supported",
        protocol::Classification::Unsupported => "unsupported",
        protocol::Classification::Ambiguous => "ambiguous",
    }
}
const fn actionability_name(value: Actionability) -> &'static str {
    match value {
        Actionability::Required => "required",
        Actionability::NotRequired => "not_required",
        Actionability::Ambiguous => "ambiguous",
    }
}
fn field_text(value: &str, verbose: bool) -> String {
    if verbose {
        sanitize(value)
    } else {
        preview(value, 160)
    }
}
fn push_field(lines: &mut Vec<String>, name: &str, value: &str, verbose: bool) {
    if verbose {
        lines.push(format!("  {name}: {}", sanitize(value)));
    }
}

fn preview(value: &str, limit: usize) -> String {
    let pieces = sanitized_pieces(value);
    let mut result = String::new();
    let mut count = 0usize;
    let mut truncated = false;
    for piece in pieces {
        let width = piece.chars().count();
        if count.saturating_add(width) > limit {
            truncated = true;
            break;
        }
        count += width;
        result.push_str(&piece);
    }
    if truncated {
        result.push_str("… [truncated; use --verbose]");
    }
    result
}

fn sanitize(value: &str) -> String {
    sanitized_pieces(value).concat()
}

fn sanitized_pieces(value: &str) -> Vec<String> {
    let chars: Vec<char> = value.chars().collect();
    let mut pieces = Vec::new();
    let mut index = 0;
    while index < chars.len() {
        let character = chars[index];
        if character == '\u{1b}' {
            let end = skip_escape(&chars, index);
            append_preserved_spacing(&mut pieces, &chars[index..end]);
            index = end;
            continue;
        }
        if matches!(
            character,
            '\u{009b}' | '\u{009d}' | '\u{0090}' | '\u{0098}' | '\u{009e}' | '\u{009f}'
        ) {
            let end = skip_c1_sequence(&chars, index, character);
            append_preserved_spacing(&mut pieces, &chars[index..end]);
            index = end;
            continue;
        }
        if character == '\n' || character == '\t' {
            pieces.push(character.to_string());
        } else if character.is_control() {
            pieces.push(format!("\\u{{{:x}}}", u32::from(character)));
        } else {
            pieces.push(character.to_string());
        }
        index += 1;
    }
    pieces
}

fn append_preserved_spacing(pieces: &mut Vec<String>, sequence: &[char]) {
    pieces.extend(
        sequence
            .iter()
            .filter(|character| matches!(character, '\n' | '\t'))
            .map(char::to_string),
    );
}

fn skip_escape(chars: &[char], start: usize) -> usize {
    let Some(&next) = chars.get(start + 1) else {
        return chars.len();
    };
    if next == '[' {
        return skip_csi(chars, start + 2);
    }
    if next == ']' {
        return skip_string(chars, start + 2, true);
    }
    if matches!(next, 'P' | 'X' | '^' | '_') {
        return skip_string(chars, start + 2, false);
    }
    let mut index = start + 1;
    while index < chars.len() && ('\u{20}'..='\u{2f}').contains(&chars[index]) {
        index += 1;
    }
    if index < chars.len() {
        index + 1
    } else {
        chars.len()
    }
}

fn skip_c1_sequence(chars: &[char], start: usize, kind: char) -> usize {
    if kind == '\u{009b}' {
        skip_csi(chars, start + 1)
    } else {
        skip_string(chars, start + 1, kind == '\u{009d}')
    }
}

fn skip_csi(chars: &[char], mut index: usize) -> usize {
    while index < chars.len() {
        let character = chars[index];
        index += 1;
        if ('\u{40}'..='\u{7e}').contains(&character) {
            return index;
        }
    }
    chars.len()
}

fn skip_string(chars: &[char], mut index: usize, osc: bool) -> usize {
    while index < chars.len() {
        if osc && chars[index] == '\u{7}' {
            return index + 1;
        }
        if chars[index] == '\u{009c}' {
            return index + 1;
        }
        if chars[index] == '\u{1b}' && chars.get(index + 1) == Some(&'\\') {
            return index + 2;
        }
        index += 1;
    }
    chars.len()
}
