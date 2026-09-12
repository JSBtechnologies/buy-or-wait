//! Field-by-field scoring of engine output against `sample_requests.csv`.
//!
//! request_01–18 are the tuning split (full diffs). request_19–25 are held out: the default
//! render shows only pass counts, so labels and diffs never reach the tuning loop.

use std::collections::{BTreeMap, HashMap};

use chrono::NaiveDate;

use super::contract::{parse_changes, parse_plan, OutputRow};
use super::data::{cents_to_f64, parse_cents, Dataset};

pub const HELDOUT_FROM: u32 = 19;

pub const FIELDS: [&str; 11] = [
    "amount_safe_to_pay",
    "affordability_status",
    "recommended_payment_method",
    "payment_plan",
    "payment_plan(strict_string)",
    "earliest_date_for_full_payment",
    "spending_changes_needed",
    "spending_changes_needed(unordered)",
    "decision_explanation(facts)",
    "all_scored_fields",
    "amount_safe_to_pay(within_1%)",
];

pub fn is_heldout(request_id: &str) -> bool {
    request_id
        .rsplit('_')
        .next()
        .and_then(|n| n.parse::<u32>().ok())
        .map(|n| n >= HELDOUT_FROM)
        .unwrap_or(false)
}

#[derive(Clone, Debug)]
pub struct Mismatch {
    pub request_id: String,
    pub field: &'static str,
    pub expected: String,
    pub got: String,
    pub note: String,
}

#[derive(Clone, Debug, Default)]
pub struct SplitScore {
    pub name: &'static str,
    pub n: usize,
    pub missing: Vec<String>,
    pub matched: BTreeMap<&'static str, usize>,
    pub amount_abs_err: Vec<f64>,
    pub amount_rel_err: Vec<f64>,
    pub mismatches: Vec<Mismatch>,
}

#[derive(Clone, Debug, Default)]
pub struct ScoreReport {
    pub tuning: SplitScore,
    pub heldout: SplitScore,
}

/// Digit groups in an explanation ("ZAR 25,256" → "25256", "15 November 2019" → "15", "2019").
fn numbers(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let chars: Vec<char> = text.chars().collect();
    for (i, c) in chars.iter().enumerate() {
        let joins = (*c == ',' || *c == '.')
            && !cur.is_empty()
            && chars.get(i + 1).map(|n| n.is_ascii_digit()).unwrap_or(false);
        if c.is_ascii_digit() || (joins && *c == '.') {
            cur.push(*c);
        } else if joins {
            // thousands separator: skip
        } else if !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out.into_iter()
        .map(|n| match parse_cents(&n) {
            Ok(c) => super::data::fmt_cents_short(c),
            Err(_) => n,
        })
        .collect()
}

fn compare(label: &OutputRow, got: &OutputRow, split: &mut SplitScore) {
    let id = label.request_id.clone();
    let mut all = true;
    let mut hit = |split: &mut SplitScore, field: &'static str, ok: bool, exp: &str, g: &str, note: String, counts: bool| {
        if ok {
            *split.matched.entry(field).or_default() += 1;
        } else {
            if counts {
                all = false;
            }
            split.mismatches.push(Mismatch {
                request_id: id.clone(),
                field,
                expected: exp.to_string(),
                got: g.to_string(),
                note,
            });
        }
    };

    // amount
    let (le, ge) = (parse_cents(&label.amount_safe_to_pay), parse_cents(&got.amount_safe_to_pay));
    match (le, ge) {
        (Ok(l), Ok(g)) => {
            let abs = cents_to_f64((l - g).abs());
            let rel = if l == 0 { if g == 0 { 0.0 } else { f64::INFINITY } } else { abs / cents_to_f64(l.abs()) };
            split.amount_abs_err.push(abs);
            split.amount_rel_err.push(rel);
            hit(split, FIELDS[0], l == g, &label.amount_safe_to_pay, &got.amount_safe_to_pay,
                format!("abs {abs:.2}, rel {:.4}%", rel * 100.0), true);
            if rel <= 0.01 {
                *split.matched.entry(FIELDS[10]).or_default() += 1;
            }
        }
        _ => {
            split.amount_abs_err.push(f64::INFINITY);
            split.amount_rel_err.push(f64::INFINITY);
            hit(split, FIELDS[0], false, &label.amount_safe_to_pay, &got.amount_safe_to_pay, "unparseable".into(), true);
        }
    }

    for (field, l, g) in [
        (FIELDS[1], &label.affordability_status, &got.affordability_status),
        (FIELDS[2], &label.recommended_payment_method, &got.recommended_payment_method),
    ] {
        hit(split, field, l == g, l, g, String::new(), true);
    }

    let plan_eq = match (parse_plan(&label.payment_plan), parse_plan(&got.payment_plan)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    };
    hit(split, FIELDS[3], plan_eq, &label.payment_plan, &got.payment_plan, String::new(), true);
    let strict = label.payment_plan == got.payment_plan;
    hit(split, FIELDS[4], strict, &label.payment_plan, &got.payment_plan,
        if plan_eq { "formatting only".into() } else { String::new() }, false);

    let day_delta = match (
        NaiveDate::parse_from_str(&label.earliest_date_for_full_payment, "%Y-%m-%d"),
        NaiveDate::parse_from_str(&got.earliest_date_for_full_payment, "%Y-%m-%d"),
    ) {
        (Ok(a), Ok(b)) => format!("{:+} days", (b - a).num_days()),
        _ => String::new(),
    };
    hit(split, FIELDS[5], label.earliest_date_for_full_payment == got.earliest_date_for_full_payment,
        &label.earliest_date_for_full_payment, &got.earliest_date_for_full_payment, day_delta, true);

    let (lc, gc) = (parse_changes(&label.spending_changes_needed), parse_changes(&got.spending_changes_needed));
    let ordered = matches!((&lc, &gc), (Ok(a), Ok(b)) if a == b);
    hit(split, FIELDS[6], ordered, &label.spending_changes_needed, &got.spending_changes_needed, String::new(), true);
    let unordered = match (lc, gc) {
        (Ok(mut a), Ok(mut b)) => {
            let key = |c: &super::contract::Change| format!("{c:?}");
            a.sort_by_key(key);
            b.sort_by_key(key);
            a == b
        }
        _ => false,
    };
    hit(split, FIELDS[7], unordered, &label.spending_changes_needed, &got.spending_changes_needed, String::new(), false);

    let ours = numbers(&got.decision_explanation);
    let missing: Vec<String> = numbers(&label.decision_explanation).into_iter().filter(|n| !ours.contains(n)).collect();
    hit(split, FIELDS[8], missing.is_empty(), &label.decision_explanation, &got.decision_explanation,
        format!("label numbers missing from ours: {}", missing.join(" ")), false);

    if all {
        *split.matched.entry(FIELDS[9]).or_default() += 1;
    }
}

