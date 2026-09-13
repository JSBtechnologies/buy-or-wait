//! 90-day day-by-day forecast and the two hard numbers (PLAN.md §2.7).
//!
//! `balance[t]` is the projected end-of-day balance on `start + t`. A payment on day d lowers
//! every later balance by the same amount, so with headroom `H(t) = balance[t] - M`:
//! - safe amount today = min(requested, max(0, min_t H(t)))
//! - suffix minimum from d = min over t >= d of H(t), computed once
//! - earliest full date = first d with suffix[d] >= requested

use chrono::{Datelike, Duration, NaiveDate};
use serde::{Deserialize, Serialize};

use super::ledger::{Fact, Ledger, LedgerEntry};
use super::money::Money;
use super::recurrence::{Cadence, Stream, StreamKind, Streams};
use super::rules::{DebitKind, PaymentTiming, Placement, Rules, ScheduledReplaceScope, VarLongPhase};
use super::types::{Direction, Flexibility, Payment, RateProvider};

/// Half a cent: plan amounts are written rounded to cents (RULES S1.5), so a plan built from
/// the rounded safe amount may exceed the exact headroom by at most this much.
pub const SAFETY_TOLERANCE: Money = Money(super::money::SCALE / 200);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum FlowSource {
    /// Pending debit reserved against the balance.
    Reserved { event_id: String },
    /// Scheduled row on its cash date.
    Scheduled { event_id: String },
    /// Projected occurrence of a detected stream.
    Stream { stream_id: String },
    /// Forecast-level evidence (salary change, new expense, one-time flow).
    Evidence { record_id: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Flow {
    pub date: NaiveDate,
    /// Signed: credits positive, debits negative (home currency).
    pub amount: Money,
    pub category: String,
    pub source: FlowSource,
}

/// A stop / reduce_to action on one flexible event.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum SpendingChange {
    Stop { event_id: String },
    ReduceTo { event_id: String, amount: Money },
}

impl SpendingChange {
    pub fn event_id(&self) -> &str {
        match self {
            SpendingChange::Stop { event_id } | SpendingChange::ReduceTo { event_id, .. } => event_id,
        }
    }

    pub fn render(&self) -> String {
        match self {
            SpendingChange::Stop { event_id } => format!("stop:{event_id}"),
            SpendingChange::ReduceTo { event_id, amount } => format!("reduce_to:{event_id}:{}", amount.fmt_plan()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Forecast {
    pub start: NaiveDate,
    pub opening_balance: Money,
    pub minimum_balance: Money,
    pub reserved_total: Money,
    pub flows: Vec<Flow>,
    /// Balance on day t after that day's debits, before its credits (RULES S2.3:
    /// debits apply before credits within a day). `horizon_days` long.
    pub low: Vec<Money>,
    /// End-of-day balance per day.
    pub balance: Vec<Money>,
    /// `suffix_low[d] = min over t >= d of low[t]`.
    pub suffix_low: Vec<Money>,
    /// How `check` applies a plan payment within its day.
    pub payment_timing: PaymentTiming,
    /// Same-day placement of each entry of `flows` (credits are recorded as BeforeCredits
    /// but always land between the two debit groups).
    pub placements: Vec<Placement>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SafetyReport {
    pub safe: bool,
    /// First day the plan takes the balance below the minimum.
    pub first_breach: Option<(NaiveDate, Money)>,
    /// Lowest balance with the plan applied, and its first date.
    pub trough_balance: Money,
    pub trough_date: NaiveDate,
}

pub struct ForecastInputs<'a> {
    pub ledger: &'a Ledger,
    pub streams: &'a Streams,
    pub opening_balance: Money,
    pub minimum_balance: Money,
    pub start: NaiveDate,
    pub rates: &'a dyn RateProvider,
    pub rules: &'a Rules,
}

impl Forecast {
    /// Build the projection with optional spending changes applied to the streams.
    pub fn build(inp: &ForecastInputs, changes: &[SpendingChange]) -> Forecast {
        let rules = inp.rules;
        let start = inp.start;
        let end = rules.horizon_end(start);
        let horizon_days = (end - start).num_days() + 1;
        let mut flows = Vec::new();

        // Reserved pending debits.
        let mut reserved_total = Money::ZERO;
        // A blank row whose image failed closed holds its largest validated read instead
        // of vanishing (image accuracy plan §4); a blank row with no read stays skipped.
        for e in inp.ledger.reserved() {
            let Some(a) = e.home_amount.or_else(|| inp.ledger.unverified_home(&e.event.id)) else { continue };
            let date = if rules.reserve_pending_on_request_date { start } else { e.cash_date.max(start) };
            reserved_total += a;
            flows.push(Flow {
                date,
                amount: -a,
                category: e.event.category.clone(),
                source: FlowSource::Reserved { event_id: e.event.id.clone() },
            });
        }

        // Scheduled rows on their cash date.
        let scheduled: Vec<&LedgerEntry> = inp
            .ledger
            .scheduled()
            .filter(|e| e.home_amount.is_some() || inp.ledger.unverified_home(&e.event.id).is_some())
            .filter(|e| !(rules.ignore_scheduled_before_request_date && e.cash_date < start))
            .collect();
        for e in &scheduled {
            if e.cash_date > end {
                continue;
            }
            // An unverified reserve is always a debit.
            let amount = e.signed_home_amount().or_else(|| inp.ledger.unverified_home(&e.event.id).map(|a| -a));
            flows.push(Flow {
                date: e.cash_date.max(start),
                amount: amount.expect("filtered to rows with an amount or reserve"),
                category: e.event.category.clone(),
                source: FlowSource::Scheduled { event_id: e.event.id.clone() },
            });
        }

        // Stream projections, with spending changes and forecast-level evidence applied.
        for stream in &inp.streams.streams {
            let Some(amount) = changed_amount(stream, changes, rules) else { continue };
            let stream_end = match (stream.kind, rules.variable_horizon) {
                (StreamKind::VariableSpend, Some(h)) => h.end(start).min(end),
                _ => end,
            };
            let mut dates = match stream.cadence {
                // RULES S8.2 VAR_LONG_PHASE=from_request: long-interval variable spend restarts
                // its phase at rd + IV_SKIP_DAYS instead of continuing from the last row.
                Cadence::EveryDays { days }
                    if stream.kind == StreamKind::VariableSpend
                        && rules.var_long_phase != VarLongPhase::LastSettled
                        && days >= rules.var_long_min_step =>
                {
                    let first = match rules.var_long_phase {
                        VarLongPhase::MidStep => {
                            (stream.last_date() + Duration::days(days)).min(start + Duration::days((days + 1) / 2))
                        }
                        _ => start + Duration::days(rules.variable_skip_days),
                    };
                    (0..)
                        .map(|k| first + Duration::days(k * days))
                        .take_while(|d| *d <= stream_end)
                        .filter(|d| *d >= start)
                        .collect()
                }
                _ => stream.dates_between(start, stream_end),
            };
            // RULES S3.4(c): a scheduled row replaces the projected occurrence of a monthly
            // stream with the same category and direction within the window (verifier#45).
            if rules.scheduled_replaces_cycle && matches!(stream.cadence, Cadence::Monthly { .. }) {
                dates.retain(|d| {
                    !scheduled.iter().any(|e| {
                        e.event.category == stream.category
                            && e.event.direction == stream.direction
                            && (e.cash_date - *d).num_days().abs() <= rules.scheduled_replacement_window_days
                            && replaces_in_scope(e, e.home_amount.or_else(|| inp.ledger.unverified_home(&e.event.id)), stream, rules)
                    })
                });
            }
            // RULES S3.2: interval (variable-spend) occurrences on rd or rd+1 are skipped.
            if stream.kind == StreamKind::VariableSpend {
                dates.retain(|d| (*d - start).num_days() >= rules.variable_skip_days);
            }
            for d in dates {
                flows.push(Flow {
                    date: d,
                    amount: Money(stream.sign() * amount.0),
                    category: stream.category.clone(),
                    source: FlowSource::Stream { stream_id: stream.id.clone() },
                });
            }
        }
        apply_adjustments(&mut flows, inp, start, end);

        flows.sort_by(|a, b| (a.date, &a.category).cmp(&(b.date, &b.category)));
        let mut f = Forecast::from_flows_placed(
            start,
            inp.opening_balance,
            inp.minimum_balance,
            reserved_total,
            flows,
            horizon_days,
            &|flow| rules.placement_of(debit_kind(flow, &inp.streams.streams)),
        );
        f.payment_timing = rules.payment_timing;
        f
    }

    /// All debits before same-day credits (RULES S2.3 default).
    pub fn from_flows(
        start: NaiveDate,
        opening_balance: Money,
        minimum_balance: Money,
        reserved_total: Money,
        flows: Vec<Flow>,
        horizon_days: i64,
    ) -> Forecast {
        Forecast::from_flows_placed(start, opening_balance, minimum_balance, reserved_total, flows, horizon_days, &|_| {
            Placement::BeforeCredits
        })
    }

    /// Day-by-day balances where each debit is placed before or after that day's credits.
    /// `low[t]` is the day's lowest point: min(balance after pre-credit debits, end of day).
    #[allow(clippy::too_many_arguments)]
    pub fn from_flows_placed(
        start: NaiveDate,
        opening_balance: Money,
        minimum_balance: Money,
        reserved_total: Money,
        flows: Vec<Flow>,
        horizon_days: i64,
        placement: &dyn Fn(&Flow) -> Placement,
    ) -> Forecast {
        let n = horizon_days as usize;
        let placements: Vec<Placement> = flows.iter().map(|f| placement(f)).collect();
        let mut before = vec![Money::ZERO; n];
        let mut credits = vec![Money::ZERO; n];
        let mut after = vec![Money::ZERO; n];
        for (f, p) in flows.iter().zip(&placements) {
            let t = (f.date - start).num_days();
            if (0..horizon_days).contains(&t) {
                let t = t as usize;
                if f.amount >= Money::ZERO {
                    credits[t] += f.amount;
                } else if *p == Placement::AfterCredits {
                    after[t] += f.amount;
                } else {
                    before[t] += f.amount;
                }
            }
        }
        let mut low = Vec::with_capacity(n);
        let mut balance = Vec::with_capacity(n);
        let mut running = opening_balance;
        for t in 0..n {
            let pre = running + before[t];
            running = pre + credits[t] + after[t];
            low.push(pre.min(running));
            balance.push(running);
        }
        let mut suffix_low = vec![Money::ZERO; n];
        let mut m = Money(i64::MAX);
        for t in (0..n).rev() {
            m = m.min(low[t]);
            suffix_low[t] = m;
        }
        Forecast {
            start,
            opening_balance,
            minimum_balance,
            reserved_total,
            flows,
            low,
            balance,
            suffix_low,
            payment_timing: PaymentTiming::AfterDayRows,
            placements,
        }
    }

    /// Flows that produce the trough, grouped by component (see `DecisionFacts::trough_drivers`).
    pub fn trough_drivers(&self) -> Vec<super::facts::TroughDriver> {
        use std::collections::BTreeMap;
        let (_, tdate) = self.trough();
        let mut groups: BTreeMap<(String, String, String), (Money, usize)> = BTreeMap::new();
        // The trough is either the trough day's pre-credit low (only its before-credit debits
        // count) or its end-of-day balance (every flow of the day counts).
        let td = (tdate - self.start).num_days() as usize;
        let end_of_day_trough = self.low.get(td) == self.balance.get(td);
        for (f, p) in self.flows.iter().zip(&self.placements) {
            let same_day = f.date == tdate && (end_of_day_trough || (f.amount < Money::ZERO && *p == Placement::BeforeCredits));
            let counts = f.date < tdate || same_day;
            if f.date < self.start || !counts {
                continue;
            }
            let (kind, component) = match &f.source {
                FlowSource::Reserved { event_id } => ("reserved", event_id.clone()),
                FlowSource::Scheduled { event_id } => ("scheduled", event_id.clone()),
                FlowSource::Stream { stream_id } => ("stream", stream_id.clone()),
                FlowSource::Evidence { record_id } => ("evidence", record_id.clone()),
            };
            let e = groups.entry((kind.to_string(), component, f.category.clone())).or_insert((Money::ZERO, 0));
            e.0 += f.amount;
            e.1 += 1;
        }
        let mut out: Vec<super::facts::TroughDriver> = groups
            .into_iter()
            .map(|((kind, component, category), (total, occurrences))| super::facts::TroughDriver { kind, component, category, total, occurrences })
            .collect();
        out.sort_by(|a, b| b.total.abs().cmp(&a.total.abs()).then(a.component.cmp(&b.component)));
        out
    }

    pub fn horizon_end(&self) -> NaiveDate {
        self.start + Duration::days(self.balance.len() as i64 - 1)
    }

    pub fn day(&self, date: NaiveDate) -> Option<usize> {
        let t = (date - self.start).num_days();
        (t >= 0 && (t as usize) < self.balance.len()).then_some(t as usize)
    }

    pub fn date_of(&self, t: usize) -> NaiveDate {
        self.start + Duration::days(t as i64)
    }

    /// Unrounded closed-form safe amount for today: `min_t low[t] - M`, floored at zero.
    pub fn raw_safe_amount(&self) -> Money {
        (self.suffix_low[0] - self.minimum_balance).max(Money::ZERO)
    }

    pub fn safe_amount(&self, requested: Money, rules: &Rules) -> Money {
        rules.round_safe_amount(self.raw_safe_amount()).min(requested)
    }

    /// Headroom for a payment made on day d after that day's credits (RULES S2.3):
    /// `min(balance[d], min_{t>d} low[t]) - M`.
    pub fn headroom_from(&self, d: usize) -> Money {
        let later = self.suffix_low.get(d + 1).copied().unwrap_or(Money(i64::MAX));
        self.balance[d].min(later) - self.minimum_balance
    }

    /// First date a single payment of `requested` keeps every later balance >= minimum.
    pub fn earliest_full_date(&self, requested: Money) -> Option<NaiveDate> {
        (0..self.balance.len()).find(|&d| self.headroom_from(d) >= requested).map(|d| self.date_of(d))
    }

    /// Lowest projected intraday balance (no plan) and its first date.
    pub fn trough(&self) -> (Money, NaiveDate) {
        self.check(&[], &Rules::default()).trough()
    }

    /// Replay a payment schedule. A payment on day d applies after d's credits, so it must
    /// keep `balance[d]` and every later intraday low `low[t]` at or above the minimum.
    pub fn check(&self, payments: &[Payment], rules: &Rules) -> SafetyReport {
        let n = self.balance.len();
        let mut add = vec![Money::ZERO; n];
        let mut before_start = Money::ZERO;
        for p in payments {
            let t = (p.date - self.start).num_days();
            if t < 0 {
                before_start += p.amount;
            } else if (t as usize) < n {
                add[t as usize] += p.amount;
            } else if !rules.ignore_payments_after_horizon {
                add[n - 1] += p.amount;
            }
        }
        let m = self.minimum_balance;
        let mut paid_before = before_start;
        let mut first_breach = None;
        let mut trough = (Money(i64::MAX), self.start);
        for t in 0..n {
            let low = match self.payment_timing {
                PaymentTiming::AfterDayRows => self.low[t] - paid_before,
                PaymentTiming::BeforeCredits => self.low[t] - paid_before - add[t],
            };
            let end = self.balance[t] - paid_before - add[t];
            for v in [low, end] {
                if v < trough.0 {
                    trough = (v, self.date_of(t));
                }
                if v < m - SAFETY_TOLERANCE && first_breach.is_none() {
                    first_breach = Some((self.date_of(t), v));
                }
            }
            paid_before += add[t];
        }
        SafetyReport { safe: first_breach.is_none(), first_breach, trough_balance: trough.0, trough_date: trough.1 }
    }
}

impl SafetyReport {
    pub fn trough(&self) -> (Money, NaiveDate) {
        (self.trough_balance, self.trough_date)
    }
}

/// Which configurable same-day ordering class a debit flow belongs to. A monthly bill is
/// flexible when its stream's latest row is reducible or stoppable; a stream not found in
/// `streams` counts as fixed.
pub fn debit_kind(f: &Flow, streams: &[Stream]) -> DebitKind {
    match &f.source {
        FlowSource::Reserved { .. } => DebitKind::Reserved,
        FlowSource::Scheduled { .. } => DebitKind::Scheduled,
        FlowSource::Evidence { .. } => DebitKind::Evidence,
        FlowSource::Stream { stream_id } => match streams.iter().find(|s| &s.id == stream_id) {
            Some(s) if s.kind == StreamKind::VariableSpend => DebitKind::VariableSpend,
            Some(s) if s.occurrences.last().is_some_and(|o| o.flexibility != Flexibility::Fixed) => {
                DebitKind::FlexibleBill
            }
            // Interval (variable-spend) streams are the only ids recurrence builds as `var:*`.
            None if stream_id.starts_with("var:") => DebitKind::VariableSpend,
            _ => DebitKind::FixedBill,
        },
    }
}

/// RULES S8.3 `SCHEDULED_REPLACE_SCOPE`: whether a scheduled row in a stream's window may
/// replace that stream's projected occurrence.
/// `home_amount` is the row's amount, or its unverified reserve.
fn replaces_in_scope(e: &LedgerEntry, home_amount: Option<Money>, stream: &Stream, rules: &Rules) -> bool {
    match rules.scheduled_replace_scope {
        ScheduledReplaceScope::CategoryWindow => true,
        ScheduledReplaceScope::LifecycleOrAmount => {
            if e.event.direction != Direction::Debit || e.event.linked_event_id.is_some() {
                return true;
            }
            let (Some(a), est) = (home_amount, stream.projected_amount) else { return false };
            (a - est).abs().0 as i128 * 100 <= est.abs().0 as i128 * rules.scheduled_replace_amount_pct as i128
        }
    }
}

/// The per-occurrence amount of a stream after spending changes; `None` when stopped.
/// RULES S1.2: stop removes the stream's future occurrences; reduce_to sets every future
/// occurrence to the new amount (for pooled variable spend too, e.g. request_11 dining).
fn changed_amount(stream: &Stream, changes: &[SpendingChange], _rules: &Rules) -> Option<Money> {
    let mut amount = stream.projected_amount;
    for c in changes.iter().filter(|c| stream.occurrences.iter().any(|o| o.event_id == c.event_id())) {
        match c {
            SpendingChange::Stop { .. } => return None,
            SpendingChange::ReduceTo { amount: a, .. } => amount = amount.min(*a),
        }
    }
    Some(amount)
}

fn add_months_clamped(d: NaiveDate, k: u32) -> NaiveDate {
    let m0 = d.month0() + k;
    let (y, m) = (d.year() + (m0 / 12) as i32, m0 % 12 + 1);
    (1..=d.day()).rev().find_map(|day| NaiveDate::from_ymd_opt(y, m, day)).expect("valid date")
}

/// The description behind a projected flow: its stream's description or its ledger row's.
fn flow_description(f: &Flow, inp: &ForecastInputs) -> Option<String> {
    match &f.source {
        FlowSource::Stream { stream_id } => {
            inp.streams.streams.iter().find(|s| &s.id == stream_id).and_then(|s| s.description.clone())
        }
        FlowSource::Scheduled { event_id } | FlowSource::Reserved { event_id } => {
            inp.ledger.get(event_id).map(|e| e.event.description.clone())
        }
        FlowSource::Evidence { .. } => None,
    }
}

/// Whether a scheduled debit is a cycle of the category's recurring expense, so a rent/bill
/// change applies to it: linked (retry/lifecycle), same description as a stream of that
/// category, or within `scheduled_replace_amount_pct` of its estimate (RULES S8.3). A one-off
/// row beside the cycle (request_16 "Outstanding rent balance" vs "Monthly rent") keeps its
/// own amount. With no stream in the category the row is treated as the cycle.
fn is_cycle_row(event_id: &str, category: &str, inp: &ForecastInputs) -> bool {
    let Some(e) = inp.ledger.get(event_id) else { return false };
    let streams: Vec<&Stream> =
        inp.streams.streams.iter().filter(|s| s.category == category && s.direction == Direction::Debit).collect();
    if streams.is_empty() || e.event.linked_event_id.is_some() {
        return true;
    }
    let amount = e.home_amount.or_else(|| inp.ledger.unverified_home(event_id));
    streams.iter().any(|s| {
        s.description.as_deref().is_some_and(|d| descriptions_match(d, &e.event.description))
            || amount.is_some_and(|a| {
                let est = s.projected_amount;
                (a - est).abs().0 as i128 * 100 <= est.abs().0 as i128 * inp.rules.scheduled_replace_amount_pct as i128
            })
    })
}

fn descriptions_match(a: &str, b: &str) -> bool {
    let (a, b) = (a.to_lowercase(), b.to_lowercase());
    !a.is_empty() && !b.is_empty() && (a.contains(&b) || b.contains(&a))
}

#[allow(clippy::too_many_arguments)]
fn seed_monthly_income(
    flows: &mut Vec<Flow>,
    category: &str,
    amount: Money,
    currency: &str,
    first: NaiveDate,
    start: NaiveDate,
    end: NaiveDate,
    src: &FlowSource,
    convert: &dyn Fn(Money, &str, NaiveDate) -> Option<Money>,
) {
    let mut k = 0;
    loop {
        let m0 = first.month0() + k;
        let (y, m) = (first.year() + (m0 / 12) as i32, m0 % 12 + 1);
        let day = (1..=first.day()).rev().find_map(|dd| NaiveDate::from_ymd_opt(y, m, dd)).expect("valid day");
        if day > end {
            break;
        }
        if day >= start {
            if let Some(a) = convert(amount, currency, day) {
                flows.push(Flow { date: day, amount: a, category: category.to_string(), source: src.clone() });
            }
        }
        k += 1;
    }
}

/// Forecast-level evidence: income changes, pay-date moves, new expenses, one-time flows.
fn apply_adjustments(flows: &mut Vec<Flow>, inp: &ForecastInputs, start: NaiveDate, end: NaiveDate) {
    let home = &inp.ledger.home_currency;
    let convert = |amount: Money, currency: &str, date: NaiveDate| {
        super::ledger::to_home(amount, currency, home, date, inp.rates)
    };
    let is_income_flow = |f: &Flow, category: &str| {
        f.category == category
            && f.amount > Money::ZERO
            && matches!(f.source, FlowSource::Stream { .. } | FlowSource::Scheduled { .. })
    };
    let mut adjustments: Vec<_> = inp.ledger.adjustments.iter().collect();
    adjustments.sort_by(|a, b| (a.observed_at, &a.record_id).cmp(&(b.observed_at, &b.record_id)));
    for rec in adjustments {
        let src = FlowSource::Evidence { record_id: rec.record_id.clone() };
        match &rec.fact {
            Fact::IncomeAmountChange { category, amount, currency, effective } => {
                let mut changed = false;
                for f in flows.iter_mut().filter(|f| is_income_flow(f, category) && f.date >= *effective) {
                    if let Some(a) = convert(*amount, currency, f.date) {
                        f.amount = a;
                        changed = true;
                    }
                }
                // No projected income to change (stream paused or never seen): the change is
                // the income from its effective date, monthly.
                if !changed {
                    seed_monthly_income(flows, category, *amount, currency, *effective, start, end, &src, &convert);
                }
            }
            Fact::IncomeStarts { category, amount, currency, first_date } => {
                flows.retain(|f| !(is_income_flow(f, category) && f.date >= *first_date));
                seed_monthly_income(flows, category, *amount, currency, *first_date, start, end, &src, &convert);
            }
            Fact::NextIncomeAmount { category, amount, currency, date } => {
                // RULES S3.4/S5 [FIT]: "next salary is reduced to X" (user_08) and "temporary pay
                // continues for the next payroll" (user_06) project X for every later month,
                // unless `next_income_amount_persists` is off (then only the next occurrence).
                let mut income: Vec<&mut Flow> = flows
                    .iter_mut()
                    .filter(|f| is_income_flow(f, category) && date.map_or(true, |d| f.date >= d))
                    .collect();
                income.sort_by_key(|f| f.date);
                let take = if inp.rules.next_income_amount_persists { income.len() } else { 1 };
                for f in income.into_iter().take(take) {
                    if let Some(a) = convert(*amount, currency, f.date) {
                        f.amount = a;
                    }
                }
            }
            Fact::IncomeDateMoved { category, new_date } => {
                // The next income occurrence moves to new_date and later months follow the new
                // day of month (RULES S3.4 user_07: 09-23 then monthly 23rd).
                let mut income: Vec<&mut Flow> = flows.iter_mut().filter(|f| is_income_flow(f, category)).collect();
                income.sort_by_key(|f| f.date);
                // RULES S6.1: every later occurrence re-anchors, k-th one = new_date + k months
                // (day clamped to month end), so a move across a month boundary never collides.
                for (k, f) in income.into_iter().enumerate() {
                    f.date = add_months_clamped(*new_date, k as u32);
                }
            }
            Fact::ExpenseAmountChange { category, amount, percent, currency, effective } => {
                let targets = flows.iter_mut().filter(|f| {
                    f.category == *category
                        && f.amount < Money::ZERO
                        // None: from the next occurrence after the message (lead, blocker #32).
                        && f.date >= effective.unwrap_or(rec.observed_at.date())
                        && match &f.source {
                            FlowSource::Stream { .. } => true,
                            FlowSource::Scheduled { event_id } => is_cycle_row(event_id, category, inp),
                            _ => false,
                        }
                });
                for f in targets {
                    let new_mag = match (amount, percent) {
                        (Some(a), _) => convert(*a, currency.as_deref().unwrap_or(home), f.date),
                        (None, Some(p)) => {
                            // (100 + p) / 100 as one exact decimal, one rounding.
                            let pct = super::money::DecimalRate::from_f64(100.0 + *p);
                            let factor = super::money::DecimalRate { mantissa: pct.mantissa, scale: pct.scale + 2 };
                            Some((-f.amount).convert(&factor))
                        }
                        (None, None) => None,
                    };
                    if let Some(m) = new_mag {
                        f.amount = -m;
                    }
                }
            }
            Fact::IncomeEnded { category, effective, description } => {
                flows.retain(|f| {
                    let selected = match description {
                        None => true,
                        Some(want) => flow_description(f, inp).is_some_and(|have| descriptions_match(&have, want)),
                    };
                    !(is_income_flow(f, category) && f.date >= *effective && selected)
                });
            }
            Fact::NewRecurringExpense { category, amount, currency, first_date, every_days, description } => {
                let occ = super::recurrence::Occurrence {
                    event_id: rec.record_id.clone(),
                    description: description.clone(),
                    date: *first_date,
                    event_date: *first_date,
                    amount: *amount,
                    flexibility: super::types::Flexibility::Fixed,
                    minimum_allowed_amount: None,
                };
                let cadence = match every_days {
                    Some(d) => Cadence::EveryDays { days: *d as i64 },
                    None => Cadence::Monthly { day: chrono::Datelike::day(first_date) },
                };
                let s = Stream {
                    id: rec.record_id.clone(),
                    kind: StreamKind::Recurring,
                    direction: Direction::Debit,
                    category: category.clone(),
                    description: Some(description.clone()),
                    cadence,
                    occurrences: vec![occ],
                    projected_amount: *amount,
                };
                let mut dates = s.dates_between(start, end);
                if *first_date >= start && *first_date <= end {
                    dates.insert(0, *first_date);
                }
                for d in dates {
                    if let Some(a) = convert(*amount, currency, d) {
                        flows.push(Flow { date: d, amount: -a, category: category.clone(), source: src.clone() });
                    }
                }
            }
            Fact::OneTimeFlow { direction, category, amount, currency, date } => {
                if *date > end || *direction == Direction::NonCash {
                    continue;
                }
                let d = (*date).max(start);
                if let Some(a) = convert(*amount, currency, d) {
                    let signed = if *direction == Direction::Credit { a } else { -a };
                    flows.push(Flow { date: d, amount: signed, category: category.clone(), source: src.clone() });
                }
            }
            // Unconfirmed credits never count; event-level facts were applied in the ledger.
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    #[test]
    fn closed_form_numbers() {
        let start = d("2024-09-04");
        let flows = vec![
            Flow { date: d("2024-09-05"), amount: Money::from_units(-500), category: "rent".into(), source: FlowSource::Stream { stream_id: "r".into() } },
            Flow { date: d("2024-09-10"), amount: Money::from_units(1000), category: "salary".into(), source: FlowSource::Stream { stream_id: "s".into() } },
        ];
        let f = Forecast::from_flows(start, Money::from_units(1000), Money::from_units(200), Money::ZERO, flows, 90);
        let rules = Rules::default();
        assert_eq!(f.safe_amount(Money::from_units(900), &rules), Money::from_units(300));
        assert_eq!(f.earliest_full_date(Money::from_units(900)), Some(d("2024-09-10")));
        assert_eq!(f.trough(), (Money::from_units(500), d("2024-09-05")));
        let plan = [Payment { date: start, amount: Money::from_units(300) }, Payment { date: d("2024-09-10"), amount: Money::from_units(600) }];
        assert!(f.check(&plan, &rules).safe);
        let bad = [Payment { date: start, amount: Money::from_units(301) }];
        assert_eq!(f.check(&bad, &rules).first_breach.map(|b| b.0), Some(d("2024-09-05")));
    }
}
