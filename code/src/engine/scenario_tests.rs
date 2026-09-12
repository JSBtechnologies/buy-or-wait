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

#[test]
fn evidence_records_serde_round_trip() {
    use super::ledger::{EvidenceRecord, EvidenceSource, Fact};
    let at = d("2025-07-29").and_hms_opt(9, 30, 0).unwrap();
    let m = Money::from_f64(1422.85);
    let facts = vec![
        Fact::EventAmount { event_id: "event_1".into(), amount: m, currency: "INR".into() },
        Fact::EventCancelled { event_id: "event_1".into() },
        Fact::EventSettled { event_id: "event_1".into(), amount: Some(m), date: Some(d("2025-08-01")) },
        Fact::EventAmended { event_id: "event_1".into(), amount: None, date: Some(d("2025-08-01")) },
        Fact::DuplicateOf { event_id: "event_1".into(), of_event_id: Some("event_0".into()) },
        Fact::OwnAccountTransfer { event_id: "event_1".into() },
        Fact::IncomeAmountChange { category: "salary".into(), amount: m, currency: "EUR".into(), effective: d("2025-08-15") },
        Fact::IncomeStarts { category: "salary".into(), amount: m, currency: "EUR".into(), first_date: d("2025-08-15") },
        Fact::NextIncomeAmount { category: "salary".into(), amount: m, currency: "EUR".into(), date: None },
        Fact::IncomeDateMoved { category: "salary".into(), new_date: d("2025-08-23") },
        Fact::IncomeEnded { category: "salary".into(), effective: d("2025-08-01"), description: Some("Second household income".into()) },
        Fact::NewRecurringExpense { description: "Childcare".into(), category: "family_support".into(), amount: m, currency: "EUR".into(), first_date: d("2025-08-01"), every_days: None },
        Fact::ExpenseAmountChange { category: "rent".into(), amount: None, percent: Some(12.0), currency: None, effective: None },
        Fact::OneTimeFlow { direction: Direction::Credit, category: "salary".into(), amount: m, currency: "EUR".into(), date: d("2025-08-20") },
        Fact::Unconfirmed { category: "bonus".into(), amount: None, currency: None },
    ];
    for (i, fact) in facts.into_iter().enumerate() {
        let rec = EvidenceRecord {
            record_id: format!("message_{i}#0"),
            source: if i == 0 { EvidenceSource::Image } else { EvidenceSource::Message { source_type: "employer".into() } },
            observed_at: at,
            fact,
        };
        let json = serde_json::to_string(&rec).unwrap();
        let back: EvidenceRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(back, rec, "{json}");
    }
    // Extraction's on-disk shape (code/store/evidence/user_02.json); IncomeEnded written
    // before the selector existed still parses with description = None.
    let disk = r#"[{"record_id":"message_01#0","source":{"Message":{"source_type":"employer"}},"observed_at":"2025-07-29T09:30:00",
        "fact":{"IncomeAmountChange":{"category":"salary","amount":427500000000,"currency":"IDR","effective":"2025-08-15"}}},
        {"record_id":"message_09#0","source":{"Message":{"source_type":"employer"}},"observed_at":"2026-03-01T09:30:00",
        "fact":{"IncomeEnded":{"category":"salary","effective":"2026-03-01"}}}]"#;
    let recs: Vec<EvidenceRecord> = serde_json::from_str(disk).unwrap();
    assert!(matches!(&recs[0].fact, Fact::IncomeAmountChange { amount, .. } if *amount == Money::from_units(42_750_000)));
    assert!(matches!(&recs[1].fact, Fact::IncomeEnded { description: None, .. }));
}

#[test]
fn income_ended_selector_stops_only_the_matching_stream() {
    use super::ledger::{EvidenceRecord, EvidenceSource, Fact};
    use Direction::*;
    let mut events = history(EventType::Income, "Payroll credit", "salary", Credit, 2000.0, 15, 1);
    events.extend(history(EventType::Income, "Second household income", "salary", Credit, 500.0, 20, 11));
    let rules = Rules::default();
    let rates = Arc::new(RateTable::default());
    let evidence = vec![EvidenceRecord {
        record_id: "message_x#0".into(),
        source: EvidenceSource::Message { source_type: "employer".into() },
        observed_at: d("2025-08-01").and_hms_opt(9, 0, 0).unwrap(),
        fact: Fact::IncomeEnded { category: "salary".into(), effective: d("2025-08-01"), description: Some("second household income".into()) },
    }];
    let ledger = Ledger::build("INR", &events, &evidence, rates.as_ref(), &rules);
    let streams = recurrence::detect(&ledger, d("2025-08-05"), &rules);
    let inputs = ForecastInputs {
        ledger: &ledger,
        streams: &streams,
        opening_balance: Money::ZERO,
        minimum_balance: Money::ZERO,
        start: d("2025-08-05"),
        rates: rates.as_ref(),
        rules: &rules,
    };
    let f = Forecast::build(&inputs, &[]);
    let amounts: Vec<Money> = f.flows.iter().filter(|x| x.category == "salary").map(|x| x.amount).collect();
    assert_eq!(amounts, vec![Money::from_units(2000); 3]);
}

