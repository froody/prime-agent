//! Online Context Compact: price a compaction at each completed plan step
//! with upstream SoL-Pi's cache economics, instead of waiting for the
//! threshold compaction.
//!
//! [`OnlineCompact`] is the per-session controller. The host feeds it plan
//! updates ([`OnlineCompact::update_plan`]), provider requests
//! ([`OnlineCompact::record_request`]) and compactions; at a turn end it asks
//! [`OnlineCompact::evaluate_boundary`] whether the pending boundary should
//! compact now. Snapshots persist through [`OnlineCompact::snapshot`] as
//! [`state::ONLINE_STATE_ENTRY`] custom entries.

pub mod economics;
pub mod plan;
pub mod state;

use economics::{
    decide_compaction, CompactionDecision, CompactionInput, DEFAULT_COMPACTION_ECONOMICS,
};
use plan::{analyze_plan_transition, format_plan_snapshot, PlanStatus, PlanStep};
use state::{CacheDebt, OnlineState, ProgressSummary};

pub const DEFAULT_KEEP_RECENT_TOKENS: u64 = 20_000;
pub const DEFAULT_NATIVE_SUMMARY_TOKEN_ESTIMATE: u64 = 1_000;
pub const BOUNDARY_COMPACTION_INSTRUCTIONS: &str =
    "Preserve completed work, verification results, important decisions, and remaining work.";
pub const POST_COMPACTION_PLAN_REMINDER: &str = "Online context compaction finished. The parent task is still active. Before continuing work, call update_plan with a fresh plan for the remaining work.";

/// Progress evidence the model attaches to a plan update.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PlanProgress {
    pub files_changed: Vec<String>,
    pub verification: Vec<String>,
    pub decisions: Vec<String>,
}

/// The reply to a plan update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanUpdateOutcome {
    /// The text the model sees: the plan snapshot plus any advice.
    pub text: String,
    pub completed_step_ids: Vec<String>,
    pub progress_recorded: bool,
}

/// The turn-end facts a boundary decision needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BoundaryContext {
    /// Current context size (reported usage or an estimate).
    pub context_tokens: u64,
    /// System prompt and other fixed prefix tokens compaction cannot remove.
    pub fixed_tokens: u64,
    pub context_window_tokens: Option<u64>,
}

/// Per-session Online Context Compact controller.
#[derive(Debug, Clone, PartialEq)]
pub struct OnlineCompact {
    state: OnlineState,
    cache_write_read_ratio: f64,
    pending_boundary: Option<String>,
    active_debt: Option<CacheDebt>,
}

impl OnlineCompact {
    /// A controller resumed from `state` (see [`state::restore_online_state`]).
    #[must_use]
    pub fn new(state: OnlineState, cache_write_read_ratio: f64) -> Self {
        OnlineCompact {
            state,
            cache_write_read_ratio,
            pending_boundary: None,
            active_debt: None,
        }
    }

    #[must_use]
    pub fn snapshot(&self) -> &OnlineState {
        &self.state
    }

    /// Replace the working plan. A step newly moved to `completed` records a
    /// boundary that the next [`Self::evaluate_boundary`] prices.
    pub fn update_plan(
        &mut self,
        tool_call_id: &str,
        steps: &[PlanStep],
        progress: Option<&PlanProgress>,
    ) -> PlanUpdateOutcome {
        let transition = analyze_plan_transition(&self.state.plan, steps);
        let completed_step_ids: Vec<String> = transition
            .completed_steps
            .iter()
            .map(|step| step.id.clone())
            .collect();
        if let Some(first) = transition.completed_steps.first() {
            let summary = progress.map(|progress| ProgressSummary {
                step_id: first.id.clone(),
                goal: first.goal.clone(),
                files_changed: progress.files_changed.clone(),
                verification: progress.verification.clone(),
                decisions: progress.decisions.clone(),
                next_work: steps
                    .iter()
                    .filter(|step| step.status != PlanStatus::Completed)
                    .map(|step| step.goal.clone())
                    .collect(),
            });
            self.state = self.state.record_boundary(steps, summary);
            self.pending_boundary
                .get_or_insert_with(|| tool_call_id.to_string());
        } else {
            self.state.plan = steps.to_vec();
        }
        let mut text = vec![format_plan_snapshot(steps)];
        text.extend(transition.advice);
        PlanUpdateOutcome {
            text: text.join("\n"),
            progress_recorded: !completed_step_ids.is_empty() && progress.is_some(),
            completed_step_ids,
        }
    }

    /// One provider request at `context_tokens`.
    pub fn record_request(&mut self, context_tokens: u64) {
        self.state = self.state.record_provider_request(context_tokens);
    }

    /// Price the pending boundary at a turn end. `succeeded_tool_calls` are
    /// the ids of this turn's tool calls that finished without error (empty
    /// when the turn errored or aborted); the boundary counts only when its
    /// `update_plan` call is among them. The boundary is consumed either
    /// way. `Some` means compact now.
    #[allow(clippy::cast_precision_loss)] // token counts are far below 2^52
    pub fn evaluate_boundary(
        &mut self,
        succeeded_tool_calls: &[&str],
        context: BoundaryContext,
    ) -> Option<CompactionDecision> {
        let tool_call_id = self.pending_boundary.take()?;
        if !succeeded_tool_calls.contains(&tool_call_id.as_str()) || self.active_debt.is_some() {
            return None;
        }
        let write_tokens = context.context_tokens as f64;
        let archive_tokens = context
            .context_tokens
            .saturating_sub(context.fixed_tokens)
            .saturating_sub(DEFAULT_KEEP_RECENT_TOKENS) as f64;
        let memo_tokens = DEFAULT_NATIVE_SUMMARY_TOKEN_ESTIMATE as f64;
        let remaining = self
            .state
            .plan
            .iter()
            .filter(|step| step.status != PlanStatus::Completed)
            .count();
        let decision = decide_compaction(&CompactionInput {
            write_tokens,
            archive_tokens,
            memo_tokens,
            context_tokens: write_tokens,
            completed_boundary_request_counts: Some(&self.state.completed_boundary_request_counts),
            remaining_boundaries: remaining as u64,
            average_context_token_increment: self.state.average_context_increment(),
            context_window_tokens: context.context_window_tokens.map(|window| window as f64),
            prior_compaction_count: self.state.native_compaction_count,
            carried_debt_tokens: self.state.cache_debt_tokens,
            cache_write_read_ratio: Some(self.cache_write_read_ratio),
            economics: DEFAULT_COMPACTION_ECONOMICS,
        });
        if !decision.compact {
            return None;
        }
        self.active_debt = Some(CacheDebt {
            debt_tokens: write_tokens * decision.incremental_cache_cost_ratio.unwrap_or(0.0),
            repayment_tokens: (archive_tokens - memo_tokens).max(0.0),
        });
        Some(decision)
    }

    /// A compaction finished. Only a compaction this controller started
    /// carries its cache debt; any other starts a debt-free epoch.
    pub fn record_compaction(&mut self) {
        let debt = self.active_debt.take().unwrap_or_default();
        self.state = self.state.record_compaction(debt);
        self.pending_boundary = None;
    }

    /// The compaction this controller started failed.
    pub fn compaction_failed(&mut self) {
        self.active_debt = None;
    }

    /// A steer or `CORRECTION:` input arrived mid-run.
    pub fn record_correction(&mut self) {
        self.pending_boundary = None;
        self.active_debt = None;
        self.state = self.state.record_correction();
    }
}

#[cfg(test)]
mod tests;
