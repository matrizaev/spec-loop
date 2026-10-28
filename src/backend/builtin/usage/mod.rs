//! Numeric-only observations of built-in provider and shell execution boundaries.

use serde_json::Value;

use crate::error::Result;

#[cfg(test)]
mod tests;

/// Numeric request and response observations for one built-in role invocation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UsageSnapshot {
    /// Successfully handed-off HTTP request attempts, or unknown after counter overflow.
    pub provider_request_attempts: Option<u64>,
    /// Completed provider responses, including malformed final role handoffs.
    pub completed_responses: Option<u64>,
    /// Shell processes whose spawn succeeded.
    pub executed_shell_calls: Option<u64>,
    /// Numeric usage from the latest completed response, if any.
    pub latest_response: Option<ResponseUsage>,
    cumulative: UsageTotals,
}

impl Default for UsageSnapshot {
    fn default() -> Self {
        Self {
            provider_request_attempts: Some(0),
            completed_responses: Some(0),
            executed_shell_calls: Some(0),
            latest_response: None,
            cumulative: UsageTotals::default(),
        }
    }
}

impl UsageSnapshot {
    /// Records one HTTP request handed to the provider client.
    pub fn request_sent(&mut self) {
        increment(&mut self.provider_request_attempts);
    }

    /// Records one shell process immediately after a successful spawn.
    pub fn shell_spawned(&mut self) {
        increment(&mut self.executed_shell_calls);
    }

    /// Records one completed provider response and parses only numeric usage fields.
    pub fn response_completed(&mut self, response: &Value) {
        increment(&mut self.completed_responses);
        let usage = ResponseUsage::from_response(response);
        self.cumulative.record(&usage);
        self.latest_response = Some(usage);
    }

    /// Merges another invocation's observed delta without changing either input.
    pub fn merged(&self, delta: &Self) -> Self {
        let latest_response = delta
            .latest_response
            .clone()
            .or_else(|| self.latest_response.clone());
        Self {
            provider_request_attempts: add_counters(
                self.provider_request_attempts,
                delta.provider_request_attempts,
            ),
            completed_responses: add_counters(self.completed_responses, delta.completed_responses),
            executed_shell_calls: add_counters(
                self.executed_shell_calls,
                delta.executed_shell_calls,
            ),
            latest_response,
            cumulative: self.cumulative.merged(&delta.cumulative),
        }
    }

    /// Formats numeric-only role usage, with unknown values shown explicitly.
    pub fn format_report(
        &self,
        role: &str,
        model: &str,
        elapsed_seconds: u64,
        context_window_tokens: Option<u64>,
    ) -> String {
        let latest = self.latest_response.as_ref();
        let cumulative = &self.cumulative;
        format!(
            "📊 Built-in · {role} · {model} · {elapsed_seconds}s\n↻ {} req · ✓ {} completed · ⚙ {} shell calls\n🧠 Last response: {} in · {} out · {} total · cache {} · reasoning {} · context {} of user limit {}\n∑ Observed completed-response usage: {} in · {} out · {} total · cache {} · reasoning {}",
            display(self.provider_request_attempts),
            display(self.completed_responses),
            display(self.executed_shell_calls),
            display_option(latest.and_then(|usage| usage.input)),
            display_option(latest.and_then(|usage| usage.output)),
            display_option(latest.and_then(|usage| usage.total)),
            display_option(latest.and_then(|usage| usage.cached_input)),
            display_option(latest.and_then(|usage| usage.reasoning_output)),
            context_percentage(latest, context_window_tokens),
            display_option(context_window_tokens),
            display_option(cumulative.input_tokens),
            display_option(cumulative.output_tokens),
            display_option(cumulative.total_tokens),
            display_option(cumulative.cached_input_tokens),
            display_option(cumulative.reasoning_output_tokens),
        )
    }
}

/// Couples a role's usage delta with its sanitized live-report callback.
pub struct UsageReporter<'a> {
    /// Mutable observations updated at request, response, and spawn boundaries.
    pub snapshot: &'a mut UsageSnapshot,
    observer: &'a mut (dyn FnMut(&UsageSnapshot) -> Result<()> + Send),
}

impl<'a> UsageReporter<'a> {
    /// Creates a boundary reporter over invocation-local observations.
    pub fn new(
        snapshot: &'a mut UsageSnapshot,
        observer: &'a mut (dyn FnMut(&UsageSnapshot) -> Result<()> + Send),
    ) -> Self {
        Self { snapshot, observer }
    }

    /// Emits the current sanitized numeric snapshot through the caller's formatter.
    pub fn report(&mut self) -> Result<()> {
        (self.observer)(self.snapshot)
    }
}