/// `ds` must be loaded from `sample_requests.csv` (it carries the labels).
pub fn score(ds: &Dataset, outputs: &[OutputRow]) -> ScoreReport {
    let by_id: HashMap<&str, &OutputRow> = outputs.iter().map(|r| (r.request_id.as_str(), r)).collect();
    let mut rep = ScoreReport {
        tuning: SplitScore { name: "tuning request_01-18", ..Default::default() },
        heldout: SplitScore { name: "held-out request_19-25", ..Default::default() },
    };
    for label in &ds.labels {
        let split = if is_heldout(&label.request_id) { &mut rep.heldout } else { &mut rep.tuning };
        split.n += 1;
        match by_id.get(label.request_id.as_str()) {
            Some(got) => compare(label, got, split),
            None => {
                split.missing.push(label.request_id.clone());
                split.amount_abs_err.push(f64::INFINITY);
                split.amount_rel_err.push(f64::INFINITY);
            }
        }
    }
    rep
}

impl SplitScore {
    fn render_counts(&self, out: &mut String) {
        out.push_str(&format!("== {} (n={})\n", self.name, self.n));
        for f in FIELDS {
            out.push_str(&format!("  {:<38} {:>2}/{}\n", f, self.matched.get(f).copied().unwrap_or(0), self.n));
        }
        let within = |t: f64| self.amount_rel_err.iter().filter(|e| **e <= t).count();
        out.push_str(&format!(
            "  amount within 0.1%/1%/5%:              {}/{}/{} of {}\n",
            within(0.001), within(0.01), within(0.05), self.n
        ));
        if !self.missing.is_empty() {
            out.push_str(&format!("  missing rows: {}\n", self.missing.len()));
        }
    }

    fn render_detail(&self, out: &mut String) {
        let finite: Vec<f64> = self.amount_abs_err.iter().copied().filter(|e| e.is_finite()).collect();
        if !finite.is_empty() {
            out.push_str(&format!(
                "  amount abs error: mean {:.2}, max {:.2}\n",
                finite.iter().sum::<f64>() / finite.len() as f64,
                finite.iter().cloned().fold(0.0, f64::max)
            ));
        }
        for id in &self.missing {
            out.push_str(&format!("  MISSING {id}\n"));
        }
        for m in &self.mismatches {
            out.push_str(&format!(
                "  {} {}: expected={:?} got={:?}{}\n",
                m.request_id,
                m.field,
                m.expected,
                m.got,
                if m.note.is_empty() { String::new() } else { format!(" ({})", m.note) }
            ));
        }
    }
}

impl ScoreReport {
    /// Tuning split in full; held-out split as counts unless `reveal_heldout` (verifier-only).
    pub fn render(&self, reveal_heldout: bool) -> String {
        let mut out = String::new();
        self.tuning.render_counts(&mut out);
        self.tuning.render_detail(&mut out);
        self.heldout.render_counts(&mut out);
        if reveal_heldout {
            self.heldout.render_detail(&mut out);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heldout_split() {
        assert!(!is_heldout("request_18"));
        assert!(is_heldout("request_19"));
        assert!(is_heldout("request_25"));
        assert!(is_heldout("request_26"));
    }

    #[test]
    fn explanation_numbers_normalised() {
        assert_eq!(numbers("Pay EUR 620.40 on 15 April 2025."), vec!["620.4", "15", "2025"]);
        assert_eq!(numbers("at least ZAR 18,000 available"), vec!["18000"]);
    }
}
