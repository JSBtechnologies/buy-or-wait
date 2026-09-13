//! Ledger reconstruction (PLAN.md §2.1): cash rules by status/direction, lifecycle de-dup,
//! evidence amendments with the spec's conflict order, and home-currency conversion.
//!
//! The ledger is a durable data model: it is `Serialize`/`Deserialize` so the store can
//! persist it per user (§2.11), and every entry records *why* it does or does not move cash.

use std::collections::{BTreeMap, HashMap};

use chrono::{NaiveDate, NaiveDateTime};
use serde::{Deserialize, Serialize};

use super::money::Money;
use super::rules::Rules;
use super::types::{Direction, Event, EventType, RateProvider, Status};

// ---------------------------------------------------------------------------------------
// Evidence: typed facts extracted from messages/images. The extraction module converts its
// validated records into these; nothing here carries a user id or free text instructions.
// ---------------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum EvidenceSource {
    /// `source_type` from messages.csv (employer, bank, merchant, ...).
    Message { source_type: String },
    Image,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EvidenceRecord {
    /// Stable provenance id, e.g. `message_02#1` or `image_01`.
    pub record_id: String,
    pub source: EvidenceSource,
    /// When the evidence was produced (`sent_at`); images use their event's date.
    pub observed_at: NaiveDateTime,
    pub fact: Fact,
}

/// The closed set of facts evidence may assert. There is deliberately no variant that can
/// express a decision, an instruction, or another user.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Fact {
    // ---- about one supplied event row --------------------------------------------------
    /// The figure for a row with a blank amount (image selector output).
    EventAmount { event_id: String, amount: Money, currency: String },
    /// The row's image failed closed, but at least one validated read selected a figure
    /// (`amount` = the largest). Reserved against a pending/scheduled debit only; never fills
    /// the row's amount, which stays missing.
    UnverifiedEventAmount { event_id: String, amount: Money, currency: String, reason: String },
    /// Witness detail for an accepted image figure: the printed final amount (`accepted`, the
    /// truth) next to the recomputed one (`computed`), e.g. 8,528 paid vs 8,528.10 summed.
    AmountWitness { event_id: String, accepted: Money, computed: Money, currency: String, witness: String },
    /// The row's transaction was explicitly cancelled / reversed.
    EventCancelled { event_id: String },
    /// The row explicitly settled (optionally with its final amount / date).
    EventSettled { event_id: String, amount: Option<Money>, date: Option<NaiveDate> },
    /// The row was explicitly amended (new amount and/or new cash date).
    EventAmended { event_id: String, amount: Option<Money>, date: Option<NaiveDate> },
    /// The row duplicates another (e.g. pending copy of a settled charge).
    DuplicateOf { event_id: String, of_event_id: Option<String> },
    /// The row is a transfer between the user's own accounts.
    OwnAccountTransfer { event_id: String },

    // ---- about the forecast (no one-to-one event row) ----------------------------------
    /// Recurring income amount changes from `effective` onward.
    IncomeAmountChange { category: String, amount: Money, currency: String, effective: NaiveDate },
    /// Income starts or resumes (first salary, pay resuming after leave): monthly on
    /// `first_date`'s day from `first_date`, replacing any projected income of the category.
    IncomeStarts { category: String, amount: Money, currency: String, first_date: NaiveDate },
    /// Only the next income occurrence has a different amount.
    NextIncomeAmount { category: String, amount: Money, currency: String, date: Option<NaiveDate> },
    /// The next income occurrence moves to `new_date`.
    IncomeDateMoved { category: String, new_date: NaiveDate },
    /// Recurring income stops from `effective` onward.
    IncomeEnded {
        category: String,
        effective: NaiveDate,
        /// Stream selector (RULES S6.2, lead-approved additive): only income streams whose
        /// description matches (case-insensitive substring either way) end, e.g. "Second
        /// household income". `None` ends every income stream of the category.
        #[serde(default)]
        description: Option<String>,
    },
    /// A confirmed new recurring expense.
    NewRecurringExpense {
        description: String,
        category: String,
        amount: Money,
        currency: String,
        first_date: NaiveDate,
        /// `None` means monthly on `first_date`'s day of month.
        every_days: Option<u32>,
    },
    /// An existing recurring expense changes from `effective` onward, either to a new
    /// amount or by a percentage (e.g. rent +12% on renewal). Exactly one of amount/percent.
    ExpenseAmountChange {
        category: String,
        amount: Option<Money>,
        percent: Option<f64>,
        currency: Option<String>,
        /// `None`: from the stream's next occurrence after the evidence was sent.
        effective: Option<NaiveDate>,
    },
    /// A confirmed one-time cash flow (e.g. arrears payment, one-off bill).
    OneTimeFlow { direction: Direction, category: String, amount: Money, currency: String, date: NaiveDate },
    /// Income/credit that is announced but not confirmed (bonus, commission, prize, refund,
    /// payout). Recorded for the explanation facts; never counted.
    Unconfirmed { category: String, amount: Option<Money>, currency: Option<String> },
}

