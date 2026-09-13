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
    forecast_with(events, rd, &Rules::default())
}

/// RULES S2.3 ordering (every debit before same-day credits), the pre-S8 default.
fn debits_first() -> Rules {
    Rules { salary_day_order: super::rules::SalaryDayOrder::DebitsFirst, ..Rules::default() }
}

fn forecast_with(events: &[Event], rd: &str, rules: &Rules) -> Forecast {
    let rates = Arc::new(RateTable::default());
    let ledger = Ledger::build("INR", events, &[], rates.as_ref(), rules);
    let streams = recurrence::detect(&ledger, d(rd), rules);
    let inputs = ForecastInputs {
        ledger: &ledger,
        streams: &streams,
        opening_balance: Money::from_units(100_000),
        minimum_balance: Money::ZERO,
        start: d(rd),
        rates: rates.as_ref(),
        rules,
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
        Fact::UnverifiedEventAmount { event_id: "event_1".into(), amount: m, currency: "INR".into(), reason: "no_witness".into() },
        Fact::AmountWitness { event_id: "event_1".into(), accepted: m, computed: m, currency: "INR".into(), witness: "sum".into() },
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
    let run = |rules: Rules| {
        let session = Session::from_model(&req.user_id, &profiles, &events, rates.clone(), rules).unwrap();
        let dec = session.decide(&req.request_id, req.request_date, &RequestSpec::from_model(&req), &[]).unwrap();
        let flows = dec.baseline.flows.clone();
        let scheduled: Vec<_> = flows.iter().filter(|f| matches!(f.source, FlowSource::Scheduled { .. })).cloned().collect();
        assert!(scheduled.iter().any(|s| s.category == "utilities" && s.date == d("2025-02-11")));
        scheduled
            .iter()
            .map(|s| {
                let near = flows
                    .iter()
                    .filter(|f| matches!(f.source, FlowSource::Stream { .. }) && f.category == s.category && f.amount.0.signum() == s.amount.0.signum())
                    .filter(|f| (f.date - s.date).num_days().abs() <= 15)
                    .count();
                (s.category.clone(), near)
            })
            .collect::<Vec<_>>()
    };
    // S3.4(c) category_window: the scheduled row replaces the cycle, never doubled.
    let rules = Rules { scheduled_replace_scope: super::rules::ScheduledReplaceScope::CategoryWindow, ..Rules::default() };
    assert!(run(rules).iter().all(|(_, n)| *n == 0));
    // S8.3 lifecycle_or_amount (default): the unlinked utility row is 39% off the stream
    // estimate, so it is additive: exactly one stream occurrence stays next to it.
    assert!(run(Rules::default()).iter().any(|(c, n)| c == "utilities" && *n == 1));
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

/// board decision.accuracy_first: malformed forecast-level facts are rejected and recorded,
/// never half-applied; a valid one is kept.
#[test]
fn malformed_forecast_facts_are_rejected_not_applied() {
    use super::ledger::{EvidenceRecord, EvidenceSource, Fact};
    let at = d("2025-08-01").and_hms_opt(9, 0, 0).unwrap();
    let rec = |id: &str, fact: Fact| EvidenceRecord {
        record_id: id.into(),
        source: EvidenceSource::Message { source_type: "employer".into() },
        observed_at: at,
        fact,
    };
    let evidence = vec![
        rec("ok", Fact::IncomeAmountChange { category: "salary".into(), amount: Money::from_units(10), currency: "INR".into(), effective: d("2025-09-01") }),
        rec("zero", Fact::IncomeStarts { category: "salary".into(), amount: Money::ZERO, currency: "INR".into(), first_date: d("2025-09-01") }),
        rec("no_rate", Fact::NextIncomeAmount { category: "salary".into(), amount: Money::from_units(10), currency: "GBP".into(), date: None }),
        rec("both", Fact::ExpenseAmountChange { category: "rent".into(), amount: Some(Money::from_units(1)), percent: Some(5.0), currency: None, effective: None }),
        rec("pct", Fact::ExpenseAmountChange { category: "rent".into(), amount: None, percent: Some(-150.0), currency: None, effective: None }),
        rec("nocat", Fact::IncomeEnded { category: " ".into(), effective: d("2025-09-01"), description: None }),
    ];
    let ledger = Ledger::build("INR", &[], &evidence, &RateTable::default(), &Rules::default());
    let kept: Vec<&str> = ledger.adjustments.iter().map(|a| a.record_id.as_str()).collect();
    let mut rejected: Vec<&str> = ledger.rejected.iter().map(|r| r.record_id.as_str()).collect();
    rejected.sort();
    assert_eq!(kept, vec!["ok"]);
    assert_eq!(rejected, vec!["both", "no_rate", "nocat", "pct", "zero"]);
}

/// Trough drivers reconcile: starting balance + drivers == trough, and the trough day's
/// credit (salary) is excluded because the trough is that day's pre-credit low.
#[test]
fn trough_drivers_reconcile_to_the_trough() {
    let mut events = history(EventType::Income, "Payroll credit", "salary", Direction::Credit, 5000.0, 15, 1);
    events.extend(history(EventType::Expense, "Monthly rent", "rent", Direction::Debit, 3000.0, 3, 11));
    events.extend(history(EventType::Expense, "Clinic payment", "healthcare", Direction::Debit, 800.0, 15, 21));
    let f = forecast_with(&events, "2025-08-01", &debits_first());
    let (trough, tdate) = f.trough();
    let drivers = f.trough_drivers();
    let total: Money = drivers.iter().map(|d| d.total).sum();
    assert_eq!(f.opening_balance + total, trough);
    assert_eq!(tdate, d("2025-08-15"));
    assert!(drivers.iter().all(|d| d.category != "salary"));
    assert_eq!(drivers[0].category, "rent");
}

/// Verifier class A plumbing: per-kind same-day placement is config-only, moves the day's low,
/// and trough drivers still reconcile under either placement.
#[test]
fn same_day_placement_per_debit_kind() {
    use super::forecast::{Flow, FlowSource};
    use super::rules::{DebitKind, Placement};
    let start = d("2025-08-01");
    let flow = |date: &str, amt: i64, cat: &str, src: FlowSource| Flow { date: d(date), amount: Money::from_units(amt), category: cat.into(), source: src };
    let flows = vec![
        flow("2025-08-15", -300, "groceries", FlowSource::Stream { stream_id: "var:groceries".into() }),
        flow("2025-08-15", -200, "utilities", FlowSource::Stream { stream_id: "rec:debit:utilities:Bill".into() }),
        flow("2025-08-15", 1000, "salary", FlowSource::Stream { stream_id: "rec:credit:salary:Payroll".into() }),
    ];
    let mut rules = debits_first();
    let build = |rules: &Rules| {
        Forecast::from_flows_placed(start, Money::from_units(600), Money::ZERO, Money::ZERO, flows.clone(), 31, &|f| {
            rules.placement_of(super::forecast::debit_kind(f, &[]))
        })
    };
    // Default: both debits before the salary -> low 100 on the 15th.
    let f = build(&rules);
    assert_eq!(f.trough(), (Money::from_units(100), d("2025-08-15")));
    // Variable spend after credits: only the bill precedes the salary -> low 400.
    rules.same_day_placement.variable_spend = Placement::AfterCredits;
    assert_eq!(rules.placement_of(DebitKind::VariableSpend), Placement::AfterCredits);
    assert_eq!(rules.placement_of(DebitKind::FixedBill), Placement::BeforeCredits);
    let f2 = build(&rules);
    assert_eq!(f2.trough(), (Money::from_units(400), d("2025-08-15")));
    for fc in [&f, &f2] {
        let (trough, _) = fc.trough();
        let sum: Money = fc.trough_drivers().iter().map(|t| t.total).sum();
        assert_eq!(fc.opening_balance + sum, trough);
    }
    // JSON patch with the RULES-style name.
    let patched: Rules =
        serde_json::from_str(r#"{"SALARY_DAY_ORDER":"debits_first","SAME_DAY_PLACEMENT":{"scheduled":"after_credits"}}"#).unwrap();
    assert_eq!(patched.placement_of(DebitKind::Scheduled), Placement::AfterCredits);
    assert_eq!(patched.placement_of(DebitKind::VariableSpend), Placement::BeforeCredits);
}

/// RULES S8.1 SALARY_DAY_ORDER=fixed_bills_after_credit: on salary day a fixed bill lands after
/// the credit, while a flexible bill and variable spend stay before it.
#[test]
fn salary_day_order_fixed_bills_after_credit() {
    use super::rules::{DebitKind::*, Placement::*};
    let rules: Rules = serde_json::from_str(r#"{"SALARY_DAY_ORDER":"fixed_bills_after_credit"}"#).unwrap();
    for (kind, want) in [(FixedBill, AfterCredits), (Scheduled, AfterCredits), (Reserved, AfterCredits), (FlexibleBill, BeforeCredits), (VariableSpend, BeforeCredits), (Evidence, BeforeCredits)] {
        assert_eq!(rules.placement_of(kind), want, "{kind:?}");
    }
    let all: Rules = serde_json::from_str(r#"{"SALARY_DAY_ORDER":"bills_after_credit"}"#).unwrap();
    assert_eq!(all.placement_of(FlexibleBill), AfterCredits);
    assert_eq!(all.placement_of(VariableSpend), BeforeCredits);

    // Salary 5,000 and a fixed 3,000 bill on the 15th, a flexible 800 bill on the 15th too.
    let mut events = history(EventType::Income, "Payroll credit", "salary", Direction::Credit, 5000.0, 15, 1);
    events.extend(history(EventType::Expense, "Monthly entertainment spend", "entertainment", Direction::Debit, 3000.0, 15, 11));
    let mut flex = history(EventType::Expense, "Cinema and events", "leisure", Direction::Debit, 800.0, 15, 21);
    flex.iter_mut().for_each(|e| e.flexibility = Flexibility::Reducible);
    events.extend(flex);
    let base = forecast_with(&events, "2025-08-01", &debits_first());
    let moved = forecast_with(&events, "2025-08-01", &rules);
    assert_eq!(Rules::default().salary_day_order, rules.salary_day_order);
    let day = base.day(d("2025-08-15")).unwrap();
    // Default: 100,000 - 3,800 before the salary. Fixed bill after: only the 800 precedes it.
    assert_eq!(base.low[day], Money::from_units(96_200));
    assert_eq!(moved.low[day], Money::from_units(99_200));
    assert_eq!(base.balance[day], moved.balance[day]);
    for f in [&base, &moved] {
        let sum: Money = f.trough_drivers().iter().map(|t| t.total).sum();
        assert_eq!(f.opening_balance + sum, f.trough().0);
    }
}

/// RULES S8.2 VAR_LONG_PHASE=from_request: a >=21-day variable stream restarts at rd + 2.
#[test]
fn var_long_phase_from_request_restarts_at_request_date() {
    let mk = |id, desc: &str, date: &str| ev(id, EventType::Expense, desc, "transport", Direction::Debit, 600.0, date, Status::Settled);
    // Two descriptions, 28-day gaps -> interval stream every 28 days, last row 2025-07-26.
    let events = vec![mk(1, "Fuel", "2025-05-03"), mk(2, "Train pass", "2025-05-31"), mk(3, "Fuel", "2025-06-28"), mk(4, "Train pass", "2025-07-26")];
    let dates = |rules: &Rules| -> Vec<NaiveDate> {
        forecast_with(&events, "2025-08-01", rules).flows.iter().filter(|f| f.category == "transport").map(|f| f.date).collect()
    };
    let last: Rules = serde_json::from_str(r#"{"VAR_LONG_PHASE":"last_settled"}"#).unwrap();
    assert_eq!(dates(&last)[0], d("2025-08-23"));
    assert_eq!(Rules::default().var_long_phase, super::rules::VarLongPhase::MidStep);
    let rules: Rules = serde_json::from_str(r#"{"VAR_LONG_PHASE":"from_request"}"#).unwrap();
    assert_eq!(dates(&rules), vec![d("2025-08-03"), d("2025-08-31"), d("2025-09-28"), d("2025-10-26")]);
    // E2 mid_step: history next (08-23) is later than rd + ceil(28/2) = 08-15, so 08-15.
    let mid: Rules = serde_json::from_str(r#"{"VAR_LONG_PHASE":"mid_step"}"#).unwrap();
    assert_eq!(dates(&mid), vec![d("2025-08-15"), d("2025-09-12"), d("2025-10-10")]);
    // ... and keeps the history phase when it is earlier (rd 08-20: 08-23 < 08-20 + 14).
    let later: Vec<NaiveDate> =
        forecast_with(&events, "2025-08-20", &mid).flows.iter().filter(|f| f.category == "transport").map(|f| f.date).collect();
    assert_eq!(later[0], d("2025-08-23"));
    // Below the minimum step the phase is unchanged.
    let short: Rules = serde_json::from_str(r#"{"VAR_LONG_PHASE":"from_request","var_long_min_step":29}"#).unwrap();
    assert_eq!(dates(&short)[0], d("2025-08-23"));
}

/// RULES S8.3 SCHEDULED_REPLACE_SCOPE=lifecycle_or_amount: an unlinked scheduled debit far from
/// the stream estimate is additive; within 10% (or linked) it still replaces the cycle.
#[test]
fn scheduled_replace_scope_lifecycle_or_amount() {
    use Direction::*;
    let rules: Rules = serde_json::from_str(r#"{"SCHEDULED_REPLACE_SCOPE":"lifecycle_or_amount"}"#).unwrap();
    let run = |amount: f64, linked: bool, rules: &Rules| -> usize {
        let mut events = history(EventType::Expense, "Municipal utilities", "utilities", Debit, 8000.0, 10, 1);
        events.push(ev(8, EventType::Expense, "Municipal utilities", "utilities", Debit, 8000.0, "2025-07-20", Status::Failed));
        let mut s = ev(9, EventType::Expense, "Scheduled utility debit", "utilities", Debit, amount, "2025-08-05", Status::Scheduled);
        if linked {
            s.linked_event_id = Some("event_8".into());
        }
        events.push(s);
        forecast_with(&events, "2025-08-01", rules).flows.iter().filter(|f| f.category == "utilities" && f.date <= d("2025-08-31")).count()
    };
    let window: Rules = serde_json::from_str(r#"{"SCHEDULED_REPLACE_SCOPE":"category_window"}"#).unwrap();
    assert_eq!(run(4830.0, false, &window), 1);
    assert_eq!(Rules::default().scheduled_replace_scope, rules.scheduled_replace_scope);
    assert_eq!(run(4830.0, false, &rules), 2);
    assert_eq!(run(7300.0, false, &rules), 1);
    assert_eq!(run(4830.0, true, &rules), 1);
}

// ---- image accuracy plan work item 2: normalized currency, unverified reserve, witnesses ----

use super::ledger::{AmountSource, EvidenceRecord, EvidenceSource, Fact};

fn blank(id: u32, ty: EventType, dir: Direction, cat: &str, date: &str, settles: &str, status: Status) -> Event {
    let mut e = ev(id, ty, "image-backed row", cat, dir, 0.0, date, status);
    e.amount = None;
    e.settlement_date = Some(d(settles));
    e
}

fn image_rec(record_id: &str, fact: Fact) -> EvidenceRecord {
    EvidenceRecord { record_id: record_id.into(), source: EvidenceSource::Image, observed_at: d("2024-01-01").and_hms_opt(0, 0, 0).unwrap(), fact }
}

fn accepted(amount: f64, currency: &str) -> Fact {
    Fact::EventAmount { event_id: "event_1".into(), amount: Money::from_f64(amount), currency: currency.into() }
}

fn unverified(amount: f64, currency: &str) -> Fact {
    Fact::UnverifiedEventAmount { event_id: "event_1".into(), amount: Money::from_f64(amount), currency: currency.into(), reason: "no_witness".into() }
}

fn build_with(events: &[Event], evidence: &[EvidenceRecord], rd: &str, rates: &RateTable) -> (Ledger, Forecast) {
    let rules = Rules::default();
    let ledger = Ledger::build("INR", events, evidence, rates, &rules);
    let streams = recurrence::detect(&ledger, d(rd), &rules);
    let inputs = ForecastInputs {
        ledger: &ledger,
        streams: &streams,
        opening_balance: Money::from_units(100_000),
        minimum_balance: Money::ZERO,
        start: d(rd),
        rates,
        rules: &rules,
    };
    let f = Forecast::build(&inputs, &[]);
    (ledger, f)
}

fn event_flows(f: &Forecast) -> Vec<(NaiveDate, Money)> {
    f.flows
        .iter()
        .filter(|x| matches!(&x.source, FlowSource::Reserved { event_id } | FlowSource::Scheduled { event_id } if event_id == "event_1"))
        .map(|x| (x.date, x.amount))
        .collect()
}

/// A blank row with no accepted figure must stay unproven: Missing, no amount, no home amount
/// (evaluation::mirror BA3/BA4 invariants).
fn assert_still_missing(ledger: &Ledger) {
    let e = ledger.get("event_1").unwrap();
    assert_eq!(e.amount_source, AmountSource::Missing);
    assert_eq!((e.amount, e.home_amount), (None, None));
}

#[test]
fn currency_symbol_normalized_before_apply() {
    let rates = RateTable::default();
    let events = [blank(1, EventType::Expense, Direction::Debit, "telecom", "2025-07-30", "2025-08-09", Status::Pending)];
    for cur in ["Rs", "Rs.", "\u{20B9}", "INR", "inr", " Rupees "] {
        let (ledger, f) = build_with(&events, &[image_rec("image_05#agree", accepted(822.05, cur))], "2025-08-01", &rates);
        assert!(ledger.rejected.is_empty(), "{cur}: {:?}", ledger.rejected);
        assert_eq!(ledger.get("event_1").unwrap().amount_source, AmountSource::Evidence("image_05#agree".into()), "{cur}");
        assert_eq!(f.trough().0, Money::from_units(100_000) - Money::from_f64(822.05), "{cur}");
    }
    // A dollar figure never lands on a rupee row.
    let (ledger, f) = build_with(&events, &[image_rec("image_05#agree", accepted(822.05, "$"))], "2025-08-01", &rates);
    assert_eq!(ledger.rejected.len(), 1);
    assert!(ledger.rejected[0].reason.contains("USD"), "{}", ledger.rejected[0].reason);
    assert_still_missing(&ledger);
    assert!(event_flows(&f).is_empty());
}

#[test]
fn usd_symbol_applies_on_usd_row_with_dated_rate() {
    let mut e = blank(1, EventType::Expense, Direction::Debit, "transport", "2024-09-15", "2024-09-20", Status::Scheduled);
    e.currency = "USD".into();
    let (ledger, f) = build_with(&[e], &[image_rec("image_12#agree", accepted(33.50, "$"))], "2024-09-15", &rates());
    assert!(ledger.rejected.is_empty(), "{:?}", ledger.rejected);
    // 33.50 * 83.33 = 2,791.555 (latest USD->INR row on or before 2024-09-20)
    assert_eq!(event_flows(&f), vec![(d("2024-09-20"), -Money(27_915_550))]);
}

#[test]
fn failed_closed_pending_debit_is_reserved() {
    use super::session::Session;
    let events = vec![blank(1, EventType::Expense, Direction::Debit, "telecom", "2026-01-30", "2026-02-09", Status::Pending)];
    let profile = Profile {
        home_currency: "INR".into(),
        current_available_balance: Money::from_units(10_000),
        minimum_balance_to_keep: Money::from_units(1_000),
        financial_priorities: vec![],
        protected_categories: vec![],
        reducible_categories: vec![],
        stoppable_categories: vec![],
        accepted_methods: vec![PaymentMethod::FullPayment],
        max_installment_months: None,
    };
    let spec = RequestSpec { amount: Money::from_units(20_000), deadline: d("2026-03-01"), request_type: "purchase".into(), allows_partial_payment: false };
    let mut session = Session::open("user_x", profile, events, Arc::new(RateTable::default()), Rules::default());
    let without = session.decide("request_x", d("2026-02-01"), &spec, &[]).unwrap();
    session.apply_evidence(vec![image_rec("image_05#unverified", unverified(822.05, "\u{20B9}"))]);
    let with = session.decide("request_x", d("2026-02-01"), &spec, &[]).unwrap();

    assert_still_missing(session.ledger());
    assert_eq!(with.facts.missing_amounts, vec!["event_1".to_string()]);
    assert_eq!(with.facts.reserved_event_ids, vec!["event_1".to_string()]);
    let r = &with.facts.unverified_reserve;
    assert_eq!(r.len(), 1);
    assert_eq!((r[0].event_id.as_str(), r[0].record_id.as_str(), r[0].amount, r[0].home_amount), ("event_1", "image_05#unverified", Money::from_f64(822.05), Money::from_f64(822.05)));
    assert_eq!(r[0].currency, "INR");
    assert!(without.facts.unverified_reserve.is_empty());
    // The reserve is a real debit in the projection and trough drivers, and costs safe amount.
    assert_eq!(with.facts.trough_balance, without.facts.trough_balance - Money::from_f64(822.05));
    assert!(with.facts.trough_drivers.iter().any(|t| t.kind == "reserved" && t.component == "event_1" && t.total == -Money::from_f64(822.05)));
    assert!(with.facts.safe_amount < without.facts.safe_amount, "{} vs {}", with.facts.safe_amount, without.facts.safe_amount);
    assert!(with.facts.applied_evidence.is_empty());
}

#[test]
fn failed_closed_scheduled_debit_reserved_on_cash_date() {
    let events = [blank(1, EventType::Expense, Direction::Debit, "rent", "2025-08-01", "2025-08-20", Status::Scheduled)];
    let (ledger, f) = build_with(&events, &[image_rec("image_02#unverified", unverified(100_000.0, "INR"))], "2025-08-05", &RateTable::default());
    assert_still_missing(&ledger);
    assert_eq!(event_flows(&f), vec![(d("2025-08-20"), -Money::from_units(100_000))]);
    assert_eq!(f.low[(d("2025-08-19") - d("2025-08-05")).num_days() as usize], Money::from_units(100_000));
    assert_eq!(f.low[(d("2025-08-20") - d("2025-08-05")).num_days() as usize], Money::ZERO);
}

#[test]
fn unverified_largest_read_wins_in_any_order() {
    let events = [blank(1, EventType::Expense, Direction::Debit, "rent", "2025-08-01", "2025-08-20", Status::Scheduled)];
    let a = image_rec("image_02#unverified:a", unverified(90_000.0, "INR"));
    let b = image_rec("image_02#unverified:b", unverified(100_000.0, "INR"));
    let rates = RateTable::default();
    let (l1, f1) = build_with(&events, &[a.clone(), b.clone()], "2025-08-05", &rates);
    let (l2, f2) = build_with(&events, &[b.clone(), a.clone()], "2025-08-05", &rates);
    assert_eq!(l1, l2);
    assert_eq!(f1, f2);
    let held = &l1.unverified["event_1"];
    assert_eq!((held.record_id.as_str(), held.amount), ("image_02#unverified:b", Money::from_units(100_000)));
    assert_eq!(l1.rejected.len(), 1);
    assert_eq!(l1.rejected[0].record_id, "image_02#unverified:a");
    // Tie: the earlier record id stays.
    let c = image_rec("image_02#unverified:c", unverified(100_000.0, "INR"));
    let (l3, _) = build_with(&events, &[c, b], "2025-08-05", &rates);
    assert_eq!(l3.unverified["event_1"].record_id, "image_02#unverified:b");
}

#[test]
fn verified_figure_overrides_unverified() {
    let events = [blank(1, EventType::Expense, Direction::Debit, "rent", "2025-08-01", "2025-08-20", Status::Scheduled)];
    let evidence = [image_rec("image_02#unverified", unverified(120_000.0, "INR")), image_rec("image_02#agree", accepted(100_000.0, "INR"))];
    let (ledger, f) = build_with(&events, &evidence, "2025-08-05", &RateTable::default());
    assert_eq!(ledger.get("event_1").unwrap().amount, Some(Money::from_units(100_000)));
    assert!(ledger.unverified.is_empty());
    assert_eq!(ledger.rejected.len(), 1);
    assert_eq!(ledger.rejected[0].record_id, "image_02#unverified");
    assert_eq!(event_flows(&f), vec![(d("2025-08-20"), -Money::from_units(100_000))]);
}

#[test]
fn unverified_never_reserves_credit_settled_or_cancelled() {
    let rates = RateTable::default();
    let rd = "2025-08-05";
    let cases = [
        blank(1, EventType::Income, Direction::Credit, "salary", "2025-08-01", "2025-08-20", Status::Scheduled),
        blank(1, EventType::Refund, Direction::Credit, "refund", "2025-08-01", "2025-08-20", Status::Pending),
        blank(1, EventType::Expense, Direction::Debit, "dining", "2025-07-10", "2025-07-10", Status::Settled),
    ];
    for e in cases {
        let (ledger, f) = build_with(&[e.clone()], &[image_rec("image_x#unverified", unverified(500.0, "INR"))], rd, &rates);
        assert!(ledger.unverified.is_empty(), "{:?}", e.status);
        assert_eq!(ledger.rejected.len(), 1, "{:?}", e.status);
        assert_still_missing(&ledger);
        assert!(event_flows(&f).is_empty());
    }
    let cancel = EvidenceRecord {
        record_id: "message_01#0".into(),
        source: EvidenceSource::Message { source_type: "merchant".into() },
        observed_at: d("2025-08-02").and_hms_opt(9, 0, 0).unwrap(),
        fact: Fact::EventCancelled { event_id: "event_1".into() },
    };
    let pending = blank(1, EventType::Expense, Direction::Debit, "telecom", "2025-08-01", "2025-08-09", Status::Pending);
    let (ledger, f) = build_with(&[pending], &[image_rec("image_x#unverified", unverified(500.0, "INR")), cancel], rd, &rates);
    assert!(ledger.unverified.is_empty());
    assert!(event_flows(&f).is_empty());
}

#[test]
fn unverified_foreign_row_converted_with_dated_rate_or_rejected() {
    let mut usd = blank(1, EventType::Expense, Direction::Debit, "transport", "2024-09-15", "2024-09-20", Status::Scheduled);
    usd.currency = "USD".into();
    let (ledger, f) = build_with(&[usd.clone()], &[image_rec("image_12#unverified", unverified(33.50, "$"))], "2024-09-15", &rates());
    assert_eq!(ledger.unverified["event_1"].home_amount, Money(27_915_550));
    assert_eq!(event_flows(&f), vec![(d("2024-09-20"), -Money(27_915_550))]);
    // No rate on the cash date: never guessed.
    let (ledger, f) = build_with(&[usd], &[image_rec("image_12#unverified", unverified(33.50, "$"))], "2024-09-15", &RateTable::default());
    assert!(ledger.unverified.is_empty());
    assert!(ledger.rejected[0].reason.contains("rate"), "{}", ledger.rejected[0].reason);
    assert!(event_flows(&f).is_empty());
}

#[test]
fn image_07_dual_totals_recorded() {
    let rates = RateTable::default();
    let events = [blank(1, EventType::Expense, Direction::Debit, "dining", "2025-07-10", "2025-07-10", Status::Settled)];
    let witness = |acc: f64| {
        image_rec(
            "image_07#witness",
            Fact::AmountWitness {
                event_id: "event_1".into(),
                accepted: Money::from_f64(acc),
                computed: Money::from_f64(8528.10),
                currency: "\u{20B9}".into(),
                witness: "8122 + 203.05 + 203.05".into(),
            },
        )
    };
    let paid = image_rec("image_07#agree", accepted(8528.0, "INR"));
    let (ledger, _) = build_with(&events, &[witness(8528.0), paid.clone()], "2025-08-01", &rates);
    assert_eq!(ledger.get("event_1").unwrap().amount, Some(Money::from_units(8528)));
    assert!(ledger.rejected.is_empty(), "{:?}", ledger.rejected);
    assert_eq!(ledger.witnesses.len(), 1);
    let w = &ledger.witnesses[0];
    assert_eq!((w.accepted, w.computed, w.currency.as_str()), (Money::from_units(8528), Money::from_f64(8528.10), "INR"));
    // A witness that disagrees with the applied figure, or has no applied figure, is rejected.
    let (ledger, _) = build_with(&events, &[witness(8528.10), paid], "2025-08-01", &rates);
    assert!(ledger.witnesses.is_empty());
    assert_eq!(ledger.get("event_1").unwrap().amount, Some(Money::from_units(8528)));
    let (ledger, _) = build_with(&events, &[witness(8528.0)], "2025-08-01", &rates);
    assert!(ledger.witnesses.is_empty());
    assert_still_missing(&ledger);
}
