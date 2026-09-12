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
    #[serde(alias = "mean_all")]
    MeanAll,
    /// Median of every occurrence (mean of the middle two when even).
    #[serde(alias = "median_all")]
    MedianAll,
    /// (min + max) / 2 over every occurrence.
    #[serde(alias = "mid_all")]
    MidAll,
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
            AmountEstimator::MedianAll => {
                let mut v = amounts.to_vec();
                v.sort();
                let n = v.len();
                if n % 2 == 1 { v[n / 2] } else { mean(&v[n / 2 - 1..=n / 2]) }
            }
            AmountEstimator::MidAll => {
                let (lo, hi) = (*amounts.iter().min().unwrap(), *amounts.iter().max().unwrap());
                mean(&[lo, hi])
            }
        }
    }
}

/// S0 `SAME_DAY_ORDER`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DayOrder {
    #[serde(alias = "debits_first")]
    DebitsFirst,
    #[serde(alias = "credits_first")]
    CreditsFirst,
}

/// S0 `PAYMENT_TIMING`: when a plan payment on day d is applied.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PaymentTiming {
    /// After all of day d's rows (so after its credits).
    #[serde(alias = "after_day_rows")]
    AfterDayRows,
    /// As a debit, before day d's credits.
    #[serde(alias = "before_credits")]
    BeforeCredits,
}

/// S0 `INSTALLMENT_LIMIT`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum InstallmentLimit {
    /// number_of_payments <= max_installment_months.
    #[serde(alias = "n_payments")]
    NPayments,
    /// ceil(number_of_payments * frequency_days / 30) <= max_installment_months.
    #[serde(alias = "ceil_months")]
    CeilMonths,
}

/// S0 `NOT_REC_TEMPLATE`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum NotRecommendedTemplate {
    #[serde(alias = "B_iff_partial_only")]
    BIffPartialOnly,
    #[serde(alias = "always_A")]
    AlwaysA,
}

/// S0 `AFFORDABLE_NOW_TEMPLATE`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AffordableNowTemplate {
    #[serde(alias = "leaves_at_least")]
    LeavesAtLeast,
    #[serde(alias = "keeps_minimum")]
    KeepsMinimum,
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
    #[serde(alias = "fewest_changes")]
    FewestChanges,
    /// Smallest total cut to projected spending first, then fewer actions. Indistinguishable
    /// from `FewestChanges` on tuning rows 01–18.
    #[serde(alias = "smallest_cut")]
    SmallestCut,
}

