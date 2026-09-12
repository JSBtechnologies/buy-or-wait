//! 90-day day-by-day forecast and the two hard numbers (PLAN.md §2.7).
//!
//! `balance[t]` is the projected end-of-day balance on `start + t`. A payment on day d lowers
//! every later balance by the same amount, so with headroom `H(t) = balance[t] - M`:
//! - safe amount today = min(requested, max(0, min_t H(t)))
//! - suffix minimum from d = min over t >= d of H(t), computed once
//! - earliest full date = first d with suffix[d] >= requested

use chrono::{Duration, NaiveDate};
use serde::{Deserialize, Serialize};

use super::ledger::{Fact, Ledger, LedgerEntry};
use super::money::Cents;
use super::recurrence::{Cadence, Stream, StreamKind, Streams};
use super::rules::Rules;
use super::types::{Direction, Payment, RateProvider};

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
    pub amount: Cents,
    pub category: String,
    pub source: FlowSource,
}

/// A stop / reduce_to action on one flexible event.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum SpendingChange {
    Stop { event_id: String },
    ReduceTo { event_id: String, amount: Cents },
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
    pub opening_balance: Cents,
    pub minimum_balance: Cents,
    pub reserved_total: Cents,
    pub flows: Vec<Flow>,
    /// End-of-day balance per day, `horizon_days` long.
    pub balance: Vec<Cents>,
    /// `suffix_min[d] = min over t >= d of balance[t] - minimum_balance`.
    pub suffix_min: Vec<Cents>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SafetyReport {
    pub safe: bool,
    /// First day the plan takes the balance below the minimum.
    pub first_breach: Option<(NaiveDate, Cents)>,
    /// Lowest balance with the plan applied, and its first date.
    pub trough_balance: Cents,
    pub trough_date: NaiveDate,
}

pub struct ForecastInputs<'a> {
    pub ledger: &'a Ledger,
    pub streams: &'a Streams,
    pub opening_balance: Cents,
    pub minimum_balance: Cents,
    pub start: NaiveDate,
    pub rates: &'a dyn RateProvider,
    pub rules: &'a Rules,
}

