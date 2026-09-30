//! Cache economics for a compaction at a plan boundary (upstream
//! `economics.ts`, unchanged math).
//!
//! A compaction rewrites the cached prefix: it costs `write_tokens *
//! (cache_write_read_ratio - 1)` extra, and saves `archive_tokens -
//! memo_tokens` on every later request. It pays off when the breakeven
//! request count fits in the expected remaining requests.

/// Tuning constants for the decision.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CompactionEconomics {
    pub remaining_request_scale: f64,
    pub remaining_request_stddev_k: f64,
    pub window_reserve_tokens: f64,
    pub first_compaction_request_scale: f64,
    pub subsequent_compaction_margin: f64,
}

pub const DEFAULT_COMPACTION_ECONOMICS: CompactionEconomics = CompactionEconomics {
    remaining_request_scale: 1.0,
    remaining_request_stddev_k: 0.0,
    window_reserve_tokens: 16_384.0,
    first_compaction_request_scale: 2.0,
    subsequent_compaction_margin: 1.5,
};

const MINIMUM_VARIANCE_SAMPLES: usize = 3;
const SMALL_SAMPLE_SCALE: f64 = 0.5;

/// Why the decision compacts or defers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionReason {
    Economic,
    WindowProtection,
    DeferredEconomic,
    DeferredSubsequentMargin,
    DeferredCarriedDebt,
    HorizonUnavailable,
    CacheRatioUnavailable,
    NonPositiveSaving,
}

/// Inputs to [`estimate_remaining_requests`].
#[derive(Debug, Clone, PartialEq)]
pub struct HorizonInput<'a> {
    pub completed_boundary_request_counts: &'a [u64],
    pub remaining_boundaries: u64,
    pub scale: f64,
    pub standard_deviation_k: f64,
    pub context_tokens: f64,
    pub context_window_tokens: Option<f64>,
    pub average_context_token_increment: Option<f64>,
}

/// The expected remaining requests before the task ends or the window fills.
#[derive(Debug, Clone, PartialEq)]
pub struct RequestHorizonEstimate {
    pub requests_per_boundary_mean: f64,
    pub requests_per_boundary_lower_bound: f64,
    pub unbounded_expected_remaining_requests: f64,
    pub window_request_upper_bound: Option<f64>,
    pub expected_remaining_requests: f64,
}

#[must_use]
#[allow(clippy::cast_precision_loss)] // request counts are far below 2^52
pub fn estimate_remaining_requests(input: &HorizonInput<'_>) -> RequestHorizonEstimate {
    let counts: Vec<f64> = input
        .completed_boundary_request_counts
        .iter()
        .map(|&count| count as f64)
        .collect();
    let mean = counts.iter().sum::<f64>() / counts.len().max(1) as f64;
    let mut lower_bound = mean;
    if input.standard_deviation_k != 0.0 {
        if counts.len() < MINIMUM_VARIANCE_SAMPLES {
            lower_bound *= SMALL_SAMPLE_SCALE;
        } else {
            let variance: f64 = counts.iter().map(|count| (count - mean).powi(2)).sum();
            let deviation = (variance / (counts.len() - 1) as f64).sqrt();
            lower_bound = (mean - input.standard_deviation_k * deviation).max(0.0);
        }
    }
    let unbounded = 1.0 + (lower_bound * input.remaining_boundaries as f64 * input.scale).floor();
    let window_request_upper_bound = match (
        input.context_window_tokens,
        input.average_context_token_increment,
    ) {
        (Some(window), Some(increment)) if increment > 0.0 => Some(
            ((window - input.context_tokens) / increment)
                .floor()
                .max(0.0),
        ),
        _ => None,
    };
    RequestHorizonEstimate {
        requests_per_boundary_mean: mean,
        requests_per_boundary_lower_bound: lower_bound,
        unbounded_expected_remaining_requests: unbounded,
        window_request_upper_bound,
        expected_remaining_requests: window_request_upper_bound
            .map_or(unbounded, |upper| unbounded.min(upper)),
    }
}

