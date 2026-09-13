//! Engine-backed verification: re-run the batch path the way `main.rs::decide_one` does
//! (session → retrieval → deterministic evidence → decide) and check what no output file can
//! show on its own: every row's forecast invariants, blank-amount handling, grounded
//! explanations against DecisionFacts, and that the shipped rows are exactly the engine's.
//!
//! Keep `evidence_for` in step with main.rs. When images are wired into the batch path, add the
//! image records here too (the reproduction check will fail loudly until then).

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use chrono::NaiveDate;

use super::contract::{Finding, OutputRow, Severity};
use super::replay::ForecastSeries;
use super::Invariants;
use crate::engine::ledger::{AmountSource, EvidenceRecord};
use crate::engine::money::Money;
use crate::engine::session::{Decision, Session};
use crate::engine::types::{PaymentOption, RateTable, RequestSpec};
use crate::engine::Rules;
use crate::model;

pub struct Inputs {
    pub profiles: Vec<model::FinancialProfile>,
    pub events: Vec<model::FinancialEvent>,
    pub rates: Arc<RateTable>,
    pub options: Vec<model::RequestPaymentOption>,
    pub messages: Vec<model::Message>,
    pub images: Vec<model::Image>,
    /// `<dataset>/../code`: where the shipped run persisted `store/processed/evidence`.
    pub code_dir: std::path::PathBuf,
}

impl Inputs {
    pub fn load(dataset_dir: &Path) -> Result<Inputs> {
        let d = dataset_dir;
        Ok(Inputs {
            profiles: model::load_financial_profiles(d.join("financial_profiles.csv"))?,
            events: model::load_financial_events(d.join("financial_events.csv"))?,
            rates: Arc::new(RateTable::from_model(&model::load_exchange_rates(d.join("exchange_rates.csv"))?)),
            options: model::load_request_payment_options(d.join("request_payment_options.csv"))?,
            messages: model::load_messages(d.join("messages.csv"))?,
            images: model::load_images(d.join("images.csv"))?,
            code_dir: d.parent().map(|p| p.join("code")).unwrap_or_else(|| Path::new(".").to_path_buf()),
        })
    }
}

/// The batch pipeline's evidence for one request (mirror of main.rs decide_one).
pub fn evidence_for(inp: &Inputs, user: &str, request_date: NaiveDate) -> Vec<EvidenceRecord> {
    let home = inp.profiles.iter().find(|p| p.user_id == user).map(|p| p.home_currency.clone()).unwrap_or_default();
    let ev = crate::extract::retrieval::for_user(user, request_date, &inp.messages, &[]);
    crate::extract::messages::deterministic_evidence(&ev.messages, &home)
}

/// Evidence the shipped run applied, when it persisted it (store/processed/evidence/<request>.json,
/// includes model-path records the verifier cannot regenerate); otherwise the live parse.
/// evidence_consistency checks the snapshot against the live parse and grounds its model records.
pub fn applied_or_live(inp: &Inputs, r: &model::Request) -> Vec<EvidenceRecord> {
    let p = inp.code_dir.join("store/processed/evidence").join(format!("{}.json", r.request_id));
    if let Ok(text) = std::fs::read_to_string(&p) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
            if let Ok(recs) = serde_json::from_value::<Vec<EvidenceRecord>>(v.get("value").cloned().unwrap_or(v)) {
                return recs;
            }
        }
    }
    evidence_for(inp, &r.user_id, r.request_date)
}

pub fn decide(inp: &Inputs, r: &model::Request, rules: Rules) -> Result<(Decision, Session)> {
    let mut session = Session::from_model(&r.user_id, &inp.profiles, &inp.events, inp.rates.clone(), rules)?;
    let facts = applied_or_live(inp, r);
    if !facts.is_empty() {
        session.apply_evidence(facts);
    }
    let options = inp
        .options
        .iter()
        .filter(|o| o.request_id == r.request_id)
        .map(PaymentOption::from_model)
        .collect::<Result<Vec<_>>>()?;
    let d = session.decide(&r.request_id, r.request_date, &RequestSpec::from_model(r), &options)?;
    Ok((d, session))
}

