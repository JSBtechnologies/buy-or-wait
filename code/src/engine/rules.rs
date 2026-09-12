//! Every numeric/threshold rule the engine depends on, gathered in one struct so RULES.md
//! (analyst) slices can be dropped in without touching the algorithms, and so the evaluation
//! harness can score rule variants against the samples.
//!
//! Values marked `PROVISIONAL` are placeholders until the matching RULES.md slice is published;
//! they are deliberately simple and must not be read as the recovered simulator behaviour.

use serde::{Deserialize, Serialize};

use super::money::Cents;
use super::types::PaymentOption;

/// How a regular variable-spend stream turns its recent occurrences into a projected amount.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AmountEstimator {
    /// Most recent occurrence.
    Last,
    /// Maximum of the last `n` occurrences.
    MaxOfLast(usize),
    /// Mean of the last `n` occurrences, rounded up to the cent.
    MeanOfLast(usize),
}

impl AmountEstimator {
    /// `amounts` is chronological (oldest first). Empty input yields zero.
    pub fn estimate(self, amounts: &[Cents]) -> Cents {
        match self {
            AmountEstimator::Last => amounts.last().copied().unwrap_or(Cents::ZERO),
            AmountEstimator::MaxOfLast(n) => {
                amounts.iter().rev().take(n).copied().max().unwrap_or(Cents::ZERO)
            }
            AmountEstimator::MeanOfLast(n) => {
                let tail: Vec<Cents> = amounts.iter().rev().take(n).copied().collect();
                if tail.is_empty() {
                    return Cents::ZERO;
                }
                let sum: i64 = tail.iter().map(|c| c.0).sum();
                let len = tail.len() as i64;
                Cents((sum + len - 1).div_euclid(len))
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Rules {
    /// Number of forecast days starting at request_date (day 0). PROVISIONAL: 90 days,
    /// i.e. request_date ..= request_date + 89.
    pub horizon_days: i64,

    // ---- ledger (§2.1) ------------------------------------------------------------------
    /// Pending debits are reserved on request_date (money already gone). If false they are
    /// reserved on their settlement date instead. PROVISIONAL: true (PLAN.md §2.1 wording).
    pub reserve_pending_on_request_date: bool,
    /// Scheduled rows whose cash date is before request_date are assumed already reflected in
    /// the balance and ignored. PROVISIONAL.
    pub ignore_scheduled_before_request_date: bool,

    // ---- recurrence (§2.2) --------------------------------------------------------------
    /// Minimum occurrences for a described bill/income to be a recurring stream. PROVISIONAL: 3.
    pub min_stream_occurrences: usize,
    /// Day-of-month spread tolerated for a monthly cadence. PROVISIONAL: 3.
    pub monthly_dom_tolerance: u32,
    /// Day spread tolerated between intervals for a fixed-interval cadence. PROVISIONAL: 1.
    pub interval_tolerance_days: i64,
    /// Categories projected as a spending rate rather than a fixed bill. PROVISIONAL.
    pub variable_categories: Vec<String>,
    /// Amount estimator for fixed-cadence bills whose amount varies. PROVISIONAL: MaxOfLast(3).
    pub bill_estimator: AmountEstimator,
    /// Amount estimator for variable-spend streams. PROVISIONAL: MaxOfLast(3) (PLAN.md §1 hint).
    pub variable_estimator: AmountEstimator,
    /// Occurrences considered when estimating a variable-spend interval. PROVISIONAL: 6.
    pub variable_interval_lookback: usize,
    /// A scheduled row replaces a projected stream occurrence of the same category/direction
    /// within this many days. PROVISIONAL: 5.
    pub scheduled_match_window_days: i64,

    // ---- plans (§2.8) -------------------------------------------------------------------
    /// Plans that finish after desired_completion_date are unsafe and dropped (problem
    /// statement: "safe only if ... complete the full request by its deadline"). PROVISIONAL.
    pub drop_late_plans: bool,
    /// Payments dated after the forecast horizon are ignored by the safety check. PROVISIONAL.
    pub ignore_payments_after_horizon: bool,
    /// Maximum spending-change actions in one plan (problem statement: up to three).
    pub max_spending_changes: usize,
    /// How change plans are ordered after "no changes". RULES S1.3: fewest changes.
    pub change_preference: ChangePreference,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChangePreference {
    /// Fewer stop/reduce actions first, then the smaller cut (RULES S1.3).
    FewestChanges,
    /// Smallest total cut to projected spending first, then fewer actions. Indistinguishable
    /// from `FewestChanges` on tuning rows 01–18.
    SmallestCut,
}

impl Default for Rules {
    fn default() -> Self {
        Rules {
            horizon_days: 90,
            reserve_pending_on_request_date: true,
            ignore_scheduled_before_request_date: true,
            min_stream_occurrences: 3,
            monthly_dom_tolerance: 3,
            interval_tolerance_days: 1,
            variable_categories: ["groceries", "dining", "transport"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            bill_estimator: AmountEstimator::MaxOfLast(3),
            variable_estimator: AmountEstimator::MaxOfLast(3),
            variable_interval_lookback: 6,
            scheduled_match_window_days: 5,
            drop_late_plans: true,
            ignore_payments_after_horizon: true,
            max_spending_changes: 3,
            change_preference: ChangePreference::FewestChanges,
        }
    }
}

impl Rules {
    pub fn is_variable_category(&self, category: &str) -> bool {
        self.variable_categories.iter().any(|c| c == category)
    }

    /// Rounds the closed-form safe amount for output. PROVISIONAL: floor to the cent (the
    /// forecast is already in cents, so this is the identity until RULES.md says otherwise).
    pub fn round_safe_amount(&self, raw: Cents) -> Cents {
        raw
    }

    /// Months an installment option spans, compared with `max_installment_months`.
    /// PROVISIONAL: the number of payments.
    pub fn installment_months(&self, option: &PaymentOption) -> u32 {
        option.number_of_payments
    }

    /// The amount a projected reduce_to target is lowered to. PROVISIONAL: the event's
    /// minimum_allowed_amount (samples 11 and 21 reduce exactly to it).
    pub fn reduce_to_amount(&self, minimum_allowed: Cents) -> Cents {
        minimum_allowed
    }
}
