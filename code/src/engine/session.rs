//! User-bound session (PLAN.md §2.5).
//!
//! A `Session` is constructed around exactly one user. The user id is bound at construction
//! and never accepted again: evidence ingestion and `decide` take no user argument, and the
//! evidence types carry none, so a model output cannot address another user's data.

use std::sync::Arc;

use anyhow::{bail, ensure, Result};
use chrono::NaiveDate;

use super::explain;
use super::facts::{CandidateFact, CandidateOutcome, ChangeFact, DecisionFacts};
use super::forecast::{Forecast, ForecastInputs, SpendingChange};
use super::ledger::{EvidenceRecord, Ledger, LedgerIssue};
use super::money::Cents;
use super::plans::{self, Outcome, PlanContext};
use super::recurrence::{self, Streams};
use super::rules::Rules;
use super::types::{AffordabilityStatus, Event, PaymentMethod, PaymentOption, Profile, RateProvider, RateTable, RequestSpec};
use crate::model;

pub struct Session {
    user_id: String,
    profile: Profile,
    events: Vec<Event>,
    evidence: Vec<EvidenceRecord>,
    ledger: Ledger,
    rates: Arc<RateTable>,
    rules: Rules,
}

/// The engine's answer for one request.
#[derive(Clone, Debug)]
pub struct Decision {
    pub row: model::OutputRow,
    pub facts: DecisionFacts,
    pub streams: Streams,
    /// Projection with no payment and no spending changes.
    pub baseline: Forecast,
    /// Projection with the recommended spending changes applied, when there are any.
    pub with_changes: Option<Forecast>,
}

impl Decision {
    /// Daily balances in currency units, for `evaluation::replay::ForecastSeries::baseline`.
    pub fn baseline_series(&self) -> Vec<f64> {
        to_f64(&self.baseline.balance)
    }

    /// Daily balances with changes, for `ForecastSeries::with_changes`.
    pub fn with_changes_series(&self) -> Option<Vec<f64>> {
        self.with_changes.as_ref().map(|f| to_f64(&f.balance))
    }

    pub fn minimum_f64(&self) -> f64 {
        self.baseline.minimum_balance.0 as f64 / 100.0
    }
}

fn to_f64(v: &[Cents]) -> Vec<f64> {
    v.iter().map(|c| c.0 as f64 / 100.0).collect()
}

impl Session {
    pub fn open(user_id: impl Into<String>, profile: Profile, events: Vec<Event>, rates: Arc<RateTable>, rules: Rules) -> Session {
        let ledger = Ledger::build(&profile.home_currency, &events, &[], rates.as_ref(), &rules);
        Session { user_id: user_id.into(), profile, events, evidence: Vec::new(), ledger, rates, rules }
    }

    /// Batch adapter: bind the session to `user_id` and take only that user's rows.
    pub fn from_model(
        user_id: &str,
        profiles: &[model::FinancialProfile],
        events: &[model::FinancialEvent],
        rates: Arc<RateTable>,
        rules: Rules,
    ) -> Result<Session> {
        let Some(p) = profiles.iter().find(|p| p.user_id == user_id) else {
            bail!("no financial profile for {user_id}");
        };
        let profile = Profile::from_model(p)?;
        let events = events
            .iter()
            .filter(|e| e.user_id == user_id)
            .map(Event::from_model)
            .collect::<Result<Vec<_>>>()?;
        Ok(Session::open(user_id, profile, events, rates, rules))
    }

    pub fn user_id(&self) -> &str {
        &self.user_id
    }

    pub fn profile(&self) -> &Profile {
        &self.profile
    }

    pub fn ledger(&self) -> &Ledger {
        &self.ledger
    }

    /// Model-facing ingestion: validated evidence records (no user id anywhere) amend this
    /// session's ledger. Rebuilds from rows so application order stays deterministic.
    pub fn apply_evidence(&mut self, records: Vec<EvidenceRecord>) {
        self.evidence.extend(records);
        self.ledger = Ledger::build(&self.profile.home_currency, &self.events, &self.evidence, self.rates.as_ref(), &self.rules);
    }

    pub fn streams(&self, as_of: NaiveDate) -> Streams {
        recurrence::detect(&self.ledger, as_of, &self.rules)
    }

