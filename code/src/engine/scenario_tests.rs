//! Hand-computed scenario tests for risks the samples cannot catch (board risk.fx,
//! risk.double_count): one FX conversion per currency pair, and each scheduled-row double
//! count trap projected exactly once.

use std::sync::Arc;

use chrono::NaiveDate;

use super::forecast::{FlowSource, Forecast, ForecastInputs};
use super::ledger::{to_home, Ledger};
use super::money::{DecimalRate, Money};
use super::recurrence;
use super::rules::Rules;
use super::types::*;

fn d(s: &str) -> NaiveDate {
    NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
}

fn rates() -> RateTable {
    let mut t = RateTable::default();
    for (from, to, r) in [("EUR", "ZAR", 20.0), ("USD", "IDR", 15833.33), ("USD", "INR", 83.33), ("EUR", "USD", 1.09), ("USD", "EUR", 0.92)] {
        t.insert(d("2024-09-15"), from, to, DecimalRate::from_f64(r));
    }
    t
}

#[test]
fn fx_one_conversion_per_pair() {
    let r = rates();
    let day = d("2024-09-15");
    let conv = |amt: f64, from: &str, home: &str| to_home(Money::from_f64(amt), from, home, day, &r).unwrap();
    // 12.34 * 20 = 246.8
    assert_eq!(conv(12.34, "EUR", "ZAR"), Money::from_f64(246.8));
    // 1800 * 15833.33 = 28,499,994
    assert_eq!(conv(1800.0, "USD", "IDR"), Money::from_units(28_499_994));
    // 2047.68 * 83.33 = 170,633.1744 (kept at full precision)
    assert_eq!(conv(2047.68, "USD", "INR"), Money(1_706_331_744));
    // 99.99 * 1.09 = 108.9891
    assert_eq!(conv(99.99, "EUR", "USD"), Money(1_089_891));
    // 55.55 * 0.92 = 51.106
    assert_eq!(conv(55.55, "USD", "EUR"), Money(511_060));
    // Home currency passes through; a missing pair is not guessed.
    assert_eq!(conv(10.0, "INR", "INR"), Money::from_units(10));
    assert_eq!(to_home(Money::from_units(1), "ZAR", "IDR", day, &r), None);
    // A projected date without a row uses the latest earlier row for the pair; no earlier row
    // means no conversion.
    let later = d("2024-10-15");
    assert_eq!(to_home(Money::from_units(10), "EUR", "ZAR", later, &r), Some(Money::from_units(200)));
    assert_eq!(to_home(Money::from_units(10), "EUR", "ZAR", d("2024-09-14"), &r), None);
}

fn ev(id: u32, ty: EventType, desc: &str, cat: &str, dir: Direction, amt: f64, date: &str, status: Status) -> Event {
    Event {
        id: format!("event_{id}"),
        event_type: ty,
        description: desc.into(),
        category: cat.into(),
        direction: dir,
        amount: Some(Money::from_f64(amt)),
        currency: "INR".into(),
        event_date: d(date),
        settlement_date: Some(d(date)),
        status,
        linked_event_id: None,
        flexibility: Flexibility::Fixed,
        minimum_allowed_amount: None,
    }
}

fn forecast_for(events: &[Event], rd: &str) -> Forecast {
    let rules = Rules::default();
    let rates = Arc::new(RateTable::default());
    let ledger = Ledger::build("INR", events, &[], rates.as_ref(), &rules);
    let streams = recurrence::detect(&ledger, d(rd), &rules);
    let inputs = ForecastInputs {
        ledger: &ledger,
        streams: &streams,
        opening_balance: Money::from_units(100_000),
        minimum_balance: Money::ZERO,
        start: d(rd),
        rates: rates.as_ref(),
        rules: &rules,
    };
    Forecast::build(&inputs, &[])
}

fn history(ty: EventType, desc: &str, cat: &str, dir: Direction, amt: f64, day: u32, first_id: u32) -> Vec<Event> {
    (4..=7).enumerate().map(|(i, m)| ev(first_id + i as u32, ty, desc, cat, dir, amt, &format!("2025-{m:02}-{day:02}"), Status::Settled)).collect()
}

#[test]
fn shifted_scheduled_salary_replaces_that_months_occurrence() {
    use Direction::*;
    let mut events = history(EventType::Income, "Payroll credit", "salary", Credit, 124000.0, 15, 1);
    events.push(ev(9, EventType::Income, "Next confirmed salary", "salary", Credit, 124000.0, "2025-08-23", Status::Scheduled));
    let f = forecast_for(&events, "2025-08-05");
    let salary: Vec<NaiveDate> = f.flows.iter().filter(|x| x.category == "salary").map(|x| x.date).collect();
    // August once (the scheduled row on the 23rd); later months re-anchor to the 23rd (S3.4(c)).
    assert_eq!(salary, vec![d("2025-08-23"), d("2025-09-23"), d("2025-10-23")]);
}

#[test]
fn scheduled_bill_settling_days_off_replaces_the_stream_occurrence() {
    use Direction::*;
    // verifier#65 (requests 44/104/164/224): stream on the 5th, scheduled debit settles the 11th.
    let mut events = history(EventType::Expense, "Municipal utilities", "utilities", Debit, 4830.0, 5, 1);
    let mut s = ev(9, EventType::Expense, "Scheduled utility debit", "utilities", Debit, 4830.0, "2025-08-04", Status::Scheduled);
    s.settlement_date = Some(d("2025-08-11"));
    events.push(s);
    let f = forecast_for(&events, "2025-08-04");
    let util: Vec<NaiveDate> = f.flows.iter().filter(|x| x.category == "utilities").map(|x| x.date).collect();
    assert_eq!(util, vec![d("2025-08-11"), d("2025-09-05"), d("2025-10-05")]);
}

#[test]
fn scheduled_bill_and_retry_replace_the_stream_cycle() {
    use Direction::*;
    let mut events = history(EventType::Expense, "Municipal utilities", "utilities", Debit, 4830.0, 7, 1);
    // A failed August debit and its scheduled retry: only the retry counts, and it is the
    // August cycle, so the stream does not also project August.
    events.push(ev(9, EventType::DebtPayment, "Failed bill payment attempt", "utilities", Debit, 4830.0, "2025-08-06", Status::Failed));
    let mut retry = ev(10, EventType::DebtPayment, "Scheduled bill payment retry", "utilities", Debit, 4830.0, "2025-08-08", Status::Scheduled);
    retry.settlement_date = Some(d("2025-08-12"));
    retry.linked_event_id = Some("event_9".into());
    events.push(retry);
    let f = forecast_for(&events, "2025-08-07");
    let util: Vec<(NaiveDate, bool)> = f
        .flows
        .iter()
        .filter(|x| x.category == "utilities")
        .map(|x| (x.date, matches!(x.source, FlowSource::Scheduled { .. })))
        .collect();
    assert_eq!(util, vec![(d("2025-08-12"), true), (d("2025-09-07"), false), (d("2025-10-07"), false)]);
}
