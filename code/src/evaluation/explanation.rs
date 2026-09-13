//! File-level explanation checks (no engine needed):
//! - EX1 every number in an explanation equals a row field, a request field, the profile's
//!   minimum balance, a date component of a row/request date, or the template constant 90
//! - EX2 the wording matches the recommended method (a wait row does not say "today", …)
//! - EX3 near-duplicate explanations across rows: identical text, or identical after masking
//!   numbers for requests of different users with different amounts is fine (templates), but an
//!   identical full text for two requests with different amounts or currencies is an error.

use std::collections::HashMap;

use super::contract::{parse_changes, parse_plan, Change, Finding, OutputRow, Severity};
use super::data::{fmt_cents_short, parse_cents, Dataset};

/// Digit groups in text, normalised like amounts (`25,256` → `25256`, `620.40` → `620.4`).
pub fn numbers(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut cur = String::new();
    for (i, c) in chars.iter().enumerate() {
        let next_digit = chars.get(i + 1).map(|n| n.is_ascii_digit()).unwrap_or(false);
        if c.is_ascii_digit() {
            cur.push(*c);
        } else if *c == ',' && !cur.is_empty() && next_digit {
        } else if *c == '.' && !cur.is_empty() && next_digit {
            cur.push('.');
        } else if !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out.into_iter().map(|n| parse_cents(&n).map(fmt_cents_short).unwrap_or(n)).collect()
}

pub fn unexplained_numbers(text: &str, allowed: &[String]) -> Vec<String> {
    numbers(text).into_iter().filter(|n| !allowed.contains(n)).collect()
}

fn allowed_for(ds: &Dataset, row: &OutputRow) -> Vec<String> {
    use chrono::Datelike;
    let mut a = Vec::new();
    let Some(req) = ds.request(&row.request_id) else { return a };
    let push_cents = |a: &mut Vec<String>, c: i64| a.push(fmt_cents_short(c));
    push_cents(&mut a, req.requested);
    if let Some(p) = ds.profiles.get(&req.user_id) {
        push_cents(&mut a, p.minimum);
    }
    if let Ok(c) = parse_cents(&row.amount_safe_to_pay) {
        push_cents(&mut a, c);
    }
    let plan = parse_plan(&row.payment_plan).unwrap_or_default();
    a.push(plan.len().to_string());
    let mut dates = vec![req.request_date, req.desired_completion_date];
    for p in &plan {
        push_cents(&mut a, p.amount);
        dates.push(p.date);
    }
    if let Some((first, last)) = plan.first().zip(plan.last()) {
        push_cents(&mut a, req.requested - first.amount); // partial remainder
        let _ = last;
    }
    if let Ok(d) = super::data::parse_date(&row.earliest_date_for_full_payment) {
        dates.push(d);
    }
    for ch in parse_changes(&row.spending_changes_needed).unwrap_or_default() {
        if let Change::ReduceTo(_, c) = ch {
            push_cents(&mut a, c);
        }
    }
    for d in dates {
        a.push(d.day().to_string());
        a.push(d.year().to_string());
    }
    a.push("90".into());
    a
}

fn wording_ok(method: &str, text: &str) -> bool {
    let t = text.to_lowercase();
    match method {
        "full_payment" => t.contains("today") && !t.starts_with("do not") && !t.contains("installment"),
        "partial_payment" => t.contains("today") && t.contains("remaining"),
        "installments" => t.contains("installment"),
        "wait" => !t.contains(" today") && (t.contains("wait until") || t.contains("in full on")),
        "not_recommended" => t.starts_with("do not"),
        _ => true,
    }
}

pub fn check(ds: &Dataset, rows: &[OutputRow]) -> Vec<Finding> {
    let mut out = Vec::new();
    for r in rows {
        let allowed = allowed_for(ds, r);
        for n in unexplained_numbers(&r.decision_explanation, &allowed) {
            out.push(Finding { request_id: r.request_id.clone(), severity: Severity::Error, code: "EX1_number_not_in_row", detail: format!("explanation number {n} matches no row/request/profile value") });
        }
        if !wording_ok(&r.recommended_payment_method, &r.decision_explanation) {
            out.push(Finding { request_id: r.request_id.clone(), severity: Severity::Error, code: "EX2_wording_vs_method", detail: format!("{} explanation: {:?}", r.recommended_payment_method, r.decision_explanation) });
        }
    }
    let mut by_text: HashMap<&str, Vec<&OutputRow>> = HashMap::new();
    for r in rows {
        by_text.entry(r.decision_explanation.as_str()).or_default().push(r);
    }
    for (text, group) in by_text.into_iter().filter(|(_, g)| g.len() > 1) {
        let ids: Vec<&str> = group.iter().map(|r| r.request_id.as_str()).collect();
        let requests: Vec<_> = ids.iter().filter_map(|id| ds.request(id)).collect();
        let same_facts = requests.windows(2).all(|w| {
            let cur = |q: &super::data::Request| ds.profiles.get(&q.user_id).map(|p| p.home_currency.clone());
            w[0].requested == w[1].requested && cur(w[0]) == cur(w[1])
        });
        let sev = if same_facts { Severity::Warn } else { Severity::Error };
        out.push(Finding { request_id: ids.join(","), severity: sev, code: "EX3_duplicate_explanation", detail: format!("{} rows share {:?}", ids.len(), text) });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn samples() -> Dataset {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../dataset");
        Dataset::load(&dir, &dir.join("sample_requests.csv")).unwrap()
    }

    #[test]
    fn every_sample_label_explanation_passes() {
        let ds = samples();
        let f = check(&ds, &ds.labels);
        assert!(f.is_empty(), "{f:#?}");
    }

    #[test]
    fn catches_foreign_numbers_wrong_wording_and_copied_text() {
        let ds = samples();
        let mut rows = ds.labels.clone();
        let i1 = rows.iter().position(|r| r.request_id == "request_01").unwrap();
        rows[i1].decision_explanation = "Pay ZAR 25,256 today. This leaves at least ZAR 19,500 available over the next 90 days.".into();
        let i3 = rows.iter().position(|r| r.request_id == "request_03").unwrap();
        rows[i3].decision_explanation = "Pay IDR 5,491,000 today.".into();
        let i9 = rows.iter().position(|r| r.request_id == "request_09").unwrap();
        rows[i9].decision_explanation = rows[i1].decision_explanation.clone();
        let codes: Vec<(String, &str)> = check(&ds, &rows).into_iter().map(|f| (f.request_id, f.code)).collect();
        assert!(codes.contains(&("request_01".into(), "EX1_number_not_in_row")), "{codes:?}");
        assert!(codes.contains(&("request_03".into(), "EX2_wording_vs_method")), "{codes:?}");
        assert!(codes.iter().any(|(id, c)| *c == "EX3_duplicate_explanation" && id.contains("request_01") && id.contains("request_09")), "{codes:?}");
    }
}