impl Fact {
    pub fn event_id(&self) -> Option<&str> {
        match self {
            Fact::EventAmount { event_id, .. }
            | Fact::UnverifiedEventAmount { event_id, .. }
            | Fact::AmountWitness { event_id, .. }
            | Fact::EventCancelled { event_id }
            | Fact::EventSettled { event_id, .. }
            | Fact::EventAmended { event_id, .. }
            | Fact::DuplicateOf { event_id, .. }
            | Fact::OwnAccountTransfer { event_id } => Some(event_id),
            _ => None,
        }
    }

    /// Image side-facts (unverified reserve, witness totals) that never set a row's amount.
    fn is_image_note(&self) -> bool {
        matches!(self, Fact::UnverifiedEventAmount { .. } | Fact::AmountWitness { .. })
    }

    /// Conflict rule 1: explicit cancellation, settlement, or amendment wins.
    fn is_explicit(&self) -> bool {
        matches!(self, Fact::EventCancelled { .. } | Fact::EventSettled { .. } | Fact::EventAmended { .. })
    }
}

// ---------------------------------------------------------------------------------------
// Ledger entries
// ---------------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CashTreatment {
    /// Settled history: already reflected in `current_available_balance`; used for
    /// recurrence detection only.
    Settled,
    /// Pending debit: reserved as money already gone.
    Reserved,
    /// A future cash flow on its cash date (scheduled rows, evidence-settled credits).
    Scheduled,
    /// Does not move cash.
    Excluded(ExclusionReason),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExclusionReason {
    Cancelled,
    Failed,
    /// Pending refunds/bonuses/payouts never count until they settle.
    PendingCredit,
    /// Unrealized investment valuations.
    NonCash,
    /// An earlier row of the same lifecycle; only the terminal row counts.
    SupersededBy(String),
    CancelledByEvidence(String),
    DuplicateOf { of: Option<String>, record: String },
    OwnAccountTransfer(String),
}

