use super::economics::CompactionReason;
use super::*;

fn step(id: &str, status: PlanStatus) -> PlanStep {
    PlanStep {
        id: id.to_string(),
        goal: format!("do {id}"),
        status,
    }
}

const BIG: BoundaryContext = BoundaryContext {
    context_tokens: 120_000,
    fixed_tokens: 5_000,
    context_window_tokens: Some(200_000),
};

/// Five requests per step with steady growth, then `a` completes.
fn after_first_boundary(ratio: f64) -> OnlineCompact {
    let mut controller = OnlineCompact::new(OnlineState::default(), ratio);
    let open = [
        step("a", PlanStatus::InProgress),
        step("b", PlanStatus::Pending),
        step("c", PlanStatus::Pending),
    ];
    controller.update_plan("call-0", &open, None);
    for request in 0..5 {
        controller.record_request(100_000 + request * 4_000);
    }
    let done = [
        step("a", PlanStatus::Completed),
        step("b", PlanStatus::InProgress),
        step("c", PlanStatus::Pending),
    ];
    let progress = PlanProgress {
        files_changed: vec!["a.rs".to_string()],
        verification: vec!["cargo test".to_string()],
        decisions: Vec::new(),
    };
    let outcome = controller.update_plan("call-1", &done, Some(&progress));
    assert_eq!(outcome.completed_step_ids, ["a"]);
    assert!(outcome.progress_recorded);
    assert!(outcome.text.starts_with("<sol-prime-plan"));
    controller
}

#[test]
fn a_cheap_cache_compacts_at_the_boundary_and_carries_its_debt() {
    let mut controller = after_first_boundary(1.5);
    let decision = controller
        .evaluate_boundary(&["call-1"], BIG)
        .expect("compacts");
    assert_eq!(decision.reason, CompactionReason::Economic);
    assert_eq!(
        controller.evaluate_boundary(&["call-1"], BIG),
        None,
        "the boundary is consumed"
    );
    controller.record_compaction();
    let state = controller.snapshot();
    assert_eq!(state.native_compaction_count, 1);
    assert_eq!(state.cache_debt_tokens, 60_000.0);
    assert_eq!(state.cache_debt_repayment_tokens, 94_000.0);
    assert!(state.plan.is_empty());
}

#[test]
fn an_expensive_cache_defers_until_the_window_needs_protection() {
    let mut controller = after_first_boundary(20.0);
    assert_eq!(controller.evaluate_boundary(&["call-1"], BIG), None);
    let mut controller = after_first_boundary(20.0);
    let near_full = BoundaryContext {
        context_tokens: 190_000,
        ..BIG
    };
    let decision = controller
        .evaluate_boundary(&["call-1"], near_full)
        .unwrap();
    assert_eq!(decision.reason, CompactionReason::WindowProtection);
}

#[test]
fn a_failed_turn_or_a_correction_cancels_the_boundary() {
    let mut controller = after_first_boundary(1.5);
    assert_eq!(controller.evaluate_boundary(&[], BIG), None);
    assert_eq!(controller.evaluate_boundary(&["call-1"], BIG), None);
    let mut controller = after_first_boundary(1.5);
    assert_eq!(controller.evaluate_boundary(&["other-call"], BIG), None);
    let mut controller = after_first_boundary(1.5);
    controller.record_correction();
    assert_eq!(controller.evaluate_boundary(&["call-1"], BIG), None);
    assert_eq!(controller.snapshot().epoch, 1);
    assert!(controller
        .snapshot()
        .completed_boundary_request_counts
        .is_empty());
}

#[test]
fn a_foreign_compaction_starts_a_debt_free_epoch() {
    let mut controller = after_first_boundary(1.5);
    controller.record_compaction();
    assert_eq!(controller.snapshot().cache_debt_tokens, 0.0);
    let mut controller = after_first_boundary(1.5);
    controller.evaluate_boundary(&["call-1"], BIG).unwrap();
    controller.compaction_failed();
    controller.record_compaction();
    assert_eq!(controller.snapshot().cache_debt_tokens, 0.0);
}

#[test]
fn a_plan_edit_without_completion_records_no_boundary() {
    let mut controller = OnlineCompact::new(OnlineState::default(), 1.5);
    let outcome = controller.update_plan("call-0", &[step("a", PlanStatus::Pending)], None);
    assert_eq!(
        outcome,
        PlanUpdateOutcome {
            text: format!(
                "{}\nMark one pending plan step in_progress before starting it.",
                format_plan_snapshot(&[step("a", PlanStatus::Pending)])
            ),
            completed_step_ids: Vec::new(),
            progress_recorded: false,
        }
    );
    assert_eq!(controller.evaluate_boundary(&["call-1"], BIG), None);
    assert_eq!(controller.snapshot().plan, [step("a", PlanStatus::Pending)]);
}