fn finding(id: &str, sev: Severity, code: &'static str, detail: String) -> Finding {
    Finding { request_id: id.to_string(), severity: sev, code, detail }
}

/// Blank amounts: each blank event is either filled by an image `EventAmount` (record id is the
/// image linked to that event in images.csv, amount > 0) or listed in `missing_amounts` with no
/// cash effect. Never a silent zero.
pub fn blank_amount_findings(inp: &Inputs, d: &Decision, session: &Session) -> Vec<Finding> {
    let id = d.row.request_id.as_str();
    let mut out = Vec::new();
    let image_of: HashMap<&str, &str> =
        inp.images.iter().map(|i| (i.related_event_id.as_str(), i.image_id.as_str())).collect();
    for e in &session.ledger().entries {
        let blank = inp.events.iter().any(|m| m.event_id == e.event.id && m.amount.is_none());
        if !blank {
            if e.amount_source == AmountSource::Missing {
                out.push(finding(id, Severity::Error, "BA1_missing_not_blank", format!("{} flagged missing but has a row amount", e.event.id)));
            }
            continue;
        }
        let listed = d.facts.missing_amounts.contains(&e.event.id);
        match &e.amount_source {
            AmountSource::Row => out.push(finding(id, Severity::Error, "BA2_blank_as_row", format!("{} blank row treated as a row amount", e.event.id))),
            AmountSource::Missing => {
                if !listed {
                    out.push(finding(id, Severity::Error, "BA3_missing_unlisted", format!("{} missing but not in DecisionFacts.missing_amounts", e.event.id)));
                }
                if e.amount.is_some() || e.home_amount.is_some() {
                    out.push(finding(id, Severity::Error, "BA4_silent_amount", format!("{} missing yet carries amount {:?}", e.event.id, e.amount)));
                }
            }
            AmountSource::Evidence(rec) => {
                if image_of.get(e.event.id.as_str()) != Some(&rec.split('#').next().unwrap_or("")) {
                    out.push(finding(id, Severity::Error, "BA5_amount_not_from_linked_image", format!("{} filled by {rec}, linked image is {:?}", e.event.id, image_of.get(e.event.id.as_str()))));
                }
                if e.amount.map(|a| a <= Money::ZERO).unwrap_or(true) {
                    out.push(finding(id, Severity::Error, "BA4_silent_amount", format!("{} evidence amount {:?} is not positive", e.event.id, e.amount)));
                }
                if listed {
                    out.push(finding(id, Severity::Error, "BA3_filled_but_listed_missing", e.event.id.clone()));
                }
            }
        }
    }
    for f in d.baseline.flows.iter().filter(|f| f.amount == Money::ZERO) {
        out.push(finding(id, Severity::Error, "BA4_zero_flow", format!("zero-amount flow {:?} on {}", f.source, f.date)));
    }
    out
}

/// Numbers in the explanation that no row field, request field, profile minimum or DecisionFacts
/// value accounts for.
pub fn explanation_fact_findings(d: &Decision) -> Vec<Finding> {
    let f = &d.facts;
    let mut allowed: Vec<String> = Vec::new();
    let money = |m: Money| super::data::fmt_cents_short(((m.to_f64() * 100.0).round()) as i64);
    for m in [f.requested_amount, f.minimum_balance, f.safe_amount, f.starting_balance, f.trough_balance, f.headroom, f.reserved_pending_total] {
        allowed.push(money(m));
    }
    for p in &f.plan {
        allowed.push(money(p.amount));
    }
    allowed.push(f.plan.len().to_string());
    for c in &f.changes {
        if let Some(a) = c.new_amount {
            allowed.push(money(a));
        }
    }
    let mut dates: Vec<NaiveDate> = vec![f.request_date, f.desired_completion_date, f.trough_date, f.horizon_end];
    dates.extend(f.earliest_full_date);
    dates.extend(f.plan.iter().map(|p| p.date));
    for dt in dates {
        use chrono::Datelike;
        allowed.push(dt.day().to_string());
        allowed.push(dt.year().to_string());
    }
    allowed.push("90".into());
    let row: OutputRow = (&d.row).into();
    super::explanation::unexplained_numbers(&row.decision_explanation, &allowed)
        .into_iter()
        .map(|n| finding(&row.request_id, Severity::Error, "EX1_number_not_in_facts", format!("explanation number {n} matches no row field or DecisionFacts value")))
        .collect()
}