    /// Decide one request of this session's user. `options` are that request's payment options.
    pub fn decide(&self, request_id: &str, request_date: NaiveDate, spec: &RequestSpec, options: &[PaymentOption]) -> Result<Decision> {
        ensure!(spec.amount > Cents::ZERO, "{request_id}: non-positive requested amount");
        let rules = &self.rules;
        let profile = &self.profile;
        let streams = self.streams(request_date);
        let rates: &dyn RateProvider = self.rates.as_ref();
        let inputs = ForecastInputs {
            ledger: &self.ledger,
            streams: &streams,
            opening_balance: profile.current_available_balance,
            minimum_balance: profile.minimum_balance_to_keep,
            start: request_date,
            rates,
            rules,
        };
        let baseline = Forecast::build(&inputs, &[]);
        let raw_safe = baseline.raw_safe_amount();
        let safe = baseline.safe_amount(spec.amount, rules);
        let earliest = baseline.earliest_full_date(spec.amount);

        let ctx = PlanContext {
            profile,
            spec,
            request_date,
            options,
            baseline: &baseline,
            safe_amount: safe,
            earliest_full_date: earliest,
            streams: &streams,
            forecast_inputs: &inputs,
            rules,
        };
        let result = plans::search(&ctx);
        let actions = plans::eligible_actions(&ctx);

        let (status, method, plan, option_id, changes, winning_key, plan_trough) = match result.winner() {
            None => (AffordabilityStatus::NotAffordable, PaymentMethod::NotRecommended, vec![], None, vec![], None, None),
            Some((c, key, safety)) => {
                let status = match (c.method, c.changes.is_empty()) {
                    (_, false) => AffordabilityStatus::AffordableWithPlan,
                    (PaymentMethod::FullPayment, true) => AffordabilityStatus::AffordableNow,
                    (PaymentMethod::Installments | PaymentMethod::PartialPayment, true) => AffordabilityStatus::AffordableWithPlan,
                    (PaymentMethod::Wait, true) => AffordabilityStatus::AffordableLater,
                    (PaymentMethod::NotRecommended, _) => unreachable!("never a candidate"),
                };
                (
                    status,
                    c.method,
                    c.payments.clone(),
                    c.option_id.clone(),
                    c.changes.clone(),
                    Some(key.clone()),
                    Some((safety.trough_balance, safety.trough_date)),
                )
            }
        };
        let with_changes = (!changes.is_empty()).then(|| Forecast::build(&inputs, &changes));

        let change_facts = changes
            .iter()
            .map(|ch| {
                let a = actions.iter().find(|a| &a.change == ch);
                ChangeFact {
                    rendered: ch.render(),
                    event_id: ch.event_id().to_string(),
                    description: a.map(|a| a.description.clone()).unwrap_or_default(),
                    category: a.map(|a| a.category.clone()).unwrap_or_default(),
                    stop: matches!(ch, SpendingChange::Stop { .. }),
                    new_amount: match ch {
                        SpendingChange::ReduceTo { amount, .. } => Some(*amount),
                        SpendingChange::Stop { .. } => None,
                    },
                }
            })
            .collect();

        let mut rank = 0;
        let candidates = result
            .evaluated
            .iter()
            .map(|e| CandidateFact {
                label: e.candidate.label(),
                method: e.candidate.method,
                option_id: e.candidate.option_id.clone(),
                total_paid: e.candidate.total_paid,
                payments: e.candidate.payments.clone(),
                changes: e.candidate.changes.iter().map(|c| c.render()).collect(),
                outcome: match &e.outcome {
                    Outcome::Survived { key, safety } => {
                        rank += 1;
                        CandidateOutcome::Survived {
                            rank,
                            key: key.clone(),
                            trough_balance: safety.trough_balance,
                            trough_date: safety.trough_date,
                        }
                    }
                    Outcome::Dropped(r) => CandidateOutcome::Dropped(r.clone()),
                },
            })
            .collect();

        let (trough_balance, trough_date) = baseline.trough();
        let facts = DecisionFacts {
            request_id: request_id.to_string(),
            request_date,
            currency: profile.home_currency.clone(),
            requested_amount: spec.amount,
            desired_completion_date: spec.deadline,
            allows_partial_payment: spec.allows_partial_payment,
            starting_balance: profile.current_available_balance,
            minimum_balance: profile.minimum_balance_to_keep,
            reserved_pending_total: baseline.reserved_total,
            reserved_event_ids: self.ledger.reserved().map(|e| e.event.id.clone()).collect(),
            trough_balance,
            trough_date,
            headroom: trough_balance - profile.minimum_balance_to_keep,
            horizon_end: baseline.horizon_end(),
            raw_safe_amount: raw_safe,
            safe_amount: safe,
            earliest_full_date: earliest,
            candidates,
            searched_spending_changes: result.searched_changes,
            winning_key,
            status,
            method,
            plan: plan.clone(),
            option_id,
            changes: change_facts,
            plan_trough,
            ledger_issues: self.ledger.issues.iter().map(issue_text).collect(),
            rejected_evidence: self.ledger.rejected.iter().map(|r| format!("{}: {}", r.record_id, r.reason)).collect(),
            applied_evidence: self
                .ledger
                .entries
                .iter()
                .flat_map(|e| e.applied_evidence.iter().cloned())
                .chain(self.ledger.adjustments.iter().map(|a| a.record_id.clone()))
                .collect(),
        };

        let row = model::OutputRow {
            request_id: request_id.to_string(),
            amount_safe_to_pay: safe.fmt_plain(),
            affordability_status: status.as_str().to_string(),
            recommended_payment_method: method.as_str().to_string(),
            payment_plan: render_plan(&plan),
            earliest_date_for_full_payment: earliest.map(|d| d.format("%Y-%m-%d").to_string()).unwrap_or_default(),
            spending_changes_needed: render_changes(&changes),
            decision_explanation: explain::render(&facts),
        };
        self_check(&facts, &row)?;
        Ok(Decision { row, facts, streams, baseline, with_changes })
    }
}