impl Forecast {
    /// Build the projection with optional spending changes applied to the streams.
    pub fn build(inp: &ForecastInputs, changes: &[SpendingChange]) -> Forecast {
        let rules = inp.rules;
        let start = inp.start;
        let end = start + Duration::days(rules.horizon_days - 1);
        let mut flows = Vec::new();

        // Reserved pending debits.
        let mut reserved_total = Cents::ZERO;
        for e in inp.ledger.reserved() {
            let Some(a) = e.home_amount else { continue };
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
            .filter(|e| e.home_amount.is_some())
            .filter(|e| !(rules.ignore_scheduled_before_request_date && e.cash_date < start))
            .collect();
        for e in &scheduled {
            if e.cash_date > end {
                continue;
            }
            flows.push(Flow {
                date: e.cash_date.max(start),
                amount: e.signed_home_amount().unwrap(),
                category: e.event.category.clone(),
                source: FlowSource::Scheduled { event_id: e.event.id.clone() },
            });
        }

        // Stream projections, with spending changes and forecast-level evidence applied.
        for stream in &inp.streams.streams {
            let Some(amount) = changed_amount(stream, changes, rules) else { continue };
            let mut dates = stream.dates_between(start, end);
            // A scheduled row of the same category/direction replaces the matching occurrence.
            dates.retain(|d| {
                !scheduled.iter().any(|e| {
                    e.event.category == stream.category
                        && e.event.direction == stream.direction
                        && (e.cash_date - *d).num_days().abs() <= rules.scheduled_match_window_days
                })
            });
            for d in dates {
                flows.push(Flow {
                    date: d,
                    amount: Cents(stream.sign() * amount.0),
                    category: stream.category.clone(),
                    source: FlowSource::Stream { stream_id: stream.id.clone() },
                });
            }
        }
        apply_adjustments(&mut flows, inp, start, end);

        flows.sort_by(|a, b| (a.date, &a.category).cmp(&(b.date, &b.category)));
        Forecast::from_flows(start, inp.opening_balance, inp.minimum_balance, reserved_total, flows, rules.horizon_days)
    }

    pub fn from_flows(
        start: NaiveDate,
        opening_balance: Cents,
        minimum_balance: Cents,
        reserved_total: Cents,
        flows: Vec<Flow>,
        horizon_days: i64,
    ) -> Forecast {
        let n = horizon_days as usize;
        let mut delta = vec![Cents::ZERO; n];
        for f in &flows {
            let t = (f.date - start).num_days();
            if (0..horizon_days).contains(&t) {
                delta[t as usize] += f.amount;
            }
        }
        let mut balance = Vec::with_capacity(n);
        let mut running = opening_balance;
        for d in delta {
            running += d;
            balance.push(running);
        }
        let mut suffix_min = vec![Cents::ZERO; n];
        let mut m = Cents(i64::MAX);
        for t in (0..n).rev() {
            m = m.min(balance[t] - minimum_balance);
            suffix_min[t] = m;
        }
        Forecast { start, opening_balance, minimum_balance, reserved_total, flows, balance, suffix_min }
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

    /// Unrounded closed-form safe amount for today, before capping at the request.
    pub fn raw_safe_amount(&self) -> Cents {
        self.suffix_min[0].max(Cents::ZERO)
    }

    pub fn safe_amount(&self, requested: Cents, rules: &Rules) -> Cents {
        rules.round_safe_amount(self.raw_safe_amount()).min(requested)
    }

    /// First date a single payment of `requested` keeps every later balance >= minimum.
    pub fn earliest_full_date(&self, requested: Cents) -> Option<NaiveDate> {
        self.suffix_min.iter().position(|&h| h >= requested).map(|t| self.date_of(t))
    }

    /// Lowest projected balance (no plan) and its first date.
    pub fn trough(&self) -> (Cents, NaiveDate) {
        self.trough_with(&[])
    }

    fn trough_with(&self, cumulative: &[Cents]) -> (Cents, NaiveDate) {
        let mut best = (Cents(i64::MAX), self.start);
        for (t, b) in self.balance.iter().enumerate() {
            let v = *b - cumulative.get(t).copied().unwrap_or(Cents::ZERO);
            if v < best.0 {
                best = (v, self.date_of(t));
            }
        }
        best
    }

    /// Replay a payment schedule: subtract each payment from its date onward and require
    /// every day of the horizon to stay >= minimum.
    pub fn check(&self, payments: &[Payment], rules: &Rules) -> SafetyReport {
        let n = self.balance.len();
        let mut cumulative = vec![Cents::ZERO; n];
        let mut add = vec![Cents::ZERO; n];
        let mut before_start = Cents::ZERO;
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
        let mut run = before_start;
        for t in 0..n {
            run += add[t];
            cumulative[t] = run;
        }
        let first_breach = (0..n)
            .find(|&t| self.balance[t] - cumulative[t] < self.minimum_balance)
            .map(|t| (self.date_of(t), self.balance[t] - cumulative[t]));
        let (trough_balance, trough_date) = self.trough_with(&cumulative);
        SafetyReport { safe: first_breach.is_none(), first_breach, trough_balance, trough_date }
    }
}

/// The per-occurrence amount of a stream after spending changes; `None` when stopped.
fn changed_amount(stream: &Stream, changes: &[SpendingChange], rules: &Rules) -> Option<Cents> {
    let hits: Vec<&SpendingChange> = changes
        .iter()
        .filter(|c| stream.occurrences.iter().any(|o| o.event_id == c.event_id()))
        .collect();
    if hits.is_empty() {
        return Some(stream.projected_amount);
    }
    match stream.kind {
        StreamKind::Recurring => {
            let mut amount = stream.projected_amount;
            for c in hits {
                match c {
                    SpendingChange::Stop { .. } => return None,
                    SpendingChange::ReduceTo { amount: a, .. } => amount = amount.min(*a),
                }
            }
            Some(amount)
        }
        StreamKind::VariableSpend => {
            // The change rewrites the targeted row inside the pool, then the rate is
            // re-estimated from the edited history.
            let mut amounts = Vec::new();
            for o in &stream.occurrences {
                match hits.iter().find(|c| c.event_id() == o.event_id) {
                    Some(SpendingChange::Stop { .. }) => {}
                    Some(SpendingChange::ReduceTo { amount, .. }) => amounts.push((*amount).min(o.amount)),
                    None => amounts.push(o.amount),
                }
            }
            Some(rules.variable_estimator.estimate(&amounts))
        }
    }
}

/// Forecast-level evidence: income changes, pay-date moves, new expenses, one-time flows.
fn apply_adjustments(flows: &mut Vec<Flow>, inp: &ForecastInputs, start: NaiveDate, end: NaiveDate) {
    let home = &inp.ledger.home_currency;
    let convert = |amount: Cents, currency: &str, date: NaiveDate| {
        super::ledger::to_home(amount, currency, home, date, inp.rates)
    };
    let is_income_flow = |f: &Flow, category: &str| {
        f.category == category
            && f.amount > Cents::ZERO
            && matches!(f.source, FlowSource::Stream { .. } | FlowSource::Scheduled { .. })
    };
    let mut adjustments: Vec<_> = inp.ledger.adjustments.iter().collect();
    adjustments.sort_by(|a, b| (a.observed_at, &a.record_id).cmp(&(b.observed_at, &b.record_id)));
    for rec in adjustments {
        let src = FlowSource::Evidence { record_id: rec.record_id.clone() };
        match &rec.fact {
            Fact::IncomeAmountChange { category, amount, currency, effective } => {
                for f in flows.iter_mut().filter(|f| is_income_flow(f, category) && f.date >= *effective) {
                    if let Some(a) = convert(*amount, currency, f.date) {
                        f.amount = a;
                    }
                }
            }
            Fact::NextIncomeAmount { category, amount, currency, date } => {
                if let Some(f) = flows
                    .iter_mut()
                    .filter(|f| is_income_flow(f, category) && date.map_or(true, |d| f.date >= d))
                    .min_by_key(|f| f.date)
                {
                    if let Some(a) = convert(*amount, currency, f.date) {
                        f.amount = a;
                    }
                }
            }
            Fact::IncomeDateMoved { category, new_date } => {
                if let Some(f) = flows.iter_mut().filter(|f| is_income_flow(f, category)).min_by_key(|f| f.date) {
                    f.date = *new_date;
                }
            }
            Fact::IncomeEnded { category, effective } => {
                flows.retain(|f| !(is_income_flow(f, category) && f.date >= *effective));
            }
            Fact::NewRecurringExpense { category, amount, currency, first_date, every_days, description } => {
                let occ = super::recurrence::Occurrence {
                    event_id: rec.record_id.clone(),
                    description: description.clone(),
                    date: *first_date,
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
            Flow { date: d("2024-09-05"), amount: Cents::from_units(-500), category: "rent".into(), source: FlowSource::Stream { stream_id: "r".into() } },
            Flow { date: d("2024-09-10"), amount: Cents::from_units(1000), category: "salary".into(), source: FlowSource::Stream { stream_id: "s".into() } },
        ];
        let f = Forecast::from_flows(start, Cents::from_units(1000), Cents::from_units(200), Cents::ZERO, flows, 90);
        let rules = Rules::default();
        assert_eq!(f.safe_amount(Cents::from_units(900), &rules), Cents::from_units(300));
        assert_eq!(f.earliest_full_date(Cents::from_units(900)), Some(d("2024-09-10")));
        assert_eq!(f.trough(), (Cents::from_units(500), d("2024-09-05")));
        let plan = [Payment { date: start, amount: Cents::from_units(300) }, Payment { date: d("2024-09-10"), amount: Cents::from_units(600) }];
        assert!(f.check(&plan, &rules).safe);
        let bad = [Payment { date: start, amount: Cents::from_units(301) }];
        assert_eq!(f.check(&bad, &rules).first_breach.map(|b| b.0), Some(d("2024-09-05")));
    }
}