impl CashTreatment {
    pub fn moves_cash(&self) -> bool {
        !matches!(self, CashTreatment::Excluded(_))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AmountSource {
    Row,
    Evidence(String),
    /// Blank row with no accepted evidence figure: never treated as zero.
    Missing,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LedgerEntry {
    pub event: Event,
    pub treatment: CashTreatment,
    /// Date the cash moves (settlement date, possibly amended).
    pub cash_date: NaiveDate,
    /// Amount in the event currency after amendments.
    pub amount: Option<Money>,
    pub amount_source: AmountSource,
    /// Amount converted to the user's home currency (positive magnitude).
    pub home_amount: Option<Money>,
    /// Evidence record ids applied to this entry, in application order.
    pub applied_evidence: Vec<String>,
    /// Root event of the linked chain this row belongs to (authorization -> settlement,
    /// failed -> retry, charge -> refund, purchase -> valuation). Chain rows are one
    /// transaction and never feed stream detection.
    pub chain_root: Option<String>,
}

impl LedgerEntry {
    /// Signed home-currency cash effect: credits positive, debits negative.
    pub fn signed_home_amount(&self) -> Option<Money> {
        let a = self.home_amount?;
        match self.event.direction {
            Direction::Credit => Some(a),
            Direction::Debit => Some(-a),
            Direction::NonCash => Some(Money::ZERO),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum LedgerIssue {
    MissingAmount { event_id: String },
    MissingRate { event_id: String, date: NaiveDate, from: String, to: String },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RejectedEvidence {
    pub record_id: String,
    pub reason: String,
}

/// A failed-closed image figure held against a blank pending/scheduled debit (image accuracy
/// plan §4). The entry itself stays `AmountSource::Missing`; only the forecast reads this.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UnverifiedReserve {
    pub event_id: String,
    pub record_id: String,
    /// Event currency.
    pub amount: Money,
    pub currency: String,
    pub home_amount: Money,
    pub reason: String,
}

/// Printed final amount next to its recomputed witness for an accepted image figure.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AmountWitnessRecord {
    pub event_id: String,
    pub record_id: String,
    pub accepted: Money,
    pub computed: Money,
    pub currency: String,
    pub witness: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Ledger {
    pub home_currency: String,
    pub entries: Vec<LedgerEntry>,
    /// Validated forecast-level facts (no event row), consumed by recurrence/forecast.
    pub adjustments: Vec<EvidenceRecord>,
    pub rejected: Vec<RejectedEvidence>,
    pub issues: Vec<LedgerIssue>,
    /// Failed-closed image figures reserved per event id (largest validated read).
    #[serde(default)]
    pub unverified: BTreeMap<String, UnverifiedReserve>,
    /// Witness totals for accepted image figures, ordered by event id then record id.
    #[serde(default)]
    pub witnesses: Vec<AmountWitnessRecord>,
    #[serde(skip)]
    index: HashMap<String, usize>,
}

impl Ledger {
    /// Rebuild a user's ledger from their event rows plus validated evidence.
    pub fn build(
        home_currency: &str,
        events: &[Event],
        evidence: &[EvidenceRecord],
        rates: &dyn RateProvider,
        _rules: &Rules,
    ) -> Ledger {
        let mut ledger = Ledger { home_currency: home_currency.to_string(), ..Default::default() };
        let mut sorted: Vec<&Event> = events.iter().collect();
        sorted.sort_by(|a, b| (a.event_date, id_num(&a.id)).cmp(&(b.event_date, id_num(&b.id))));
        for e in sorted {
            ledger.entries.push(LedgerEntry {
                treatment: base_treatment(e),
                cash_date: e.cash_date(),
                amount: e.amount,
                amount_source: if e.amount.is_some() { AmountSource::Row } else { AmountSource::Missing },
                home_amount: None,
                applied_evidence: Vec::new(),
                chain_root: None,
                event: e.clone(),
            });
        }
        ledger.reindex();
        ledger.resolve_lifecycles();
        ledger.apply_evidence(evidence, rates);
        ledger.convert_amounts(rates);
        ledger.apply_image_notes(evidence, rates);
        ledger
    }

    /// Home-currency reserve for a failed-closed image figure on this event, if any.
    pub fn unverified_home(&self, event_id: &str) -> Option<Money> {
        self.unverified.get(event_id).map(|r| r.home_amount)
    }

    pub fn reindex(&mut self) {
        self.index = self.entries.iter().enumerate().map(|(i, e)| (e.event.id.clone(), i)).collect();
    }

    pub fn get(&self, event_id: &str) -> Option<&LedgerEntry> {
        self.index.get(event_id).map(|&i| &self.entries[i])
    }

    fn get_mut(&mut self, event_id: &str) -> Option<&mut LedgerEntry> {
        let i = *self.index.get(event_id)?;
        Some(&mut self.entries[i])
    }

    /// Settled history strictly before `as_of`, for recurrence detection.
    pub fn settled_history(&self, as_of: NaiveDate) -> impl Iterator<Item = &LedgerEntry> {
        self.entries
            .iter()
            .filter(move |e| e.treatment == CashTreatment::Settled && e.event.event_date < as_of)
    }

    /// Pending debits held against the balance.
    pub fn reserved(&self) -> impl Iterator<Item = &LedgerEntry> {
        self.entries.iter().filter(|e| e.treatment == CashTreatment::Reserved)
    }

    /// Future flows (scheduled rows and evidence-settled credits).
    pub fn scheduled(&self) -> impl Iterator<Item = &LedgerEntry> {
        self.entries.iter().filter(|e| e.treatment == CashTreatment::Scheduled)
    }

    /// Authorization -> settlement and failed -> retry: when a later row continues an earlier
    /// row's lifecycle, only the terminal row counts. Refunds and investment rows link to
    /// their purchase but are separate cash movements, so they never supersede.
    fn resolve_lifecycles(&mut self) {
        let links: Vec<(usize, String)> = self
            .entries
            .iter()
            .enumerate()
            .filter_map(|(i, e)| e.event.linked_event_id.clone().map(|l| (i, l)))
            .collect();
        for (succ_idx, pred_id) in links {
            let succ = &self.entries[succ_idx];
            if !is_lifecycle_continuation(&succ.event) || !succ.treatment.moves_cash() {
                continue;
            }
            let succ_id = succ.event.id.clone();
            let succ_dir = succ.event.direction;
            if let Some(pred) = self.get_mut(&pred_id) {
                // Settled is terminal: a later pending "possible duplicate" never erases it.
                if pred.event.direction == succ_dir
                    && pred.event.status != Status::Settled
                    && pred.treatment.moves_cash()
                {
                    pred.treatment = CashTreatment::Excluded(ExclusionReason::SupersededBy(succ_id));
                }
            }
        }
        // RULES S2.1: a linked chain is one transaction; every row in it (root included) is
        // excluded from recurrence detection. Record each row's chain root.
        let parent: HashMap<String, String> = self
            .entries
            .iter()
            .filter_map(|e| e.event.linked_event_id.clone().map(|l| (e.event.id.clone(), l)))
            .collect();
        let root_of = |id: &str| {
            let mut cur = id.to_string();
            let mut steps = 0;
            while let Some(p) = parent.get(&cur) {
                cur = p.clone();
                steps += 1;
                if steps > parent.len() {
                    break; // malformed cycle: stop deterministically
                }
            }
            cur
        };
        let roots: Vec<(String, String)> =
            parent.keys().chain(parent.values()).map(|id| (id.clone(), root_of(id))).collect();
        for (id, root) in roots {
            if let Some(e) = self.get_mut(&id) {
                e.chain_root = Some(root);
            }
        }
    }

    /// Apply event-level facts per the conflict order: explicit cancellation/settlement/
    /// amendment beats anything else; among equals the newer record wins (applied last).
    /// Forecast-level facts are kept as adjustments.
    fn apply_evidence(&mut self, evidence: &[EvidenceRecord], rates: &dyn RateProvider) {
        let mut ordered: Vec<&EvidenceRecord> = evidence.iter().collect();
        ordered.sort_by(|a, b| {
            (a.fact.is_explicit(), a.observed_at, &a.record_id)
                .cmp(&(b.fact.is_explicit(), b.observed_at, &b.record_id))
        });
        for rec in ordered {
            if rec.fact.is_image_note() {
                continue; // applied after amounts settle, in `apply_image_notes`
            }
            let Some(event_id) = rec.fact.event_id() else {
                // Accuracy first (board decision.accuracy_first): a forecast-level fact that
                // cannot be applied exactly is rejected and recorded, never half-applied.
                match validate_adjustment(&rec.fact, &self.home_currency, rates) {
                    Ok(()) => self.adjustments.push(rec.clone()),
                    Err(reason) => self.reject(rec, reason),
                }
                continue;
            };
            let Some(i) = self.index.get(event_id).copied() else {
                self.reject(rec, format!("unknown event {event_id}"));
                continue;
            };
            if let Err(reason) = apply_fact(&mut self.entries[i], rec) {
                self.reject(rec, reason);
            }
        }
    }

    /// Unverified reserves and amount witnesses, applied once every cancellation, amendment,
    /// verified figure and conversion is final, in record-id order (deterministic).
    fn apply_image_notes(&mut self, evidence: &[EvidenceRecord], rates: &dyn RateProvider) {
        let mut notes: Vec<&EvidenceRecord> = evidence.iter().filter(|r| r.fact.is_image_note()).collect();
        notes.sort_by(|a, b| a.record_id.cmp(&b.record_id));
        for rec in notes {
            let event_id = rec.fact.event_id().unwrap_or_default();
            let Some(i) = self.index.get(event_id).copied() else {
                self.reject(rec, format!("unknown event {event_id}"));
                continue;
            };
            let result = match &rec.fact {
                Fact::UnverifiedEventAmount { amount, currency, reason, .. } => {
                    self.reserve_unverified(i, rec, *amount, currency, reason, rates)
                }
                Fact::AmountWitness { accepted, computed, currency, witness, .. } => {
                    self.record_witness(i, rec, *accepted, *computed, currency, witness)
                }
                _ => Err("not an image note".into()),
            };
            if let Err(reason) = result {
                self.reject(rec, reason);
            }
        }
        self.witnesses.sort_by(|a, b| (&a.event_id, &a.record_id).cmp(&(&b.event_id, &b.record_id)));
    }

    fn reserve_unverified(
        &mut self,
        i: usize,
        rec: &EvidenceRecord,
        amount: Money,
        currency: &str,
        reason: &str,
        rates: &dyn RateProvider,
    ) -> Result<(), String> {
        let e = &self.entries[i];
        if e.event.amount.is_some() || e.amount.is_some() {
            return Err("row already has an amount".into());
        }
        if e.event.direction != Direction::Debit {
            return Err("unverified figure only reserves debits".into());
        }
        if !matches!(e.treatment, CashTreatment::Reserved | CashTreatment::Scheduled) {
            return Err(format!("unverified figure only reserves pending/scheduled rows, treatment {:?}", e.treatment));
        }
        if !same_currency(currency, &e.event.currency) {
            return Err(currency_mismatch(currency, &e.event.currency));
        }
        if amount <= Money::ZERO {
            return Err("non-positive figure".into());
        }
        let Some(home_amount) = to_home(amount, &e.event.currency, &self.home_currency, e.cash_date, rates) else {
            return Err(format!("no {}->{} rate on {}", e.event.currency, self.home_currency, e.cash_date));
        };
        let reserve = UnverifiedReserve {
            event_id: e.event.id.clone(),
            record_id: rec.record_id.clone(),
            amount,
            currency: e.event.currency.clone(),
            home_amount,
            reason: reason.to_string(),
        };
        // Largest figure wins; on a tie the earlier record id (already held) stays.
        match self.unverified.get(&reserve.event_id) {
            Some(held) if held.amount >= amount => Err(format!("superseded by larger unverified figure {}", held.record_id)),
            Some(held) => {
                let loser = RejectedEvidence {
                    record_id: held.record_id.clone(),
                    reason: format!("superseded by larger unverified figure {}", rec.record_id),
                };
                self.rejected.push(loser);
                self.unverified.insert(reserve.event_id.clone(), reserve);
                Ok(())
            }
            None => {
                self.unverified.insert(reserve.event_id.clone(), reserve);
                Ok(())
            }
        }
    }

    fn record_witness(
        &mut self,
        i: usize,
        rec: &EvidenceRecord,
        accepted: Money,
        computed: Money,
        currency: &str,
        witness: &str,
    ) -> Result<(), String> {
        let e = &self.entries[i];
        if !matches!(e.amount_source, AmountSource::Evidence(_)) {
            return Err("witness without an accepted image figure".into());
        }
        if e.amount != Some(accepted) {
            return Err(format!("witness accepted {accepted} != applied figure {:?}", e.amount));
        }
        if !same_currency(currency, &e.event.currency) {
            return Err(currency_mismatch(currency, &e.event.currency));
        }
        if computed <= Money::ZERO {
            return Err("non-positive computed total".into());
        }
        self.witnesses.push(AmountWitnessRecord {
            event_id: e.event.id.clone(),
            record_id: rec.record_id.clone(),
            accepted,
            computed,
            currency: e.event.currency.clone(),
            witness: witness.to_string(),
        });
        Ok(())
    }

    fn reject(&mut self, rec: &EvidenceRecord, reason: String) {
        self.rejected.push(RejectedEvidence { record_id: rec.record_id.clone(), reason });
    }

    fn convert_amounts(&mut self, rates: &dyn RateProvider) {
        let home = self.home_currency.clone();
        let mut issues = Vec::new();
        for e in &mut self.entries {
            let Some(amount) = e.amount else {
                if e.treatment.moves_cash() {
                    issues.push(LedgerIssue::MissingAmount { event_id: e.event.id.clone() });
                }
                continue;
            };
            match to_home(amount, &e.event.currency, &home, e.cash_date, rates) {
                Some(h) => e.home_amount = Some(h),
                None => issues.push(LedgerIssue::MissingRate {
                    event_id: e.event.id.clone(),
                    date: e.cash_date,
                    from: e.event.currency.clone(),
                    to: home.clone(),
                }),
            }
        }
        self.issues.extend(issues);
    }
}

/// Checks a forecast-level fact can be applied exactly: positive amounts, a sane percentage,
/// exactly one of amount/percent, a non-empty category, and a currency that converts to home.
pub fn validate_adjustment(fact: &Fact, home: &str, rates: &dyn RateProvider) -> Result<(), String> {
    let positive = |a: &Money| if *a > Money::ZERO { Ok(()) } else { Err(format!("non-positive amount {a}")) };
    let convertible = |c: &str| {
        let far = NaiveDate::from_ymd_opt(9999, 12, 31).expect("valid date");
        if c == home || rates.rate(far, c, home).is_some() || rates.rate(far, home, c).is_some() {
            Ok(())
        } else {
            Err(format!("no {c}->{home} rate"))
        }
    };
    let category = |c: &str| if c.trim().is_empty() { Err("empty category".to_string()) } else { Ok(()) };
    match fact {
        Fact::IncomeAmountChange { category: c, amount, currency, .. }
        | Fact::IncomeStarts { category: c, amount, currency, .. }
        | Fact::NextIncomeAmount { category: c, amount, currency, .. } => {
            category(c)?;
            positive(amount)?;
            convertible(currency)
        }
        Fact::NewRecurringExpense { category: c, amount, currency, every_days, .. } => {
            category(c)?;
            positive(amount)?;
            if *every_days == Some(0) {
                return Err("zero-day interval".into());
            }
            convertible(currency)
        }
        Fact::OneTimeFlow { direction, category: c, amount, currency, .. } => {
            category(c)?;
            if *direction == Direction::NonCash {
                return Err("non-cash one-time flow".into());
            }
            positive(amount)?;
            convertible(currency)
        }
        Fact::ExpenseAmountChange { category: c, amount, percent, currency, .. } => {
            category(c)?;
            match (amount, percent) {
                (Some(a), None) => {
                    positive(a)?;
                    convertible(currency.as_deref().unwrap_or(home))
                }
                (None, Some(p)) if p.is_finite() && *p > -100.0 && *p <= 1000.0 => Ok(()),
                (None, Some(p)) => Err(format!("implausible percent {p}")),
                _ => Err("needs exactly one of amount/percent".into()),
            }
        }
        Fact::IncomeDateMoved { category: c, .. } | Fact::IncomeEnded { category: c, .. } => category(c),
        Fact::Unconfirmed { .. } => Ok(()),
        _ => Err("event-level fact without an event id".into()),
    }
}

/// Status/direction cash rules (PLAN.md §2.1 table).
pub fn base_treatment(e: &Event) -> CashTreatment {
    use CashTreatment::*;
    match (e.status, e.direction) {
        (_, Direction::NonCash) | (Status::Unrealized, _) => Excluded(ExclusionReason::NonCash),
        (Status::Cancelled, _) => Excluded(ExclusionReason::Cancelled),
        (Status::Failed, _) => Excluded(ExclusionReason::Failed),
        (Status::Pending, Direction::Credit) => Excluded(ExclusionReason::PendingCredit),
        (Status::Pending, Direction::Debit) => Reserved,
        (Status::Scheduled, _) => Scheduled,
        (Status::Settled, _) => Settled,
    }
}

fn is_lifecycle_continuation(e: &Event) -> bool {
    !matches!(
        e.event_type,
        EventType::Refund | EventType::InvestmentSale | EventType::InvestmentValuation
    )
}

fn apply_fact(entry: &mut LedgerEntry, rec: &EvidenceRecord) -> Result<(), String> {
    let id = rec.record_id.clone();
    match &rec.fact {
        Fact::EventAmount { amount, currency, .. } => {
            if !same_currency(currency, &entry.event.currency) {
                return Err(currency_mismatch(currency, &entry.event.currency));
            }
            if *amount <= Money::ZERO {
                return Err("non-positive figure".into());
            }
            if entry.event.amount.is_some() {
                // Conflict rule 3: the supplied row beats an extracted estimate.
                return Err("row already has an amount".into());
            }
            entry.amount = Some(*amount);
            entry.amount_source = AmountSource::Evidence(id.clone());
        }
        Fact::EventCancelled { .. } => {
            entry.treatment = CashTreatment::Excluded(ExclusionReason::CancelledByEvidence(id.clone()));
        }
        Fact::EventSettled { amount, date, .. } => {
            if let Some(a) = amount {
                entry.amount = Some(*a);
                entry.amount_source = AmountSource::Evidence(id.clone());
            }
            if let Some(d) = date {
                entry.cash_date = *d;
            }
            entry.treatment = match (entry.event.status, entry.event.direction) {
                // A pending credit that has now settled becomes real incoming cash.
                (Status::Pending, Direction::Credit) => CashTreatment::Scheduled,
                (_, _) if matches!(entry.treatment, CashTreatment::Excluded(ExclusionReason::PendingCredit)) => {
                    CashTreatment::Scheduled
                }
                _ => entry.treatment.clone(),
            };
        }
        Fact::EventAmended { amount, date, .. } => {
            if let Some(a) = amount {
                entry.amount = Some(*a);
                entry.amount_source = AmountSource::Evidence(id.clone());
            }
            if let Some(d) = date {
                entry.cash_date = *d;
            }
        }
        Fact::DuplicateOf { of_event_id, .. } => {
            entry.treatment = CashTreatment::Excluded(ExclusionReason::DuplicateOf {
                of: of_event_id.clone(),
                record: id.clone(),
            });
        }
        Fact::OwnAccountTransfer { .. } => {
            entry.treatment = CashTreatment::Excluded(ExclusionReason::OwnAccountTransfer(id.clone()));
        }
        _ => return Err("not an event-level fact".into()),
    }
    entry.applied_evidence.push(id);
    Ok(())
}

/// Printed currency marks to ISO codes. Same table as extraction's
/// `extract::normalize::parse_currency` (OCR branch); the engine keeps its own copy so a raw
/// mark that slips past extraction still compares correctly.
fn normalize_currency(raw: &str) -> Option<&'static str> {
    let upper = raw.trim().trim_end_matches('.').to_uppercase();
    Some(match upper.as_str() {
        "INR" | "RS" | "RUPEES" | "RUPEE" | "INDIAN RUPEE" | "INDIAN RUPEES" | "\u{20B9}" => "INR",
        "USD" | "US$" | "$" | "US DOLLAR" | "US DOLLARS" | "DOLLAR" | "DOLLARS" => "USD",
        "EUR" | "\u{20AC}" | "EURO" | "EUROS" => "EUR",
        "IDR" | "RP" | "RUPIAH" | "RUPIAHS" | "INDONESIAN RUPIAH" => "IDR",
        "ZAR" | "R" | "RAND" | "SOUTH AFRICAN RAND" => "ZAR",
        _ => return None,
    })
}

/// Evidence currencies arrive as printed (`Rs`, `₹`, `$`): compare the normalized code with
/// the event currency, never the raw string. Codes the table does not know compare as-is.
fn same_currency(raw: &str, event_currency: &str) -> bool {
    normalize_currency(raw).unwrap_or(raw.trim()).eq_ignore_ascii_case(event_currency.trim())
}

fn currency_mismatch(raw: &str, event_currency: &str) -> String {
    format!("currency {raw} (normalized {:?}) != event currency {event_currency}", normalize_currency(raw))
}

/// Convert with the rate row for `date` in the stated direction; fall back to the inverse
/// of the opposite-direction row for the same date.
pub fn to_home(amount: Money, from: &str, home: &str, date: NaiveDate, rates: &dyn RateProvider) -> Option<Money> {
    if from == home {
        return Some(amount);
    }
    if let Some(r) = rates.rate(date, from, home) {
        return Some(amount.convert(&r));
    }
    rates.rate(date, home, from).map(|r| amount.convert(&r.inverse()))
}

fn id_num(id: &str) -> u64 {
    super::types::id_rank(id).0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::types::{Flexibility, RateTable};

    fn ev(id: &str, status: Status, dir: Direction, ty: EventType, link: Option<&str>) -> Event {
        let d = NaiveDate::from_ymd_opt(2024, 3, 1).unwrap();
        Event {
            id: id.into(),
            event_type: ty,
            description: "x".into(),
            category: "shopping".into(),
            direction: dir,
            amount: Some(Money::from_units(10)),
            currency: "ZAR".into(),
            event_date: d,
            settlement_date: Some(d),
            status,
            linked_event_id: link.map(String::from),
            flexibility: Flexibility::Fixed,
            minimum_allowed_amount: None,
        }
    }

    #[test]
    fn cash_rules_and_lifecycle() {
        use Direction::*;
        use EventType::*;
        let events = vec![
            ev("event_1", Status::Pending, Debit, Expense, None),
            ev("event_2", Status::Settled, Debit, Expense, Some("event_1")),
            ev("event_3", Status::Pending, Credit, Refund, Some("event_2")),
            ev("event_4", Status::Failed, Debit, DebtPayment, None),
            ev("event_5", Status::Scheduled, Debit, DebtPayment, Some("event_4")),
            ev("event_6", Status::Pending, Debit, Expense, None),
            ev("event_7", Status::Pending, Debit, Expense, Some("event_2")),
        ];
        let evidence = vec![EvidenceRecord {
            record_id: "message_9#0".into(),
            source: EvidenceSource::Message { source_type: "bank".into() },
            observed_at: NaiveDate::from_ymd_opt(2024, 3, 1).unwrap().and_hms_opt(9, 0, 0).unwrap(),
            fact: Fact::DuplicateOf { event_id: "event_6".into(), of_event_id: Some("event_2".into()) },
        }];
        let l = Ledger::build("ZAR", &events, &evidence, &RateTable::default(), &Rules::default());
        let t = |id: &str| l.get(id).unwrap().treatment.clone();
        assert_eq!(t("event_1"), CashTreatment::Excluded(ExclusionReason::SupersededBy("event_2".into())));
        assert_eq!(t("event_2"), CashTreatment::Settled);
        assert_eq!(t("event_7"), CashTreatment::Reserved);
        assert_eq!(t("event_3"), CashTreatment::Excluded(ExclusionReason::PendingCredit));
        assert_eq!(t("event_4"), CashTreatment::Excluded(ExclusionReason::Failed));
        assert_eq!(t("event_5"), CashTreatment::Scheduled);
        assert!(matches!(t("event_6"), CashTreatment::Excluded(ExclusionReason::DuplicateOf { .. })));
    }
}