/// Inputs to [`decide_compaction`].
#[derive(Debug, Clone, PartialEq)]
pub struct CompactionInput<'a> {
    pub write_tokens: f64,
    pub archive_tokens: f64,
    pub memo_tokens: f64,
    pub context_tokens: f64,
    /// `None` when no boundary history exists yet.
    pub completed_boundary_request_counts: Option<&'a [u64]>,
    pub remaining_boundaries: u64,
    pub average_context_token_increment: Option<f64>,
    pub context_window_tokens: Option<f64>,
    pub prior_compaction_count: u64,
    pub carried_debt_tokens: f64,
    pub cache_write_read_ratio: Option<f64>,
    pub economics: CompactionEconomics,
}

/// The priced decision.
#[derive(Debug, Clone, PartialEq)]
pub struct CompactionDecision {
    pub horizon: Option<RequestHorizonEstimate>,
    pub breakeven_requests: Option<f64>,
    pub combined_breakeven_requests: Option<f64>,
    pub effective_horizon_requests: Option<f64>,
    pub incremental_cache_cost_ratio: Option<f64>,
    pub compact: bool,
    pub reason: CompactionReason,
}

#[must_use]
pub fn decide_compaction(input: &CompactionInput<'_>) -> CompactionDecision {
    let economics = input.economics;
    let horizon = input.completed_boundary_request_counts.map(|counts| {
        estimate_remaining_requests(&HorizonInput {
            completed_boundary_request_counts: counts,
            remaining_boundaries: input.remaining_boundaries,
            scale: economics.remaining_request_scale,
            standard_deviation_k: economics.remaining_request_stddev_k,
            context_tokens: input.context_tokens,
            context_window_tokens: input.context_window_tokens,
            average_context_token_increment: input.average_context_token_increment,
        })
    });
    let saving = input.archive_tokens - input.memo_tokens;
    let incremental = input
        .cache_write_read_ratio
        .map(|ratio| (ratio - 1.0).max(0.0));
    let positive_incremental = incremental.filter(|_| saving > 0.0);
    let breakeven = positive_incremental.map(|ratio| input.write_tokens * ratio / saving);
    let combined_breakeven = positive_incremental
        .map(|ratio| (input.carried_debt_tokens + input.write_tokens * ratio) / saving);
    let first = input.prior_compaction_count == 0;
    let expected = horizon
        .as_ref()
        .map(|horizon| horizon.expected_remaining_requests);
    let effective = horizon.as_ref().map(|horizon| {
        if first {
            (horizon.expected_remaining_requests * economics.first_compaction_request_scale)
                .min(horizon.window_request_upper_bound.unwrap_or(f64::INFINITY))
        } else {
            horizon.expected_remaining_requests
        }
    });
    let window_protection = input
        .context_window_tokens
        .is_some_and(|window| input.context_tokens >= window - economics.window_reserve_tokens);
    let fits = |requests: Option<f64>, needed: Option<f64>| match (requests, needed) {
        (Some(requests), Some(needed)) => needed <= requests,
        _ => false,
    };
    let base_economic =
        expected.is_some_and(|expected| expected > 0.0) && fits(expected, breakeven);
    let first_economic =
        first && effective.is_some_and(|effective| effective > 0.0) && fits(effective, breakeven);
    let margin_open = !first
        && fits(
            expected,
            breakeven.map(|breakeven| breakeven * economics.subsequent_compaction_margin),
        );
    let debt_gate_open = !first && fits(expected, combined_breakeven);
    let economic = if first {
        first_economic
    } else {
        base_economic && margin_open && debt_gate_open
    };
    let compressible = saving > 0.0;
    let reason = if !compressible {
        CompactionReason::NonPositiveSaving
    } else if window_protection {
        CompactionReason::WindowProtection
    } else if economic {
        CompactionReason::Economic
    } else if horizon.is_none() {
        CompactionReason::HorizonUnavailable
    } else if breakeven.is_none() {
        CompactionReason::CacheRatioUnavailable
    } else if !first && base_economic && !margin_open {
        CompactionReason::DeferredSubsequentMargin
    } else if !first && base_economic && !debt_gate_open {
        CompactionReason::DeferredCarriedDebt
    } else {
        CompactionReason::DeferredEconomic
    };
    CompactionDecision {
        horizon,
        breakeven_requests: breakeven,
        combined_breakeven_requests: combined_breakeven,
        effective_horizon_requests: effective,
        incremental_cache_cost_ratio: incremental,
        compact: compressible && (window_protection || economic),
        reason,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decision(edit: impl FnOnce(&mut CompactionInput<'static>)) -> CompactionDecision {
        let mut input = CompactionInput {
            write_tokens: 80_000.0,
            archive_tokens: 60_000.0,
            memo_tokens: 1_000.0,
            context_tokens: 80_000.0,
            completed_boundary_request_counts: Some(&[4, 6, 5]),
            remaining_boundaries: 4,
            average_context_token_increment: Some(2_000.0),
            context_window_tokens: Some(200_000.0),
            prior_compaction_count: 0,
            carried_debt_tokens: 0.0,
            cache_write_read_ratio: Some(1.0),
            economics: DEFAULT_COMPACTION_ECONOMICS,
        };
        edit(&mut input);
        decide_compaction(&input)
    }

    #[test]
    fn estimates_the_remaining_horizon_from_completed_boundaries() {
        let estimate = estimate_remaining_requests(&HorizonInput {
            completed_boundary_request_counts: &[4, 6, 5],
            remaining_boundaries: 3,
            scale: 1.0,
            standard_deviation_k: 0.0,
            context_tokens: 100_000.0,
            context_window_tokens: Some(200_000.0),
            average_context_token_increment: Some(5_000.0),
        });
        assert_eq!(
            estimate,
            RequestHorizonEstimate {
                requests_per_boundary_mean: 5.0,
                requests_per_boundary_lower_bound: 5.0,
                unbounded_expected_remaining_requests: 16.0,
                window_request_upper_bound: Some(20.0),
                expected_remaining_requests: 16.0,
            }
        );
    }

    #[test]
    fn a_standard_deviation_lowers_the_horizon() {
        let input = |counts: &'static [u64]| HorizonInput {
            completed_boundary_request_counts: counts,
            remaining_boundaries: 2,
            scale: 1.0,
            standard_deviation_k: 1.0,
            context_tokens: 0.0,
            context_window_tokens: None,
            average_context_token_increment: None,
        };
        assert_eq!(
            estimate_remaining_requests(&input(&[4, 6])).requests_per_boundary_lower_bound,
            2.5
        );
        let spread = estimate_remaining_requests(&input(&[4, 6, 5]));
        assert_eq!(spread.requests_per_boundary_lower_bound, 4.0);
        assert_eq!(spread.expected_remaining_requests, 9.0);
    }

    #[test]
    fn each_reason_is_reachable() {
        let cases: Vec<(CompactionDecision, bool, CompactionReason)> = vec![
            (
                decision(|i| {
                    i.archive_tokens = 500.0;
                }),
                false,
                CompactionReason::NonPositiveSaving,
            ),
            (decision(|_| {}), true, CompactionReason::Economic),
            (
                decision(|i| {
                    i.context_tokens = 195_000.0;
                    i.cache_write_read_ratio = Some(100.0);
                    i.economics.window_reserve_tokens = 10_000.0;
                }),
                true,
                CompactionReason::WindowProtection,
            ),
            (
                decision(|i| {
                    i.cache_write_read_ratio = None;
                }),
                false,
                CompactionReason::CacheRatioUnavailable,
            ),
            (
                decision(|i| {
                    i.completed_boundary_request_counts = None;
                }),
                false,
                CompactionReason::HorizonUnavailable,
            ),
            (
                decision(|i| {
                    i.cache_write_read_ratio = Some(100.0);
                }),
                false,
                CompactionReason::DeferredEconomic,
            ),
            (
                decision(|i| {
                    i.prior_compaction_count = 1;
                    i.cache_write_read_ratio = Some(2.0);
                    i.carried_debt_tokens = 2_000_000.0;
                }),
                false,
                CompactionReason::DeferredCarriedDebt,
            ),
            (
                decision(|i| {
                    i.prior_compaction_count = 1;
                    i.cache_write_read_ratio = Some(13.0);
                }),
                false,
                CompactionReason::DeferredSubsequentMargin,
            ),
        ];
        for (index, (result, compact, reason)) in cases.into_iter().enumerate() {
            assert_eq!(
                (result.compact, result.reason),
                (compact, reason),
                "case {index}"
            );
        }
    }

    #[test]
    fn carried_debt_raises_the_combined_breakeven() {
        let result = decision(|i| {
            i.prior_compaction_count = 1;
            i.cache_write_read_ratio = Some(2.0);
            i.carried_debt_tokens = 2_000_000.0;
        });
        assert!(result.combined_breakeven_requests > result.breakeven_requests);
    }
}