/// Validated numeric usage from one completed provider response.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ResponseUsage {
    /// Provider-reported input tokens after consistency validation.
    pub input: Option<u64>,
    /// Provider-reported output tokens after consistency validation.
    pub output: Option<u64>,
    /// Provider-reported total, required to equal input plus output.
    pub total: Option<u64>,
    /// Optional cached-input breakdown; never added to input totals.
    pub cached_input: Option<u64>,
    /// Optional reasoning-output breakdown; never added to output totals.
    pub reasoning_output: Option<u64>,
}

impl ResponseUsage {
    /// Parses checked unsigned counts and validates parent/breakdown consistency.
    pub fn from_response(response: &Value) -> Self {
        let Some(usage) = response.get("usage").filter(|value| !value.is_null()) else {
            return Self::default();
        };
        let (Some(input), Some(output), Some(total)) = (
            unsigned(usage.get("input_tokens")),
            unsigned(usage.get("output_tokens")),
            unsigned(usage.get("total_tokens")),
        ) else {
            return Self::default();
        };
        if input.checked_add(output) != Some(total) {
            return Self::default();
        }
        Self {
            input: Some(input),
            output: Some(output),
            total: Some(total),
            cached_input: breakdown(usage, "input_tokens_details", "cached_tokens", input),
            reasoning_output: breakdown(usage, "output_tokens_details", "reasoning_tokens", output),
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct UsageTotals {
    responses_seen: bool,
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    total_tokens: Option<u64>,
    cached_input_tokens: Option<u64>,
    reasoning_output_tokens: Option<u64>,
}

impl UsageTotals {
    const fn record(&mut self, usage: &ResponseUsage) {
        if !self.responses_seen {
            self.responses_seen = true;
            self.input_tokens = usage.input;
            self.output_tokens = usage.output;
            self.total_tokens = usage.total;
            self.cached_input_tokens = usage.cached_input;
            self.reasoning_output_tokens = usage.reasoning_output;
            return;
        }
        self.input_tokens = add_options(self.input_tokens, usage.input);
        self.output_tokens = add_options(self.output_tokens, usage.output);
        self.total_tokens = add_options(self.total_tokens, usage.total);
        self.cached_input_tokens = add_options(self.cached_input_tokens, usage.cached_input);
        self.reasoning_output_tokens =
            add_options(self.reasoning_output_tokens, usage.reasoning_output);
    }

    fn merged(&self, other: &Self) -> Self {
        if !other.responses_seen {
            return self.clone();
        }
        if !self.responses_seen {
            return other.clone();
        }
        Self {
            responses_seen: true,
            input_tokens: add_options(self.input_tokens, other.input_tokens),
            output_tokens: add_options(self.output_tokens, other.output_tokens),
            total_tokens: add_options(self.total_tokens, other.total_tokens),
            cached_input_tokens: add_options(self.cached_input_tokens, other.cached_input_tokens),
            reasoning_output_tokens: add_options(
                self.reasoning_output_tokens,
                other.reasoning_output_tokens,
            ),
        }
    }
}

fn unsigned(value: Option<&Value>) -> Option<u64> {
    value.and_then(Value::as_u64)
}

fn breakdown(usage: &Value, details: &str, key: &str, parent: u64) -> Option<u64> {
    let value = usage.get(details)?.get(key)?;
    let count = value.as_u64()?;
    (count <= parent).then_some(count)
}

fn increment(value: &mut Option<u64>) {
    *value = value.and_then(|count| count.checked_add(1));
}

const fn add_counters(left: Option<u64>, right: Option<u64>) -> Option<u64> {
    match (left, right) {
        (Some(left), Some(right)) => left.checked_add(right),
        _ => None,
    }
}

const fn add_options(left: Option<u64>, right: Option<u64>) -> Option<u64> {
    match (left, right) {
        (Some(left), Some(right)) => left.checked_add(right),
        _ => None,
    }
}

fn display(value: Option<u64>) -> String {
    value.map_or_else(|| "unknown".to_owned(), grouped_count)
}

fn grouped_count(count: u64) -> String {
    let mut grouped = String::new();
    for (index, digit) in count.to_string().chars().rev().enumerate() {
        if index != 0 && index % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    grouped.chars().rev().collect()
}

fn display_option(value: Option<u64>) -> String {
    display(value)
}

fn context_percentage(usage: Option<&ResponseUsage>, limit: Option<u64>) -> String {
    let (Some(usage), Some(limit)) = (usage, limit) else {
        return "unknown".to_owned();
    };
    if limit == 0 {
        return "unknown".to_owned();
    }
    let Some(tokens) = usage
        .input
        .zip(usage.output)
        .and_then(|(input, output)| input.checked_add(output))
    else {
        return "unknown".to_owned();
    };
    let Some(tenths) = u128::from(tokens)
        .checked_mul(1000)
        .map(|scaled| scaled / u128::from(limit))
    else {
        return "unknown".to_owned();
    };
    format!("{}.{:01}%", tenths / 10, tenths % 10)
}
