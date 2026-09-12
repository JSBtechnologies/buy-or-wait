//! Recurring-stream detection (PLAN.md §2.2, RULES S3.2–S3.4) from settled ledger history.
//!
//! Signals: description + category + cadence. Two stream kinds:
//! - `Recurring`: bills/income/subscriptions whose rows of one description recur monthly
//!   (every gap 28–31 days), projected on the day of month of the last row.
//! - `VariableSpend`: a debit category spread over several descriptions (groceries, transport,
//!   dining...) recurring on a fixed interval, projected every `step` days as a spending rate.
//!
//! Anything else (one-time purchases, transfers, refunds, arrears, bonuses, irregular payouts)
//! is not a stream and is never projected.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{Datelike, Duration, NaiveDate};
use serde::{Deserialize, Serialize};

use super::ledger::{CashTreatment, Ledger};
use super::money::Money;
use super::rules::Rules;
use super::types::{id_rank, Direction, EventType, Flexibility};

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
    pub amount: Money,
    pub flexibility: Flexibility,
    /// Home-currency floor for reduce_to, when the row carries one.
    pub minimum_allowed_amount: Option<Money>,
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
    /// Chronological supporting history (a scheduled anchor may be the last entry).
    pub occurrences: Vec<Occurrence>,
    /// Home-currency magnitude of each projected occurrence (before spending changes).
    pub projected_amount: Money,
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

    /// The first occurrence expected after the last observed one.
    pub fn next_expected(&self) -> NaiveDate {
        next_after(self.cadence, self.last_date())
    }

    /// Projected occurrence dates in `[from, to]` (inclusive), strictly after the last
    /// observed occurrence.
    pub fn dates_between(&self, from: NaiveDate, to: NaiveDate) -> Vec<NaiveDate> {
        let mut out = Vec::new();
        let mut d = self.last_date();
        loop {
            d = next_after(self.cadence, d);
            if d > to {
                break;
            }
            if d >= from {
                out.push(d);
            }
        }
        out
    }

    /// The events the user may stop or reduce in this stream: the latest settled row of each
    /// flexible description (S1.2: "latest settled occurrence of the stream").
    pub fn flexible_targets(&self) -> Vec<&Occurrence> {
        let mut latest: BTreeMap<&str, &Occurrence> = BTreeMap::new();
        for o in &self.occurrences {
            if o.flexibility != Flexibility::Fixed {
                latest.insert(&o.description, o);
            }
        }
        let mut v: Vec<&Occurrence> = latest.into_values().collect();
        v.sort_by_key(|o| (o.date, id_rank(&o.event_id)));
        v
    }
}

