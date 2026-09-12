//! Recurring-stream detection (PLAN.md §2.2) from settled ledger history.
//!
//! Signals: description + category + cadence. Two stream kinds:
//! - `Recurring`: bills/income/subscriptions with a confirmed cadence (monthly by day of month,
//!   or a fixed interval in days), projected on that cadence.
//! - `VariableSpend`: essential variable spending (groceries, dining, transport...) pooled by
//!   category across descriptions and projected conservatively as a rate.
//!
//! Anything else (one-time purchases, transfers, refunds, arrears, unusual events) is not a
//! stream and is never projected.

use std::collections::BTreeMap;

use chrono::{Datelike, Duration, NaiveDate};
use serde::{Deserialize, Serialize};

use super::ledger::Ledger;
use super::money::Cents;
use super::rules::Rules;
use super::types::{Direction, EventType, Flexibility};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Cadence {
    /// Monthly on this day of month (clamped to month end).
    Monthly { day: u32 },
    /// Every `days` days from the last occurrence.
    EveryDays { days: i64 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum StreamKind {
    Recurring,
    VariableSpend,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Occurrence {
    pub event_id: String,
    pub description: String,
    pub date: NaiveDate,
    /// Home-currency magnitude.
    pub amount: Cents,
    pub flexibility: Flexibility,
    /// Home-currency floor for reduce_to, when the row carries one.
    pub minimum_allowed_amount: Option<Cents>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Stream {
    /// Stable id: `rec:<direction>:<category>:<description>` or `var:<category>`.
    pub id: String,
    pub kind: StreamKind,
    pub direction: Direction,
    pub category: String,
    /// Shared description for `Recurring`; `None` for pooled variable spend.
    pub description: Option<String>,
    pub cadence: Cadence,
    /// Chronological supporting history.
    pub occurrences: Vec<Occurrence>,
    /// Home-currency magnitude of each projected occurrence (before spending changes).
    pub projected_amount: Cents,
}

impl Stream {
    pub fn last_date(&self) -> NaiveDate {
        self.occurrences.last().expect("stream has occurrences").date
    }

    pub fn latest_event_id(&self) -> &str {
        &self.occurrences.last().expect("stream has occurrences").event_id
    }

    pub fn sign(&self) -> i64 {
        if self.direction == Direction::Credit { 1 } else { -1 }
    }

    /// Projected occurrence dates in `[from, to]` (inclusive), strictly after the last
    /// observed occurrence.
    pub fn dates_between(&self, from: NaiveDate, to: NaiveDate) -> Vec<NaiveDate> {
        let mut out = Vec::new();
        let last = self.last_date();
        match self.cadence {
            Cadence::Monthly { day } => {
                let (mut y, mut m) = (last.year(), last.month());
                loop {
                    (y, m) = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
                    let d = clamp_day(y, m, day);
                    if d > to {
                        break;
                    }
                    if d >= from && d > last {
                        out.push(d);
                    }
                }
            }
            Cadence::EveryDays { days } => {
                let step = days.max(1);
                let mut d = last + Duration::days(step);
                while d <= to {
                    if d >= from {
                        out.push(d);
                    }
                    d += Duration::days(step);
                }
            }
        }
        out
    }

    /// Events inside the stream that the user may stop or reduce, one per description
    /// (the latest row of that description), in chronological order.
    pub fn flexible_targets(&self) -> Vec<&Occurrence> {
        let mut latest: BTreeMap<&str, &Occurrence> = BTreeMap::new();
        for o in &self.occurrences {
            if o.flexibility != Flexibility::Fixed {
                latest.insert(&o.description, o);
            }
        }
        let mut v: Vec<&Occurrence> = latest.into_values().collect();
        v.sort_by_key(|o| (o.date, super::types::id_rank(&o.event_id)));
        v
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Streams {
    pub as_of: Option<NaiveDate>,
    pub streams: Vec<Stream>,
    /// Settled history rows that belong to no stream (one-offs), for the facts/debugging.
    pub one_off_event_ids: Vec<String>,
}

impl Streams {
    pub fn stream_of_event(&self, event_id: &str) -> Option<&Stream> {
        self.streams.iter().find(|s| s.occurrences.iter().any(|o| o.event_id == event_id))
    }
}

/// Detect streams from settled history strictly before `as_of`.
pub fn detect(ledger: &Ledger, as_of: NaiveDate, rules: &Rules) -> Streams {
    let mut described: BTreeMap<(Direction, String, String), Vec<Occurrence>> = BTreeMap::new();
    let mut pooled: BTreeMap<String, Vec<Occurrence>> = BTreeMap::new();
    let mut one_offs = Vec::new();

    for e in ledger.settled_history(as_of) {
        let ev = &e.event;
        if !is_stream_eligible(ev.event_type, ev.direction) || e.reversed_by.is_some() {
            one_offs.push(ev.id.clone());
            continue;
        }
        let Some(amount) = e.home_amount else { continue };
        let occ = Occurrence {
            event_id: ev.id.clone(),
            description: ev.description.clone(),
            date: ev.event_date,
            amount,
            flexibility: ev.flexibility,
            minimum_allowed_amount: ev.minimum_allowed_amount.and_then(|m| {
                // Same-currency floors pass straight through; foreign floors use the row rate.
                if ev.currency == ledger.home_currency { Some(m) } else { scale_like(m, ev.amount?, amount) }
            }),
        };
        if ev.direction == Direction::Debit && rules.is_variable_category(&ev.category) {
            pooled.entry(ev.category.clone()).or_default().push(occ);
        } else {
            described.entry((ev.direction, ev.category.clone(), ev.description.clone())).or_default().push(occ);
        }
    }

    let mut streams = Vec::new();
    for ((direction, category, description), mut occs) in described {
        occs.sort_by_key(|o| (o.date, super::types::id_rank(&o.event_id)));
        match recurring_cadence(&occs, as_of, rules) {
            Some(cadence) => {
                let amounts: Vec<Cents> = occs.iter().map(|o| o.amount).collect();
                streams.push(Stream {
                    id: format!("rec:{}:{}:{}", direction.as_str(), category, description),
                    kind: StreamKind::Recurring,
                    direction,
                    category,
                    description: Some(description),
                    cadence,
                    projected_amount: rules.bill_estimator.estimate(&amounts),
                    occurrences: occs,
                });
            }
            None => one_offs.extend(occs.into_iter().map(|o| o.event_id)),
        }
    }
    for (category, mut occs) in pooled {
        occs.sort_by_key(|o| (o.date, super::types::id_rank(&o.event_id)));
        match variable_cadence(&occs, as_of, rules) {
            Some(cadence) => {
                let amounts: Vec<Cents> = occs.iter().map(|o| o.amount).collect();
                streams.push(Stream {
                    id: format!("var:{category}"),
                    kind: StreamKind::VariableSpend,
                    direction: Direction::Debit,
                    category,
                    description: None,
                    cadence,
                    projected_amount: rules.variable_estimator.estimate(&amounts),
                    occurrences: occs,
                });
            }
            None => one_offs.extend(occs.into_iter().map(|o| o.event_id)),
        }
    }
    one_offs.sort_by_key(|id| super::types::id_rank(id));
    Streams { as_of: Some(as_of), streams, one_off_event_ids: one_offs }
}

/// Refunds, investment rows and non-cash valuations are never streams.
fn is_stream_eligible(t: EventType, d: Direction) -> bool {
    d != Direction::NonCash
        && !matches!(
            t,
            EventType::Refund
                | EventType::InvestmentPurchase
                | EventType::InvestmentSale
                | EventType::InvestmentValuation
        )
}

/// Cadence for a described bill/income group, confirmed by repeated occurrences and still
/// active at `as_of`.
pub fn recurring_cadence(occs: &[Occurrence], as_of: NaiveDate, rules: &Rules) -> Option<Cadence> {
    if occs.len() < rules.min_stream_occurrences {
        return None;
    }
    let dates: Vec<NaiveDate> = occs.iter().map(|o| o.date).collect();
    let cadence = monthly_cadence(&dates, rules).or_else(|| interval_cadence(&dates, rules))?;
    is_active(&dates, cadence, as_of).then_some(cadence)
}

/// Cadence for a pooled variable-spend category: a fixed interval when the pool is regular,
/// otherwise the mean interval over the recent lookback (a spending rate).
pub fn variable_cadence(occs: &[Occurrence], as_of: NaiveDate, rules: &Rules) -> Option<Cadence> {
    if occs.len() < rules.min_stream_occurrences {
        return None;
    }
    let dates: Vec<NaiveDate> = occs.iter().map(|o| o.date).collect();
    let cadence = interval_cadence(&dates, rules).or_else(|| {
        let tail = &dates[dates.len().saturating_sub(rules.variable_interval_lookback)..];
        let span = (*tail.last()? - tail[0]).num_days();
        let gaps = tail.len() as i64 - 1;
        (gaps > 0 && span > 0).then(|| Cadence::EveryDays { days: ((span + gaps / 2) / gaps).max(1) })
    })?;
    is_active(&dates, cadence, as_of).then_some(cadence)
}

/// Consecutive calendar months with a stable day of month (checked over the whole group).
fn monthly_cadence(dates: &[NaiveDate], rules: &Rules) -> Option<Cadence> {
    let month_index = |d: &NaiveDate| d.year() * 12 + d.month0() as i32;
    let consecutive = dates.windows(2).all(|w| month_index(&w[1]) - month_index(&w[0]) == 1);
    if !consecutive {
        return None;
    }
    let days: Vec<u32> = dates.iter().map(|d| d.day()).collect();
    let (lo, hi) = (*days.iter().min()?, *days.iter().max()?);
    (hi - lo <= rules.monthly_dom_tolerance).then(|| Cadence::Monthly { day: *days.last().unwrap() })
}

/// Near-constant gaps in days.
fn interval_cadence(dates: &[NaiveDate], rules: &Rules) -> Option<Cadence> {
    let gaps: Vec<i64> = dates.windows(2).map(|w| (w[1] - w[0]).num_days()).collect();
    let (lo, hi) = (*gaps.iter().min()?, *gaps.iter().max()?);
    if lo <= 0 || hi - lo > rules.interval_tolerance_days {
        return None;
    }
    let mean = (gaps.iter().sum::<i64>() + gaps.len() as i64 / 2) / gaps.len() as i64;
    Some(Cadence::EveryDays { days: mean })
}

/// A stream is still active if its next expected occurrence is not long overdue.
fn is_active(dates: &[NaiveDate], cadence: Cadence, as_of: NaiveDate) -> bool {
    let Some(last) = dates.last() else { return false };
    let period = match cadence {
        Cadence::Monthly { .. } => 31,
        Cadence::EveryDays { days } => days,
    };
    (as_of - *last).num_days() <= 2 * period
}

fn clamp_day(y: i32, m: u32, day: u32) -> NaiveDate {
    let mut d = day;
    loop {
        if let Some(date) = NaiveDate::from_ymd_opt(y, m, d) {
            return date;
        }
        d -= 1;
    }
}

/// Convert a foreign-currency floor with the same ratio as its row's amount conversion.
fn scale_like(value: Cents, row_amount: Cents, home_amount: Cents) -> Option<Cents> {
    if row_amount.0 == 0 {
        return None;
    }
    Some(Cents(((value.0 as i128 * home_amount.0 as i128) / row_amount.0 as i128) as i64))
}
