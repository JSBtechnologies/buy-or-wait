//! Validator tests on the real dataset. Mutations use tuning rows only (request_01–18).

use std::path::{Path, PathBuf};

use super::contract::{check_row, OutputRow, Severity};
use super::data::Dataset;
use super::scorer;

fn dataset_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../dataset")
}

fn samples() -> Dataset {
    Dataset::load(&dataset_dir(), &dataset_dir().join("sample_requests.csv")).expect("load samples")
}

fn label(ds: &Dataset, id: &str) -> OutputRow {
    ds.labels.iter().find(|r| r.request_id == id).cloned().expect(id)
}

fn error_codes(ds: &Dataset, row: &OutputRow) -> Vec<&'static str> {
    check_row(ds, row).into_iter().filter(|f| f.severity == Severity::Error).map(|f| f.code).collect()
}

/// Assert the mutated row raises `code` as a hard error.
fn expect(ds: &Dataset, id: &str, code: &str, mutate: impl FnOnce(&mut OutputRow)) {
    let mut row = label(ds, id);
    mutate(&mut row);
    let codes = error_codes(ds, &row);
    assert!(codes.contains(&code), "{id}: expected {code}, got {codes:?} for {row:?}");
}

#[test]
fn every_sample_label_passes_the_contract() {
    let rep = super::selftest(&dataset_dir()).unwrap();
    assert!(rep.passed(), "{}", rep.render());
    assert_eq!(rep.rows, 25);
}

#[test]
fn labels_score_perfectly_against_themselves() {
    let ds = samples();
    let rep = scorer::score(&ds, &ds.labels);
    for split in [&rep.tuning, &rep.heldout] {
        assert!(split.mismatches.is_empty(), "{:?}", split.mismatches);
        assert_eq!(split.matched.get("all_scored_fields").copied(), Some(split.n));
    }
    assert_eq!(rep.tuning.n, 18);
    assert_eq!(rep.heldout.n, 7);
}

#[test]
fn heldout_detail_hidden_by_default() {
    let ds = samples();
    let mut rows = ds.labels.clone();
    for r in rows.iter_mut().filter(|r| scorer::is_heldout(&r.request_id)) {
        r.affordability_status = "zz_mutated".into();
    }
    let text = scorer::score(&ds, &rows).render(false);
    assert!(!text.contains("expected=") && !text.contains("zz_mutated"), "{text}");
    assert_eq!(scorer::score(&ds, &rows).heldout.matched.get("affordability_status"), None);
    assert!(scorer::score(&ds, &rows).render(true).contains("request_19 affordability_status"));
}

#[test]
fn horizon_end_covers_month_end_rule() {
    use super::contract::horizon_end;
    let d = |s: &str| super::data::parse_date(s).unwrap();
    assert_eq!(horizon_end(d("2025-02-07")), d("2025-05-08")); // rd+90 later than 2025-04-30
    assert_eq!(horizon_end(d("2026-03-01")), d("2026-05-31")); // month end later (91 days)
    assert_eq!(horizon_end(d("2025-11-06")), d("2026-02-04")); // year wrap
    assert_eq!(horizon_end(d("2024-12-01")), d("2025-03-01"));
}

#[test]
fn scalar_rules() {
    let ds = samples();
    expect(&ds, "request_01", "B1_amount_range", |r| r.amount_safe_to_pay = "25256.01".into());
    expect(&ds, "request_05", "B1_amount_range", |r| r.amount_safe_to_pay = "-1".into());
    expect(&ds, "request_05", "B1_amount_format", |r| r.amount_safe_to_pay = "737.001".into());
    expect(&ds, "request_05", "B1_amount_format", |r| r.amount_safe_to_pay = "1,000".into());
    expect(&ds, "request_01", "B2_status_enum", |r| r.affordability_status = "affordable".into());
    expect(&ds, "request_01", "B3_method_enum", |r| r.recommended_payment_method = "pay_now".into());
    expect(&ds, "request_03", "B4_earliest_window", |r| {
        r.earliest_date_for_full_payment = "2019-12-03".into();
        r.payment_plan = "2019-12-03:5491000".into();
    });
    expect(&ds, "request_03", "B4_earliest_format", |r| r.earliest_date_for_full_payment = "15/11/2019".into());
    expect(&ds, "request_12", "B5_amount_vs_earliest", |r| r.earliest_date_for_full_payment = "2026-04-06".into());
    expect(&ds, "request_01", "B6_explanation_empty", |r| r.decision_explanation = " ".into());
    expect(&ds, "request_01", "A2_unknown_request", |r| r.request_id = "request_99".into());
}

