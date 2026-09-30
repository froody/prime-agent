//! Working-plan steps and the transitions that mark compaction boundaries.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

const MAX_PLAN_STEPS: usize = 128;
const MAX_PLAN_STRING_BYTES: usize = 16_384;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanStatus {
    Pending,
    InProgress,
    Completed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanStep {
    pub id: String,
    pub goal: String,
    pub status: PlanStatus,
}

/// What changed between two plans.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PlanTransition {
    /// Steps newly moved to `completed`.
    pub completed_steps: Vec<PlanStep>,
    /// Corrective hints for the model.
    pub advice: Vec<String>,
}

/// Parse and bound a plan: at most 128 steps, non-empty strings of at most
/// 16 KiB, known statuses, unique ids.
#[must_use]
pub fn parse_plan_steps(value: &serde_json::Value) -> Option<Vec<PlanStep>> {
    let steps: Vec<PlanStep> = serde_json::from_value(value.clone()).ok()?;
    let bounded = |text: &str| !text.is_empty() && text.len() <= MAX_PLAN_STRING_BYTES;
    if steps.len() > MAX_PLAN_STEPS
        || !steps
            .iter()
            .all(|step| bounded(&step.id) && bounded(&step.goal))
    {
        return None;
    }
    let ids: HashSet<&str> = steps.iter().map(|step| step.id.as_str()).collect();
    (ids.len() == steps.len()).then_some(steps)
}

#[must_use]
pub fn analyze_plan_transition(previous: &[PlanStep], next: &[PlanStep]) -> PlanTransition {
    let previous_by_id: HashMap<&str, &PlanStep> = previous
        .iter()
        .map(|step| (step.id.as_str(), step))
        .collect();
    let mut transition = PlanTransition::default();
    for step in next {
        let prior = previous_by_id.get(step.id.as_str());
        let was_completed = prior.is_some_and(|prior| prior.status == PlanStatus::Completed);
        if !was_completed && step.status == PlanStatus::Completed {
            transition.completed_steps.push(step.clone());
        }
        if prior.is_some_and(|prior| prior.goal != step.goal) {
            let id = serde_json::to_string(&step.id).unwrap_or_default();
            transition.advice.push(format!(
                "Plan step {id} changed goal; reuse an id only for the same goal."
            ));
        }
    }
    let in_progress = next
        .iter()
        .filter(|step| step.status == PlanStatus::InProgress)
        .count();
    if in_progress > 1 {
        transition
            .advice
            .push("Keep at most one plan step in_progress.".to_string());
    }
    if in_progress == 0 && next.iter().any(|step| step.status == PlanStatus::Pending) {
        transition
            .advice
            .push("Mark one pending plan step in_progress before starting it.".to_string());
    }
    transition
}

#[must_use]
pub fn format_plan_snapshot(steps: &[PlanStep]) -> String {
    let body = serde_json::json!({ "steps": steps });
    format!("<sol-prime-plan task_status=\"active\">{body}</sol-prime-plan>")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn step(id: &str, goal: &str, status: PlanStatus) -> PlanStep {
        PlanStep {
            id: id.to_string(),
            goal: goal.to_string(),
            status,
        }
    }

    #[test]
    fn accepts_empty_plans_and_rejects_malformed_plans() {
        assert_eq!(parse_plan_steps(&json!([])), Some(Vec::new()));
        for bad in [
            json!([{ "goal": "missing id", "status": "pending" }]),
            json!([{ "id": "x", "goal": "x", "status": "unknown" }]),
            json!([{ "id": "x", "goal": "a", "status": "pending" }, { "id": "x", "goal": "b", "status": "pending" }]),
            json!([{ "id": "x", "goal": "x", "status": "pending", "extra": 1 }]),
            json!([{ "id": "", "goal": "x", "status": "pending" }]),
            json!("broken"),
        ] {
            assert_eq!(parse_plan_steps(&bad), None, "{bad}");
        }
        assert_eq!(
            parse_plan_steps(
                &json!([{ "id": "build", "goal": "build it", "status": "in_progress" }])
            ),
            Some(vec![step("build", "build it", PlanStatus::InProgress)])
        );
    }

    #[test]
    fn detects_only_new_transitions_into_completed() {
        let open = [step("build", "build it", PlanStatus::InProgress)];
        let done = [step("build", "build it", PlanStatus::Completed)];
        assert_eq!(analyze_plan_transition(&open, &done).completed_steps, done);
        assert!(analyze_plan_transition(&done, &done)
            .completed_steps
            .is_empty());
    }

    #[test]
    fn flags_ambiguous_active_work_and_reused_ids() {
        let transition = analyze_plan_transition(
            &[step("a", "old", PlanStatus::InProgress)],
            &[
                step("a", "new", PlanStatus::InProgress),
                step("b", "second", PlanStatus::InProgress),
            ],
        );
        assert_eq!(
            transition.advice,
            [
                "Plan step \"a\" changed goal; reuse an id only for the same goal.",
                "Keep at most one plan step in_progress.",
            ]
        );
        let idle = analyze_plan_transition(&[], &[step("a", "x", PlanStatus::Pending)]);
        assert_eq!(
            idle.advice,
            ["Mark one pending plan step in_progress before starting it."]
        );
    }

    #[test]
    fn formats_a_progress_only_snapshot() {
        assert_eq!(
            format_plan_snapshot(&[step("build", "build it", PlanStatus::InProgress)]),
            r#"<sol-prime-plan task_status="active">{"steps":[{"id":"build","goal":"build it","status":"in_progress"}]}</sol-prime-plan>"#
        );
    }
}
