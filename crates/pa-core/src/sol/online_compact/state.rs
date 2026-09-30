//! Online Context Compact state: request counts, boundary history, context
//! growth, and carried cache debt. Every transition is a pure function; the
//! controller persists snapshots as `sol-prime-online-context-state-v1`
//! custom session entries and restores the latest valid one.

use pa_types::session::FileEntry;
use serde::{Deserialize, Serialize};

use super::plan::{parse_plan_steps, PlanStep};

pub const ONLINE_STATE_ENTRY: &str = "sol-prime-online-context-state-v1";

/// Progress evidence recorded when a step completes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProgressSummary {
    pub step_id: String,
    pub goal: String,
    pub files_changed: Vec<String>,
    pub verification: Vec<String>,
    pub decisions: Vec<String>,
    pub next_work: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OnlineState {
    pub version: u32,
    pub epoch: u64,
    pub plan: Vec<PlanStep>,
    pub pending_progress: Vec<ProgressSummary>,
    pub request_count: u64,
    pub last_boundary_request_count: u64,
    pub completed_boundary_request_counts: Vec<u64>,
    pub last_context_tokens: Option<u64>,
    pub positive_context_delta_total: f64,
    pub positive_context_delta_count: u64,
    pub native_compaction_count: u64,
    pub cache_debt_tokens: f64,
    pub cache_debt_repayment_tokens: f64,
}

/// Debt a compaction leaves behind and the per-request saving that repays it.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct CacheDebt {
    pub debt_tokens: f64,
    pub repayment_tokens: f64,
}

impl Default for OnlineState {
    fn default() -> Self {
        OnlineState {
            version: 1,
            epoch: 0,
            plan: Vec::new(),
            pending_progress: Vec::new(),
            request_count: 0,
            last_boundary_request_count: 0,
            completed_boundary_request_counts: Vec::new(),
            last_context_tokens: None,
            positive_context_delta_total: 0.0,
            positive_context_delta_count: 0,
            native_compaction_count: 0,
            cache_debt_tokens: 0.0,
            cache_debt_repayment_tokens: 0.0,
        }
    }
}

fn parse_online_state(value: &serde_json::Value) -> Option<OnlineState> {
    let state: OnlineState = serde_json::from_value(value.clone()).ok()?;
    let finite = |value: f64| value.is_finite() && value >= 0.0;
    let valid = state.version == 1
        && parse_plan_steps(&value["plan"]).is_some()
        && state.last_boundary_request_count <= state.request_count
        && finite(state.positive_context_delta_total)
        && finite(state.cache_debt_tokens)
        && finite(state.cache_debt_repayment_tokens);
    valid.then_some(state)
}

/// The latest valid snapshot on the branch, or a fresh state.
#[must_use]
pub fn restore_online_state(entries: &[FileEntry]) -> OnlineState {
    entries
        .iter()
        .rev()
        .find_map(|entry| match entry {
            FileEntry::Custom { payload, .. } if payload.custom_type == ONLINE_STATE_ENTRY => {
                payload.data.as_ref().and_then(parse_online_state)
            }
            _ => None,
        })
        .unwrap_or_default()
}

impl OnlineState {
    /// One provider request at `context_tokens`: count it, track positive
    /// growth, and repay carried cache debt.
    #[must_use]
    #[allow(clippy::cast_precision_loss)] // token counts are far below 2^52
    pub fn record_provider_request(&self, context_tokens: u64) -> Self {
        let delta = self
            .last_context_tokens
            .map_or(0.0, |last| context_tokens as f64 - last as f64);
        let cache_debt_tokens =
            (self.cache_debt_tokens - self.cache_debt_repayment_tokens).max(0.0);
        OnlineState {
            request_count: self.request_count + 1,
            last_context_tokens: Some(context_tokens),
            positive_context_delta_total: self.positive_context_delta_total + delta.max(0.0),
            positive_context_delta_count: self.positive_context_delta_count
                + u64::from(delta > 0.0),
            cache_debt_tokens,
            cache_debt_repayment_tokens: if cache_debt_tokens == 0.0 {
                0.0
            } else {
                self.cache_debt_repayment_tokens
            },
            ..self.clone()
        }
    }

    /// A completed plan step: record the request interval since the last one.
    #[must_use]
    pub fn record_boundary(&self, plan: &[PlanStep], progress: Option<ProgressSummary>) -> Self {
        let mut next = self.clone();
        next.plan = plan.to_vec();
        next.pending_progress.extend(progress);
        next.last_boundary_request_count = self.request_count;
        next.completed_boundary_request_counts.push(
            self.request_count
                .saturating_sub(self.last_boundary_request_count),
        );
        next
    }

