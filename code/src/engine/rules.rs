//! Every numeric/threshold rule the engine depends on, gathered in one struct so RULES.md
//! (analyst) slices drop in without touching the algorithms, and so rule variants can be
//! scored against the samples. Each field cites the RULES.md section it implements.

use chrono::{Datelike, Duration, NaiveDate};
use serde::{Deserialize, Serialize};

use super::money::Money;
use super::types::PaymentOption;

/// How a stream turns its historical amounts into a projected amount.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AmountEstimator {
    /// Most recent occurrence.
    Last,
    /// Maximum of the last `n` occurrences.
    MaxOfLast(usize),
    /// Mean of the last `n` occurrences.
    MeanOfLast(usize),
    /// Mean of every occurrence.
    MeanAll,
}

impl AmountEstimator {
    /// `amounts` is chronological (oldest first). Identical amounts always yield that value
    /// (RULES S3.3). Empty input yields zero. No rounding beyond the internal unit.
    pub fn estimate(self, amounts: &[Money]) -> Money {
        let Some(&first) = amounts.first() else { return Money::ZERO };
        if amounts.iter().all(|&a| a == first) {
            return first;
        }
        let mean = |xs: &[Money]| Money(xs.iter().map(|m| m.0 as i128).sum::<i128>().div_euclid(xs.len() as i128) as i64);
        let tail = |n: usize| &amounts[amounts.len().saturating_sub(n)..];
        match self {
            AmountEstimator::Last => *amounts.last().unwrap(),
            AmountEstimator::MaxOfLast(n) => tail(n).iter().copied().max().unwrap(),
            AmountEstimator::MeanOfLast(n) => mean(tail(n)),
            AmountEstimator::MeanAll => mean(amounts),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Horizon {
    /// `rd ..= rd + (days - 1)`.
    Days(i64),
    /// `rd ..=` last calendar day of month(rd) + n (RULES S3.1: n = 2).
    EndOfMonthPlus(u32),
}

impl Horizon {
    pub fn end(self, rd: NaiveDate) -> NaiveDate {
        match self {
            Horizon::Days(d) => rd + Duration::days(d - 1),
            Horizon::EndOfMonthPlus(n) => {
                let m0 = rd.month0() + n + 1; // first day of the month after the last month
                let (y, m) = (rd.year() + (m0 / 12) as i32, m0 % 12 + 1);
                NaiveDate::from_ymd_opt(y, m, 1).expect("valid month") - Duration::days(1)
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChangePreference {
    /// Fewer stop/reduce actions first, then the smaller cut (RULES S1.3).
    FewestChanges,
    /// Smallest total cut to projected spending first, then fewer actions. Indistinguishable
    /// from `FewestChanges` on tuning rows 01–18.
    SmallestCut,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Rules {
    /// RULES S3.1 [FIT, strong]: through the end of month(rd) + 2.
    pub horizon: Horizon,

    // ---- ledger (S2) --------------------------------------------------------------------
    /// Pending debits reserved on rd if true, else on max(rd, settlement_date) (S2.1; both
    /// give identical tuning labels).
    pub reserve_pending_on_request_date: bool,
    /// Scheduled rows dated before rd are assumed already reflected in the balance.
    pub ignore_scheduled_before_request_date: bool,

    // ---- recurrence (S3.2–S3.4) ---------------------------------------------------------
    /// Minimum rows for a described monthly stream (S3.2: 2/3/4 identical on tuning; use 3).
    pub min_stream_occurrences: usize,
    /// Inclusive day-gap range that counts as monthly (S3.2: 28..=31).
    pub monthly_gap_days: (i64, i64),
    /// Monthly bills whose amounts vary (S3.3: mean of last 3).
    pub bill_estimator: AmountEstimator,
    /// Interval (variable spending) streams (S3.3: mean of all).
    pub variable_estimator: AmountEstimator,
    /// Income streams (S3.4: last settled amount).
    pub income_estimator: AmountEstimator,
    /// Credit descriptions never projected (S3.4), matched case-insensitively as substrings.
    pub one_off_income_keywords: Vec<String>,
    /// Credit descriptions marking the last payment of a stream (S3.4: "Final employer payroll").
    pub income_end_keywords: Vec<String>,
    /// An income stream whose next expected occurrence fell before rd has stopped (S3.4).
    pub stop_income_after_missed_occurrence: bool,
    /// A scheduled row replaces its calendar month's occurrence of a monthly stream with the
    /// same category and direction (S2.1 for salary; verifier#45 for scheduled bills).
    pub scheduled_replaces_month_occurrence: bool,

    // ---- plans (S1) ---------------------------------------------------------------------
    /// Plans finishing after desired_completion_date are dropped (S1.1 `pays[-1] <= due`).
    pub drop_late_plans: bool,
    /// Payments dated after the forecast horizon are ignored by the safety check. [GUESS]
    pub ignore_payments_after_horizon: bool,
    /// Maximum spending-change actions in one plan (problem statement: up to three).
    pub max_spending_changes: usize,
    /// How change plans are ordered after "no changes". S1.3: fewest changes.
    pub change_preference: ChangePreference,
}

fn strings(xs: &[&str]) -> Vec<String> {
    xs.iter().map(|s| s.to_string()).collect()
}

impl Default for Rules {
    fn default() -> Self {
        Rules {
            horizon: Horizon::EndOfMonthPlus(2),
            reserve_pending_on_request_date: false,
            ignore_scheduled_before_request_date: true,
            min_stream_occurrences: 3,
            monthly_gap_days: (28, 31),
            bill_estimator: AmountEstimator::MeanOfLast(3),
            variable_estimator: AmountEstimator::MeanAll,
            income_estimator: AmountEstimator::Last,
            one_off_income_keywords: strings(&[
                "bonus", "commission", "arrears", "prize", "lottery", "reimbursement", "refund", "payout", "windfall",
            ]),
            income_end_keywords: strings(&["final"]),
            stop_income_after_missed_occurrence: true,
            scheduled_replaces_month_occurrence: true,
            drop_late_plans: true,
            ignore_payments_after_horizon: true,
            max_spending_changes: 3,
            change_preference: ChangePreference::FewestChanges,
        }
    }
}

impl Rules {
    pub fn horizon_end(&self, rd: NaiveDate) -> NaiveDate {
        self.horizon.end(rd)
    }

    /// Rounds the closed-form safe amount to cents. Floor, so the partial plan built from it
    /// replays as safe; differs from half-up only when FX amounts sit in the trough (no tuning
    /// row). [GUESS vs S1.5 "rounded to 2 dp"]
    pub fn round_safe_amount(&self, raw: Money) -> Money {
        raw.floor_to_cent()
    }

    /// Months an installment option spans, compared with `max_installment_months`.
    /// S1.1 [FIT]: the number of payments.
    pub fn installment_months(&self, option: &PaymentOption) -> u32 {
        option.number_of_payments
    }

    /// The amount a reduce_to target is lowered to. S1.2 [EXACT]: minimum_allowed_amount.
    pub fn reduce_to_amount(&self, minimum_allowed: Money) -> Money {
        minimum_allowed
    }

    pub fn is_one_off_income(&self, description: &str) -> bool {
        contains_any(description, &self.one_off_income_keywords)
    }

    pub fn is_income_end(&self, description: &str) -> bool {
        contains_any(description, &self.income_end_keywords)
    }
}

fn contains_any(text: &str, keywords: &[String]) -> bool {
    let lower = text.to_lowercase();
    keywords.iter().any(|k| lower.contains(k.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn horizon_end_of_month_plus_two() {
        let d = |s: &str| NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap();
        assert_eq!(Horizon::EndOfMonthPlus(2).end(d("2025-02-07")), d("2025-04-30"));
        assert_eq!(Horizon::EndOfMonthPlus(2).end(d("2025-11-06")), d("2026-01-31"));
        assert_eq!(Horizon::EndOfMonthPlus(2).end(d("2023-12-31")), d("2024-02-29"));
        assert_eq!(Horizon::Days(91).end(d("2025-02-07")), d("2025-05-08"));
    }
}