#[test]
fn status_method_and_plan_rules() {
    let ds = samples();
    expect(&ds, "request_01", "C_status_method", |r| r.affordability_status = "affordable_later".into());
    expect(&ds, "request_05", "C_status_method", |r| r.recommended_payment_method = "wait".into());
    expect(&ds, "request_01", "D2_full_plan", |r| r.payment_plan = "2024-03-04:25256".into());
    expect(&ds, "request_01", "D1_plan_format", |r| r.payment_plan = "2024-03-03;25256".into());
    expect(&ds, "request_02", "D1_plan_order", |r| {
        r.payment_plan = "2025-09-07:15952906.67|2025-08-08:15952906.67|2025-10-07:15952906.67".into()
    });
    expect(&ds, "request_05", "D8_notrec_plan", |r| r.payment_plan = "2025-11-06:15488".into());
    expect(&ds, "request_05", "D1_plan_none", |r| {
        r.affordability_status = "affordable_now".into();
        r.recommended_payment_method = "full_payment".into();
    });
    // full_payment for a user who only considers partial/installments
    expect(&ds, "request_12", "D_method_not_accepted", |r| {
        r.recommended_payment_method = "full_payment".into();
        r.payment_plan = "2026-04-05:65164".into();
        r.spending_changes_needed = "stop:event_1".into();
    });
    expect(&ds, "request_06", "D4_full_with_plan_no_changes", |r| r.spending_changes_needed = "none".into());
    expect(&ds, "request_16", "D3_now_changes", |r| r.spending_changes_needed = "stop:event_1".into());
}

#[test]
fn partial_payment_rules() {
    let ds = samples();
    // request_03 does not allow partial payment; request_10 allows it and the user accepts it.
    let partial = |r: &mut OutputRow, amt: &str, plan: &str, earliest: &str| {
        r.affordability_status = "affordable_with_plan".into();
        r.recommended_payment_method = "partial_payment".into();
        r.amount_safe_to_pay = amt.into();
        r.payment_plan = plan.into();
        r.earliest_date_for_full_payment = earliest.into();
    };
    expect(&ds, "request_03", "D5_partial_not_allowed", |r| {
        partial(r, "873000", "2019-09-03:873000|2019-11-15:4618000", "2019-11-15")
    });
    // request_10: requested 266700, desired 2025-02-10, allows partial, user accepts partial.
    let ok = {
        let mut r = label(&ds, "request_10");
        partial(&mut r, "12700", "2024-12-06:12700|2025-01-15:254000", "2025-01-15");
        r
    };
    assert!(error_codes(&ds, &ok).is_empty(), "{:?}", error_codes(&ds, &ok));
    expect(&ds, "request_10", "D5_partial_sum", |r| {
        partial(r, "12700", "2024-12-06:12700|2025-01-15:254001", "2025-01-15")
    });
    expect(&ds, "request_10", "D5_partial_plan", |r| {
        partial(r, "12700", "2024-12-06:12000|2025-01-15:254700", "2025-01-15")
    });
    expect(&ds, "request_10", "D5_partial_plan", |r| {
        partial(r, "12700", "2024-12-06:12700|2025-01-16:254000", "2025-01-15")
    });
    expect(&ds, "request_10", "D5_partial_deadline", |r| {
        partial(r, "12700", "2024-12-06:12700|2025-02-11:254000", "2025-02-11")
    });
    expect(&ds, "request_10", "D5_partial_amount", |r| {
        partial(r, "0", "2024-12-06:0|2025-01-15:266700", "2025-01-15")
    });
    expect(&ds, "request_10", "D5_partial_plan", |r| {
        partial(r, "12700", "2024-12-06:12700|2025-01-01:100000|2025-01-15:154000", "2025-01-15")
    });
}

#[test]
fn installment_rules() {
    let ds = samples();
    expect(&ds, "request_02", "D6_installment_no_option", |r| {
        r.payment_plan = "2025-08-08:15952906.67|2025-09-08:15952906.67|2025-10-07:15952906.67".into()
    });
    expect(&ds, "request_02", "D6_installment_no_option", |r| {
        r.payment_plan = "2025-08-08:15952906.67|2025-09-07:15952906.67".into()
    });
    // option_07 exists (18 payments) but exceeds max_installment_months 7.
    let o7 = ds.options["request_02"].iter().find(|o| o.payment_option_id == "payment_option_07").unwrap();
    let plan = o7
        .schedule()
        .iter()
        .map(|(d, a)| format!("{d}:{}", super::data::fmt_cents_2dp(*a)))
        .collect::<Vec<_>>()
        .join("|");
    expect(&ds, "request_02", "D6_installment_max", |r| r.payment_plan = plan);
    // user_01 does not consider installments (blank max); option_03 is a real schedule.
    let o3 = ds.options["request_01"].iter().find(|o| o.payment_option_id == "payment_option_03").unwrap();
    let plan = o3
        .schedule()
        .iter()
        .map(|(d, a)| format!("{d}:{}", super::data::fmt_cents_2dp(*a)))
        .collect::<Vec<_>>()
        .join("|");
    expect(&ds, "request_01", "D_method_not_accepted", |r| {
        r.affordability_status = "affordable_with_plan".into();
        r.recommended_payment_method = "installments".into();
        r.payment_plan = plan;
    });
}