/// All fields default per RULES.md; `#[serde(default)]` lets the verifier A/B any subset as a
/// JSON patch over `Rules::default()`. RULES S0 toggle names are given in brackets.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Rules {
    /// [HORIZON] S3.1 [FIT, strong]: through the end of month(rd) + 2. `fixed_90` (rd..=rd+90)
    /// is `Days(91)`; `fixed_86` is `Days(87)`.
    #[serde(alias = "HORIZON")]
    pub horizon: Horizon,
    /// [VAR_HORIZON] Horizon for Interval (variable-spend) projections only; `None` = same as
    /// `horizon`, `Some(Days(91))` = rd_plus_90. The forecast itself still ends at `horizon`.
    #[serde(alias = "VAR_HORIZON")]
    pub variable_horizon: Option<Horizon>,
    /// [SAME_DAY_ORDER] S2.3: debits before credits.
    #[serde(alias = "SAME_DAY_ORDER")]
    pub same_day_order: DayOrder,
    /// [PAYMENT_TIMING] S3.5 (fixed 52b4184): plan payments apply after the day's rows.
    #[serde(alias = "PAYMENT_TIMING")]
    pub payment_timing: PaymentTiming,

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
    #[serde(alias = "BILL_ESTIMATOR")]
    pub bill_estimator: AmountEstimator,
    /// Interval (variable spending) streams (S3.3: mean of all).
    #[serde(alias = "VAR_ESTIMATOR")]
    pub variable_estimator: AmountEstimator,
    /// Income streams (S3.4: last settled amount).
    pub income_estimator: AmountEstimator,
    /// Credit descriptions never projected (S3.4), matched case-insensitively as substrings.
    pub one_off_income_keywords: Vec<String>,
    /// Credit descriptions marking the last payment of a stream (S3.4: "Final employer payroll").
    pub income_end_keywords: Vec<String>,
    /// Settled rows whose amount came from an image are left out of stream detection (S5:
    /// image_03 bulk grocery purchase excluded from the groceries estimator).
    #[serde(alias = "EXCLUDE_IMAGE_BULK_ONEOFF")]
    pub exclude_evidence_amounts_from_streams: bool,
    /// A "next salary is X" fact sets every later projected occurrence, not only the next
    /// one (S3.4 table: user_06 1,037.52 and user_08 1,422.85 monthly). [FIT]
    pub next_income_amount_persists: bool,
    /// An income stream whose next expected occurrence fell before rd has stopped (S3.4).
    pub stop_income_after_missed_occurrence: bool,
    /// [SCHEDULED_REPLACES_CYCLE] S3.4(c): a scheduled row replaces a monthly stream's
    /// projected occurrence of the same category/direction within the window, and a scheduled
    /// salary re-anchors the stream's day of month.
    #[serde(alias = "SCHEDULED_REPLACES_CYCLE")]
    pub scheduled_replaces_cycle: bool,
    pub scheduled_replacement_window_days: i64,
    /// [SEEDED_SALARY_STREAM] S3.4(b): a scheduled salary seeds/continues a monthly stream.
    #[serde(alias = "SEEDED_SALARY_STREAM")]
    pub seeded_salary_stream: bool,
    /// [FINAL_PAYROLL_STOPS_INCOME] S3.4(a).
    #[serde(alias = "FINAL_PAYROLL_STOPS_INCOME")]
    pub final_payroll_stops_income: bool,
    /// Interval-stream occurrences earlier than rd + this many days are skipped (S3.2: 2,
    /// i.e. rd and rd+1).
    #[serde(alias = "IV_SKIP_DAYS")]
    pub variable_skip_days: i64,

    // ---- plans (S1) ---------------------------------------------------------------------
    /// Plans finishing after desired_completion_date are dropped (S1.1 `pays[-1] <= due`).
    pub drop_late_plans: bool,
    /// Payments dated after the forecast horizon are ignored by the safety check. [GUESS]
    pub ignore_payments_after_horizon: bool,
    /// Maximum spending-change actions in one plan (problem statement: up to three).
    pub max_spending_changes: usize,
    /// [CHANGE_PREFERENCE] How change plans are ordered after "no changes". S1.3: fewest.
    #[serde(alias = "CHANGE_PREFERENCE")]
    pub change_preference: ChangePreference,
    /// [INSTALLMENT_LIMIT] S1.1 [FIT].
    #[serde(alias = "INSTALLMENT_LIMIT")]
    pub installment_limit: InstallmentLimit,
    /// [NOT_REC_TEMPLATE] S1.6.
    #[serde(alias = "NOT_REC_TEMPLATE")]
    pub not_recommended_template: NotRecommendedTemplate,
    /// [AFFORDABLE_NOW_TEMPLATE] S1.6.
    #[serde(alias = "AFFORDABLE_NOW_TEMPLATE")]
    pub affordable_now_template: AffordableNowTemplate,
}

fn strings(xs: &[&str]) -> Vec<String> {
    xs.iter().map(|s| s.to_string()).collect()
}