pub fn render_plan(plan: &[super::types::Payment]) -> String {
    if plan.is_empty() {
        return "none".into();
    }
    plan.iter().map(|p| format!("{}:{}", p.date.format("%Y-%m-%d"), p.amount.fmt_plan())).collect::<Vec<_>>().join("|")
}

pub fn render_changes(changes: &[SpendingChange]) -> String {
    if changes.is_empty() {
        return "none".into();
    }
    changes.iter().map(|c| c.render()).collect::<Vec<_>>().join("|")
}

fn issue_text(i: &LedgerIssue) -> String {
    match i {
        LedgerIssue::MissingAmount { event_id } => format!("missing amount: {event_id}"),
        LedgerIssue::MissingRate { event_id, date, from, to } => format!("missing rate {from}->{to} on {date}: {event_id}"),
    }
}

/// Engine-side hard checks that facts and row agree (the verifier runs the full §2.10 set).
fn self_check(f: &DecisionFacts, row: &model::OutputRow) -> Result<()> {
    let id = &f.request_id;
    ensure!(f.safe_amount >= Cents::ZERO && f.safe_amount <= f.requested_amount, "{id}: safe amount out of range");
    if f.status == AffordabilityStatus::AffordableNow {
        ensure!(f.earliest_full_date == Some(f.request_date), "{id}: affordable_now without earliest == request_date");
        ensure!(f.method == PaymentMethod::FullPayment, "{id}: affordable_now must be full_payment");
    }
    ensure!(f.plan.windows(2).all(|w| w[0].date <= w[1].date), "{id}: plan not chronological");
    match f.method {
        PaymentMethod::FullPayment | PaymentMethod::Wait | PaymentMethod::PartialPayment => {
            let total: Cents = f.plan.iter().map(|p| p.amount).sum();
            ensure!(total == f.requested_amount, "{id}: plan sums to {total}, not the requested amount");
        }
        PaymentMethod::NotRecommended => ensure!(row.payment_plan == "none", "{id}: not_recommended with a plan"),
        _ => {}
    }
    if f.method == PaymentMethod::PartialPayment {
        ensure!(f.plan.len() == 2 && f.plan[0].amount == f.safe_amount && f.plan[0].date == f.request_date, "{id}: bad partial split");
        ensure!(Some(f.plan[1].date) == f.earliest_full_date, "{id}: partial remainder not on earliest date");
    }
    ensure!(f.changes.len() <= 3, "{id}: more than three spending changes");
    Ok(())
}