#[derive(Default, Debug)]
pub struct MirrorReport {
    pub rows: usize,
    pub engine_errors: Vec<String>,
    pub findings: Vec<Finding>,
    pub diverged: BTreeMap<String, Vec<String>>,
    pub missing_amount_rows: BTreeMap<String, Vec<String>>,
}

/// Run every request, compare with the shipped rows (by request_id), and collect findings.
pub fn run(dataset_dir: &Path, requests_file: &Path, shipped: &[OutputRow]) -> Result<MirrorReport> {
    let inp = Inputs::load(dataset_dir)?;
    let inv = Invariants::load(dataset_dir, requests_file)?;
    let requests = model::load_requests(requests_file)?;
    let shipped: HashMap<&str, &OutputRow> = shipped.iter().map(|r| (r.request_id.as_str(), r)).collect();
    let mut rep = MirrorReport { rows: requests.len(), ..Default::default() };
    for r in &requests {
        let (d, session) = match decide(&inp, r, Rules::default()) {
            Ok(x) => x,
            Err(e) => {
                rep.engine_errors.push(format!("{}: {e}", r.request_id));
                continue;
            }
        };
        let (b, bl, wc, wl) = (d.baseline_series(), d.baseline_low_series(), d.with_changes_series(), d.with_changes_low_series());
        let fc = ForecastSeries {
            start: r.request_date,
            minimum: d.minimum_f64(),
            baseline: &b,
            baseline_low: Some(&bl),
            with_changes: wc.as_deref(),
            with_changes_low: wl.as_deref(),
        };
        match inv.assert_row(&d.row, &fc) {
            Ok(w) => rep.findings.extend(w),
            Err(v) => rep.findings.extend(v.findings),
        }
        rep.findings.extend(blank_amount_findings(&inp, &d, &session));
        rep.findings.extend(explanation_fact_findings(&d));
        if !d.facts.missing_amounts.is_empty() {
            rep.missing_amount_rows.insert(r.request_id.clone(), d.facts.missing_amounts.clone());
        }
        let row: OutputRow = (&d.row).into();
        match shipped.get(r.request_id.as_str()) {
            Some(s) if **s != row => {
                let fields = s
                    .fields()
                    .iter()
                    .zip(row.fields())
                    .enumerate()
                    .filter(|(_, (a, b))| **a != *b)
                    .map(|(i, _)| super::contract::HEADER[i].to_string())
                    .collect();
                rep.diverged.insert(r.request_id.clone(), fields);
            }
            None => {
                rep.diverged.insert(r.request_id.clone(), vec!["row missing from shipped file".into()]);
            }
            _ => {}
        }
    }
    Ok(rep)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::ledger::{EvidenceSource, Fact};

    fn dir() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../dataset")
    }

    fn decide_with_extra(inp: &Inputs, request_id: &str, extra: Vec<EvidenceRecord>) -> (Decision, Session) {
        let reqs = model::load_requests(dir().join("requests.csv")).unwrap();
        let r = reqs.iter().find(|r| r.request_id == request_id).unwrap();
        let mut session = Session::from_model(&r.user_id, &inp.profiles, &inp.events, inp.rates.clone(), Rules::default()).unwrap();
        let mut facts = evidence_for(inp, &r.user_id, r.request_date);
        facts.extend(extra);
        session.apply_evidence(facts);
        let options: Vec<PaymentOption> = inp.options.iter().filter(|o| o.request_id == r.request_id).map(|o| PaymentOption::from_model(o).unwrap()).collect();
        let d = session.decide(&r.request_id, r.request_date, &RequestSpec::from_model(r), &options).unwrap();
        (d, session)
    }

    fn amount_record(record_id: &str, event_id: &str, amount: f64) -> EvidenceRecord {
        EvidenceRecord {
            record_id: record_id.into(),
            source: if record_id.starts_with("image_") { EvidenceSource::Image } else { EvidenceSource::Message { source_type: "bank".into() } },
            observed_at: NaiveDate::from_ymd_opt(2024, 6, 3).unwrap().and_hms_opt(0, 0, 0).unwrap(),
            fact: Fact::EventAmount { event_id: event_id.into(), amount: Money::from_f64(amount), currency: "INR".into() },
        }
    }

    #[test]
    fn blank_amount_rules_fire_and_clear() {
        let inp = Inputs::load(&dir()).unwrap();
        // As shipped: event_6033 (request_64) is blank and must be listed missing, no findings.
        let (d, s) = decide_with_extra(&inp, "request_64", vec![]);
        assert!(d.facts.missing_amounts.contains(&"event_6033".to_string()));
        assert!(blank_amount_findings(&inp, &d, &s).is_empty(), "{:?}", blank_amount_findings(&inp, &d, &s));
        // Filled by the linked image: accepted, no longer missing, and the reserve lowers safe.
        let (d2, s2) = decide_with_extra(&inp, "request_64", vec![amount_record("image_10", "event_6033", 1000.0)]);
        assert!(blank_amount_findings(&inp, &d2, &s2).is_empty(), "{:?}", blank_amount_findings(&inp, &d2, &s2));
        assert!(!d2.facts.missing_amounts.contains(&"event_6033".to_string()));
        assert!(d2.facts.safe_amount < d.facts.safe_amount);
        // Filled from a source that is not the linked image: flagged.
        let (d3, s3) = decide_with_extra(&inp, "request_64", vec![amount_record("message_999#0", "event_6033", 100.0)]);
        let codes: Vec<&str> = blank_amount_findings(&inp, &d3, &s3).iter().map(|f| f.code).collect();
        assert!(codes.contains(&"BA5_amount_not_from_linked_image"), "{codes:?}");
    }

    /// Verifier-only preview (never in the prediction path): what requests 64/73 become once their
    /// linked image amounts are applied, taking the analyst's reference from RULES.md at runtime.
    #[test]
    #[ignore]
    fn preview_64_73_with_image_reference() {
        let inp = Inputs::load(&dir()).unwrap();
        let Some(reference) = crate::evaluation::false_accepts::gold(dir().parent()) else { return };
        for (rid, image) in [("request_64", "image_10"), ("request_73", "image_11")] {
            let g = &reference[image];
            let amount = g.amounts[0] as f64 / 100.0;
            let (d0, _) = decide_with_extra(&inp, rid, vec![]);
            let (d1, s1) = decide_with_extra(&inp, rid, vec![amount_record(image, &g.event_id, amount)]);
            println!("PREVIEW {rid} now:  {:?}", d0.row);
            println!("PREVIEW {rid} with reference: {:?}  blank_findings={}", d1.row, blank_amount_findings(&inp, &d1, &s1).len());
        }
    }

    #[test]
    fn explanation_numbers_must_come_from_facts() {
        let inp = Inputs::load(&dir()).unwrap();
        let (mut d, _) = decide_with_extra(&inp, "request_26", vec![]);
        assert!(explanation_fact_findings(&d).is_empty(), "{:?}", explanation_fact_findings(&d));
        d.row.decision_explanation.push_str(" Your balance will be IDR 12,345,678.");
        assert!(explanation_fact_findings(&d).iter().any(|f| f.code == "EX1_number_not_in_facts"));
    }
}