    /// A native compaction: start a clean epoch carrying `debt`.
    #[must_use]
    pub fn record_compaction(&self, debt: CacheDebt) -> Self {
        OnlineState {
            epoch: self.epoch + 1,
            plan: Vec::new(),
            pending_progress: Vec::new(),
            last_context_tokens: None,
            positive_context_delta_total: 0.0,
            positive_context_delta_count: 0,
            native_compaction_count: self.native_compaction_count + 1,
            cache_debt_tokens: debt.debt_tokens.max(0.0),
            cache_debt_repayment_tokens: debt.repayment_tokens.max(0.0),
            ..self.clone()
        }
    }

    /// A user correction mid-run: drop stale plan history and debt.
    #[must_use]
    pub fn record_correction(&self) -> Self {
        OnlineState {
            epoch: self.epoch + 1,
            plan: Vec::new(),
            pending_progress: Vec::new(),
            last_boundary_request_count: self.request_count,
            completed_boundary_request_counts: Vec::new(),
            last_context_tokens: None,
            positive_context_delta_total: 0.0,
            positive_context_delta_count: 0,
            cache_debt_tokens: 0.0,
            cache_debt_repayment_tokens: 0.0,
            ..self.clone()
        }
    }

    /// Mean positive context growth per request, when any growth was seen.
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn average_context_increment(&self) -> Option<f64> {
        (self.positive_context_delta_count > 0)
            .then(|| self.positive_context_delta_total / self.positive_context_delta_count as f64)
    }
}

#[cfg(test)]
mod tests {
    use super::super::plan::PlanStatus;
    use super::*;

    fn plan() -> Vec<PlanStep> {
        vec![
            PlanStep {
                id: "inspect".into(),
                goal: "inspect the implementation".into(),
                status: PlanStatus::Completed,
            },
            PlanStep {
                id: "verify".into(),
                goal: "verify the change".into(),
                status: PlanStatus::InProgress,
            },
        ]
    }

    fn progress() -> ProgressSummary {
        ProgressSummary {
            step_id: "inspect".into(),
            goal: "inspect the implementation".into(),
            files_changed: vec!["src/a.rs".into()],
            verification: vec!["targeted test passed".into()],
            decisions: vec!["keep the change small".into()],
            next_work: vec!["verify the change".into()],
        }
    }

    fn custom(data: &serde_json::Value) -> FileEntry {
        serde_json::from_value(serde_json::json!({
            "type": "custom", "id": "e", "parentId": null, "timestamp": "1970-01-01T00:00:00.000Z",
            "customType": ONLINE_STATE_ENTRY, "data": data,
        }))
        .unwrap()
    }

    #[test]
    fn restores_the_latest_valid_snapshot_and_ignores_a_malformed_tail() {
        let state = OnlineState::default()
            .record_provider_request(100)
            .record_boundary(&plan(), Some(progress()));
        let entries = [
            custom(&serde_json::to_value(&state).unwrap()),
            custom(&serde_json::json!({ "version": 1, "plan": "broken" })),
        ];
        assert_eq!(restore_online_state(&entries), state);
        assert_eq!(restore_online_state(&[]), OnlineState::default());
    }

    #[test]
    fn counts_requests_growth_and_debt_repayment() {
        let charged = OnlineState {
            cache_debt_tokens: 300.0,
            cache_debt_repayment_tokens: 100.0,
            ..OnlineState::default()
        };
        let third = charged
            .record_provider_request(1_000)
            .record_provider_request(1_250)
            .record_provider_request(900);
        assert_eq!(
            third,
            OnlineState {
                request_count: 3,
                last_context_tokens: Some(900),
                positive_context_delta_total: 250.0,
                positive_context_delta_count: 1,
                ..OnlineState::default()
            }
        );
    }

    #[test]
    fn records_one_interval_and_one_summary_per_boundary() {
        let state = OnlineState::default()
            .record_provider_request(100)
            .record_provider_request(200)
            .record_boundary(&plan(), Some(progress()))
            .record_provider_request(300)
            .record_boundary(&plan(), None);
        assert_eq!(state.completed_boundary_request_counts, [2, 1]);
        assert_eq!(state.last_boundary_request_count, 3);
        assert_eq!(state.pending_progress, [progress()]);
    }

    #[test]
    fn compaction_and_correction_start_clean_epochs() {
        let before = OnlineState::default()
            .record_provider_request(5_000)
            .record_boundary(&plan(), Some(progress()));
        let compacted = before.record_compaction(CacheDebt {
            debt_tokens: 1_200.0,
            repayment_tokens: 300.0,
        });
        assert_eq!(
            compacted,
            OnlineState {
                epoch: 1,
                request_count: 1,
                last_boundary_request_count: 1,
                completed_boundary_request_counts: vec![1],
                native_compaction_count: 1,
                cache_debt_tokens: 1_200.0,
                cache_debt_repayment_tokens: 300.0,
                ..OnlineState::default()
            }
        );
        assert_eq!(
            before.record_correction(),
            OnlineState {
                epoch: 1,
                request_count: 1,
                last_boundary_request_count: 1,
                ..OnlineState::default()
            }
        );
    }
}