#[test]
fn wait_rules() {
    let ds = samples();
    expect(&ds, "request_03", "D7_wait_plan", |r| r.payment_plan = "2019-11-14:5491000".into());
    expect(&ds, "request_03", "D7_wait_plan", |r| r.payment_plan = "2019-11-15:873000".into());
    expect(&ds, "request_03", "D7_wait_earliest", |r| r.earliest_date_for_full_payment = "".into());
    // user_07 only considers installments, so wait is not eligible.
    expect(&ds, "request_07", "D_method_not_accepted", |r| {
        r.affordability_status = "affordable_later".into();
        r.recommended_payment_method = "wait".into();
        r.payment_plan = "2024-10-23:197400".into();
    });
}

#[test]
fn spending_change_rules() {
    let ds = samples();
    expect(&ds, "request_06", "E1_changes_format", |r| r.spending_changes_needed = "cancel:event_476".into());
    expect(&ds, "request_06", "E1_changes_format", |r| r.spending_changes_needed = "reduce_to:event_476".into());
    expect(&ds, "request_06", "E2_changes_unknown_event", |r| r.spending_changes_needed = "stop:event_999999".into());
    expect(&ds, "request_06", "E2_changes_other_user", |r| r.spending_changes_needed = "stop:event_01".into());
    expect(&ds, "request_06", "E6_changes_same_event", |r| {
        r.spending_changes_needed = "stop:event_476|reduce_to:event_476:10".into()
    });
    // event_476 is stoppable, not reducible.
    expect(&ds, "request_06", "E5_reduce_flexibility", |r| r.spending_changes_needed = "reduce_to:event_476:10".into());
    expect(&ds, "request_11", "E5_reduce_below_minimum", |r| {
        r.spending_changes_needed = "reduce_to:event_989:665949.99".into()
    });
    expect(&ds, "request_11", "E5_reduce_not_lower", |r| {
        r.spending_changes_needed = "reduce_to:event_989:1163530.49".into()
    });
    // dining is reducible for user_11 but not stoppable, and event_989 is reducible only.
    expect(&ds, "request_11", "E4_stop_category", |r| r.spending_changes_needed = "stop:event_989".into());
    expect(&ds, "request_11", "E4_stop_flexibility", |r| r.spending_changes_needed = "stop:event_989".into());
    expect(&ds, "request_06", "E1_changes_count", |r| {
        r.spending_changes_needed = "stop:event_476|stop:event_476|stop:event_476|stop:event_476".into()
    });
    expect(&ds, "request_03", "E7_changes_status", |r| r.spending_changes_needed = "stop:event_476".into());

    // A fixed or protected event must never be touched: find one for user_06.
    let fixed = ds.events.values().find(|e| e.user_id == "user_06" && e.flexibility == "fixed" && e.direction == "debit").unwrap();
    let id = fixed.event_id.clone();
    expect(&ds, "request_06", "E4_stop_flexibility", |r| r.spending_changes_needed = format!("stop:{id}"));
    let protected = ds.events.values().find(|e| e.user_id == "user_06" && ["rent", "insurance", "transport"].contains(&e.category.as_str())).unwrap();
    let id = protected.event_id.clone();
    expect(&ds, "request_06", "E3_changes_protected", |r| r.spending_changes_needed = format!("stop:{id}"));
}

#[test]
fn file_level_rules() {
    let ds = samples();
    let good: Vec<String> = super::contract::HEADER.iter().map(|s| s.to_string()).collect();
    let mut rows = ds.labels.clone();
    assert!(super::contract::validate_rows(&ds, &good, &rows).passed());

    let mut swapped = good.clone();
    swapped.swap(4, 5);
    let rep = super::contract::validate_rows(&ds, &swapped, &rows);
    assert!(rep.errors().any(|f| f.code == "A1_header"));

    rows.push(rows[0].clone());
    let rep = super::contract::validate_rows(&ds, &good, &rows);
    assert!(rep.errors().any(|f| f.code == "A2_duplicate"));

    rows.truncate(24);
    let rep = super::contract::validate_rows(&ds, &good, &rows);
    assert!(rep.errors().any(|f| f.code == "A2_missing"));
}

/// Set VERIFY_OUTPUT=<engine output for sample_requests.csv> to score it under `cargo test`.
#[test]
fn score_engine_output_if_given() {
    let Ok(path) = std::env::var("VERIFY_OUTPUT") else { return };
    let (code, text) = super::score_file(&dataset_dir(), Path::new(&path), false).unwrap();
    println!("{text}");
    assert_eq!(code, 0, "contract failures");
}