/// verify#65 on the real data: request_44's scheduled utility debit (event_date 02-04,
/// settles 02-11) replaces the 02-05 utilities projection; no stream occurrence of a
/// scheduled row's category lands within 15 days of it.
#[test]
fn request_44_scheduled_utility_projected_once() {
    use crate::engine::session::Session;
    use crate::model;
    let ds = std::path::Path::new("../dataset");
    let profiles = model::load_financial_profiles(ds.join("financial_profiles.csv")).unwrap();
    let events = model::load_financial_events(ds.join("financial_events.csv")).unwrap();
    let rates = Arc::new(RateTable::from_model(&model::load_exchange_rates(ds.join("exchange_rates.csv")).unwrap()));
    let req = model::load_requests(ds.join("requests.csv")).unwrap().into_iter().find(|r| r.request_id == "request_44").unwrap();
    let session = Session::from_model(&req.user_id, &profiles, &events, rates, Rules::default()).unwrap();
    let dec = session.decide(&req.request_id, req.request_date, &RequestSpec::from_model(&req), &[]).unwrap();
    let flows = &dec.baseline.flows;
    let scheduled: Vec<_> = flows.iter().filter(|f| matches!(f.source, FlowSource::Scheduled { .. })).collect();
    assert!(scheduled.iter().any(|s| s.category == "utilities" && s.date == d("2025-02-11")));
    for s in scheduled {
        let doubles: Vec<_> = flows
            .iter()
            .filter(|f| matches!(f.source, FlowSource::Stream { .. }) && f.category == s.category && f.amount.0.signum() == s.amount.0.signum())
            .filter(|f| (f.date - s.date).num_days().abs() <= 15)
            .collect();
        assert!(doubles.is_empty(), "{} {:?} doubled by {:?}", s.category, s.date, doubles);
    }
}

/// RULES S6.1 (extraction #125): IncomeDateMoved re-anchors ALL later months, not only the
/// next occurrence; a month-end move clamps per month.
#[test]
fn income_date_moved_reanchors_every_later_month() {
    use super::ledger::{EvidenceRecord, EvidenceSource, Fact};
    let run = |new_date: &str, rd: &str| {
        let events = history(EventType::Income, "Payroll credit", "salary", Direction::Credit, 149000.0, 15, 1);
        let rules = Rules::default();
        let rates = Arc::new(RateTable::default());
        let evidence = vec![EvidenceRecord {
            record_id: "message_05#0".into(),
            source: EvidenceSource::Message { source_type: "employer".into() },
            observed_at: d("2025-07-29").and_hms_opt(9, 30, 0).unwrap(),
            fact: Fact::IncomeDateMoved { category: "salary".into(), new_date: d(new_date) },
        }];
        let ledger = Ledger::build("INR", &events, &evidence, rates.as_ref(), &rules);
        let streams = recurrence::detect(&ledger, d(rd), &rules);
        let inputs = ForecastInputs {
            ledger: &ledger,
            streams: &streams,
            opening_balance: Money::ZERO,
            minimum_balance: Money::ZERO,
            start: d(rd),
            rates: rates.as_ref(),
            rules: &rules,
        };
        let f = Forecast::build(&inputs, &[]);
        f.flows.iter().filter(|x| x.category == "salary").map(|x| x.date).collect::<Vec<_>>()
    };
    assert_eq!(run("2025-08-23", "2025-08-05"), vec![d("2025-08-23"), d("2025-09-23"), d("2025-10-23")]);
    assert_eq!(run("2025-08-31", "2025-08-05"), vec![d("2025-08-31"), d("2025-09-30"), d("2025-10-31")]);
}
