//! `DecisionFacts` (PLAN.md §2.10): every number the recommendation rests on, recorded per
//! request. The explanation is rendered only from these, so it cannot contradict the row.

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

use super::money::Cents;
use super::plans::{DropReason, RankKey};
use super::types::{AffordabilityStatus, Payment, PaymentMethod};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CandidateFact {
    pub label: String,
    pub method: PaymentMethod,
    pub option_id: Option<String>,
    pub total_paid: Cents,
    pub payments: Vec<Payment>,
    pub changes: Vec<String>,
    pub outcome: CandidateOutcome,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum CandidateOutcome {
    Survived { rank: usize, key: RankKey, trough_balance: Cents, trough_date: NaiveDate },
    Dropped(DropReason),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChangeFact {
    pub rendered: String,
    pub event_id: String,
    pub description: String,
    pub category: String,
    pub stop: bool,
    pub new_amount: Option<Cents>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DecisionFacts {
    pub request_id: String,
    pub request_date: NaiveDate,
    pub currency: String,
    pub requested_amount: Cents,
    pub desired_completion_date: NaiveDate,
    pub allows_partial_payment: bool,

    // ---- position ----------------------------------------------------------------------
    pub starting_balance: Cents,
    pub minimum_balance: Cents,
    pub reserved_pending_total: Cents,
    pub reserved_event_ids: Vec<String>,
    /// Lowest projected balance with no payment, and its first date.
    pub trough_balance: Cents,
    pub trough_date: NaiveDate,
    /// `trough_balance - minimum_balance` (may be negative).
    pub headroom: Cents,
    pub horizon_end: NaiveDate,

    // ---- the two hard numbers ----------------------------------------------------------
    pub raw_safe_amount: Cents,
    pub safe_amount: Cents,
    pub earliest_full_date: Option<NaiveDate>,

    // ---- search ------------------------------------------------------------------------
    pub candidates: Vec<CandidateFact>,
    pub searched_spending_changes: bool,
    pub winning_key: Option<RankKey>,

    // ---- decision ----------------------------------------------------------------------
    pub status: AffordabilityStatus,
    pub method: PaymentMethod,
    pub plan: Vec<Payment>,
    pub option_id: Option<String>,
    pub changes: Vec<ChangeFact>,
    /// Trough with the chosen plan (and changes) applied.
    pub plan_trough: Option<(Cents, NaiveDate)>,

    // ---- data quality ------------------------------------------------------------------
    pub ledger_issues: Vec<String>,
    pub rejected_evidence: Vec<String>,
    pub applied_evidence: Vec<String>,
}