fn next_after(cadence: Cadence, d: NaiveDate) -> NaiveDate {
    match cadence {
        Cadence::Monthly { day } => {
            let (y, m) = if d.month() == 12 { (d.year() + 1, 1) } else { (d.year(), d.month() + 1) };
            clamp_day(y, m, day)
        }
        Cadence::EveryDays { days } => d + Duration::days(days.max(1)),
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Streams {
    pub as_of: Option<NaiveDate>,
    pub streams: Vec<Stream>,
    /// Streams detected but not projected, with the reason (income ended, missed, one-off).
    pub inactive: Vec<(String, String)>,
}

impl Streams {
    pub fn stream_of_event(&self, event_id: &str) -> Option<&Stream> {
        self.streams.iter().find(|s| s.occurrences.iter().any(|o| o.event_id == event_id))
    }
}

/// Detect streams from settled history with settlement before `as_of` (RULES S3.2).
pub fn detect(ledger: &Ledger, as_of: NaiveDate, rules: &Rules) -> Streams {
    let mut groups: BTreeMap<(String, Direction), Vec<Occurrence>> = BTreeMap::new();
    for e in &ledger.entries {
        let ev = &e.event;
        let eligible = e.treatment == CashTreatment::Settled
            && e.cash_date < as_of
            && e.chain_root.is_none()
            && matches!(ev.event_type, EventType::Expense | EventType::Subscription | EventType::DebtPayment | EventType::Income)
            && ev.direction != Direction::NonCash;
        let Some(amount) = e.home_amount.filter(|_| eligible) else { continue };
        groups.entry((ev.category.clone(), ev.direction)).or_default().push(Occurrence {
            event_id: ev.id.clone(),
            description: ev.description.clone(),
            date: e.cash_date,
            amount,
            flexibility: ev.flexibility,
            minimum_allowed_amount: ev.minimum_allowed_amount.and_then(|m| {
                if ev.currency == ledger.home_currency { Some(m) } else { scale_like(m, e.amount?, amount) }
            }),
        });
    }

    let mut out = Streams { as_of: Some(as_of), ..Default::default() };
    for ((category, direction), mut occs) in groups {
        occs.sort_by_key(|o| (o.date, id_rank(&o.event_id)));
        let descriptions: BTreeSet<&str> = occs.iter().map(|o| o.description.as_str()).collect();
        if direction == Direction::Debit && descriptions.len() > 1 {
            if occs.len() < rules.min_stream_occurrences {
                continue;
            }
            if let Some(step) = interval_step(&occs) {
                let amounts: Vec<Money> = occs.iter().map(|o| o.amount).collect();
                out.streams.push(Stream {
                    id: format!("var:{category}"),
                    kind: StreamKind::VariableSpend,
                    direction,
                    category,
                    description: None,
                    cadence: Cadence::EveryDays { days: step },
                    projected_amount: rules.variable_estimator.estimate(&amounts),
                    occurrences: occs,
                });
            }
            continue;
        }
        let mut by_desc: BTreeMap<String, Vec<Occurrence>> = BTreeMap::new();
        for o in occs {
            by_desc.entry(o.description.clone()).or_default().push(o);
        }
        for (description, occs) in by_desc {
            let id = format!("rec:{}:{}:{}", direction.as_str(), category, description);
            if occs.len() < rules.min_stream_occurrences || !is_monthly(&occs, rules) {
                continue;
            }
            let income = direction == Direction::Credit;
            if income && rules.is_one_off_income(&description) {
                out.inactive.push((id, "one-off income description".into()));
                continue;
            }
            let amounts: Vec<Money> = occs.iter().map(|o| o.amount).collect();
            let estimator = if income { rules.income_estimator } else { rules.bill_estimator };
            let stream = Stream {
                id,
                kind: StreamKind::Recurring,
                direction,
                category: category.clone(),
                description: Some(description),
                cadence: Cadence::Monthly { day: occs.last().unwrap().date.day() },
                projected_amount: estimator.estimate(&amounts),
                occurrences: occs,
            };
            if income {
                if occs_end(&stream, rules) {
                    out.inactive.push((stream.id, "income ended (final payroll)".into()));
                    continue;
                }
                if rules.stop_income_after_missed_occurrence && stream.next_expected() < as_of {
                    let missed = stream.next_expected();
                    out.inactive.push((stream.id, format!("missed expected {missed}")));
                    continue;
                }
            }
            out.streams.push(stream);
        }
    }
    anchor_scheduled_income(ledger, &mut out.streams, rules);
    out
}

fn occs_end(s: &Stream, rules: &Rules) -> bool {
    s.occurrences.last().is_some_and(|o| rules.is_income_end(&o.description))
}

/// S3.2 monthly: every gap between consecutive rows within the configured range.
fn is_monthly(occs: &[Occurrence], rules: &Rules) -> bool {
    let (lo, hi) = rules.monthly_gap_days;
    occs.windows(2).all(|w| (lo..=hi).contains(&(w[1].date - w[0].date).num_days()))
}

/// S3.2 interval: the modal positive gap (smallest on ties), valid only if every gap is a
/// multiple of it (a skipped week is still the stream).
fn interval_step(occs: &[Occurrence]) -> Option<i64> {
    let gaps: Vec<i64> = occs.windows(2).map(|w| (w[1].date - w[0].date).num_days()).collect();
    let mut counts: BTreeMap<i64, usize> = BTreeMap::new();
    for &g in gaps.iter().filter(|&&g| g > 0) {
        *counts.entry(g).or_default() += 1;
    }
    let step = counts.iter().max_by(|a, b| a.1.cmp(b.1).then(b.0.cmp(a.0))).map(|(g, _)| *g)?;
    gaps.iter().all(|g| g % step == 0).then_some(step)
}

/// RULES S2.1/S3.4: a scheduled income row (`Next confirmed salary`) *is* its month's
/// occurrence of the income stream, and the stream continues monthly afterwards at the
/// scheduled amount. With no active stream (e.g. only a prorated first salary) the row seeds one.
fn anchor_scheduled_income(ledger: &Ledger, streams: &mut Vec<Stream>, rules: &Rules) {
    let scheduled = ledger
        .scheduled()
        .filter(|e| e.event.direction == Direction::Credit && e.event.event_type == EventType::Income)
        .filter(|e| !rules.is_one_off_income(&e.event.description));
    for e in scheduled {
        let Some(amount) = e.home_amount else { continue };
        let occ = Occurrence {
            event_id: e.event.id.clone(),
            description: e.event.description.clone(),
            date: e.cash_date,
            amount,
            flexibility: e.event.flexibility,
            minimum_allowed_amount: None,
        };
        let existing = streams
            .iter_mut()
            .filter(|s| s.kind == StreamKind::Recurring && s.direction == Direction::Credit && s.category == e.event.category)
            .max_by_key(|s| s.last_date());
        match existing {
            Some(s) if s.last_date() < e.cash_date => {
                s.occurrences.push(occ);
                s.projected_amount = amount;
            }
            Some(_) => {}
            None => streams.push(Stream {
                id: format!("sched:credit:{}:{}", e.event.category, e.event.id),
                kind: StreamKind::Recurring,
                direction: Direction::Credit,
                category: e.event.category.clone(),
                description: Some(e.event.description.clone()),
                cadence: Cadence::Monthly { day: e.cash_date.day() },
                occurrences: vec![occ],
                projected_amount: amount,
            }),
        }
    }
}

fn clamp_day(y: i32, m: u32, day: u32) -> NaiveDate {
    let mut d = day.min(31);
    loop {
        if let Some(date) = NaiveDate::from_ymd_opt(y, m, d) {
            return date;
        }
        d -= 1;
    }
}

/// Convert a foreign-currency floor with the same ratio as its row's amount conversion.
fn scale_like(value: Money, row_amount: Money, home_amount: Money) -> Option<Money> {
    if row_amount.0 == 0 {
        return None;
    }
    Some(Money(((value.0 as i128 * home_amount.0 as i128) / row_amount.0 as i128) as i64))
}