impl Default for Rules {
    fn default() -> Self {
        Rules {
            horizon: Horizon::EndOfMonthPlus(2),
            variable_horizon: None,
            same_day_order: DayOrder::DebitsFirst,
            payment_timing: PaymentTiming::AfterDayRows,
            reserve_pending_on_request_date: false,
            ignore_scheduled_before_request_date: true,
            min_stream_occurrences: 3,
            monthly_gap_days: (28, 31),
            bill_estimator: AmountEstimator::MeanOfLast(3),
            variable_estimator: AmountEstimator::MeanAll,
            income_estimator: AmountEstimator::Last,
            one_off_income_keywords: strings(&[
                "bonus", "commission", "arrears", "incentive", "payout", "earnings", "reimburse", "prize", "lottery",
                "refund", "windfall",
            ]),
            income_end_keywords: strings(&["final"]),
            exclude_evidence_amounts_from_streams: true,
            next_income_amount_persists: true,
            stop_income_after_missed_occurrence: true,
            scheduled_replaces_cycle: true,
            scheduled_replacement_window_days: 15,
            seeded_salary_stream: true,
            final_payroll_stops_income: true,
            variable_skip_days: 2,
            drop_late_plans: true,
            ignore_payments_after_horizon: true,
            max_spending_changes: 3,
            change_preference: ChangePreference::FewestChanges,
            installment_limit: InstallmentLimit::NPayments,
            not_recommended_template: NotRecommendedTemplate::BIffPartialOnly,
            affordable_now_template: AffordableNowTemplate::LeavesAtLeast,
        }
    }
}

impl Rules {
    pub fn horizon_end(&self, rd: NaiveDate) -> NaiveDate {
        self.horizon.end(rd)
    }

    /// Rounds the closed-form safe amount for output: half-up to 2 dp, nothing coarser
    /// (RULES S1.5 rounding rule). Plan checks allow `forecast::SAFETY_TOLERANCE` for it.
    pub fn round_safe_amount(&self, raw: Money) -> Money {
        raw.round_to_cent()
    }

    /// Months an installment option spans, compared with `max_installment_months`.
    pub fn installment_months(&self, option: &PaymentOption) -> u32 {
        match self.installment_limit {
            InstallmentLimit::NPayments => option.number_of_payments,
            InstallmentLimit::CeilMonths => {
                let days = option.number_of_payments * option.payment_frequency_days.unwrap_or(30);
                days.div_ceil(30)
            }
        }
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
    fn json_patch_accepts_rules_s0_names() {
        let patch = r#"{"HORIZON":{"Days":91},"VAR_HORIZON":{"Days":91},"IV_SKIP_DAYS":0,
            "VAR_ESTIMATOR":{"MaxOfLast":4},"BILL_ESTIMATOR":"mid_all","SAME_DAY_ORDER":"credits_first",
            "PAYMENT_TIMING":"before_credits","SEEDED_SALARY_STREAM":false,"INSTALLMENT_LIMIT":"ceil_months",
            "CHANGE_PREFERENCE":"smallest_cut","NOT_REC_TEMPLATE":"always_A","AFFORDABLE_NOW_TEMPLATE":"keeps_minimum"}"#;
        let r: Rules = serde_json::from_str(patch).unwrap();
        assert_eq!(r.horizon, Horizon::Days(91));
        assert_eq!(r.variable_skip_days, 0);
        assert_eq!(r.bill_estimator, AmountEstimator::MidAll);
        assert_eq!(r.change_preference, ChangePreference::SmallestCut);
        assert!(!r.seeded_salary_stream);
        // Unpatched fields keep their defaults.
        assert_eq!(r.scheduled_replacement_window_days, 15);
        let empty: Rules = serde_json::from_str("{}").unwrap();
        assert_eq!(empty, Rules::default());
    }

    #[test]
    fn horizon_end_of_month_plus_two() {
        let d = |s: &str| NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap();
        assert_eq!(Horizon::EndOfMonthPlus(2).end(d("2025-02-07")), d("2025-04-30"));
        assert_eq!(Horizon::EndOfMonthPlus(2).end(d("2025-11-06")), d("2026-01-31"));
        assert_eq!(Horizon::EndOfMonthPlus(2).end(d("2023-12-31")), d("2024-02-29"));
        assert_eq!(Horizon::Days(91).end(d("2025-02-07")), d("2025-05-08"));
    }
}
