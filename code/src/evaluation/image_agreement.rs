//! Signoff check: image-derived `EventAmount`s are applied only through the OCR witness gate
//! (fleet/specs/ocr_vllm_pipeline.md A4; image_accuracy_plan.md §2; user rulings
//! `total_or_witnessed_sum`, `lone_printed_total`, `total_trumps_all`). The HF VLM and Anthropic
//! image paths were removed (board cleanup.remove_vlm_anthropic), so any other provenance mode is
//! an error.
//!
//! Provenance (`store/processed/image_reads/<image_id>.json`, persisted by main.rs):
//! `{ image_id, event_id, class, mode: "ocr", outcome: "witness_accept" | "fail_closed",
//! accepted_amount, reads: [{ role: "ocr", model_id, model_revision, max_dim_px, reconciled,
//! selected_amount, cutoff: {due_date, before_amount, after_amount} | null, witness, ocr_notes }] }`.
//! An `evidence` record in place of `accepted_amount` is also read.
//!
//! The gate is re-derived from the persisted read: it counts when it selected a figure (> 0 for
//! pending/scheduled events), meets any due-date cutoff (after-cutoff amount iff the cash date is
//! after the due date), carries no final-label contradiction, and names a known witness kind whose
//! computed result (if any) proves the figure. One counted read with a witness is an accept;
//! anything else is fail-closed (missing, never a guess).
//!
//! Codes: IA0 unreadable provenance; IA1 applied image amount with no provenance; IA2 image/event
//! link; IA4 applied although the gate accepts nothing; IA5 applied amount differs from the gate's;
//! IA6 recorded outcome differs from the gate (unknown outcome or mode = naming drift); IA11
//! non-positive cash-moving amount; IA13 cutoff-like read field the gate does not map; IA15 accept
//! whose supporting read names no known witness; IA17 accept whose supporting read reports a
//! final-label contradiction. A non-summing breakdown is never a contradiction (extraction never
//! reports one), so a declined but witnessed figure shows as an IA6 warning, never a pass.

use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;
use chrono::NaiveDate;
use serde::Deserialize;
use serde_json::Value;

use super::contract::{Finding, Severity};
use super::data::{Dataset, Event};

/// Amount tolerance (one currency unit: image_07 Grand Total 8,528 vs Total 8,528.10).
pub const TOLERANCE: f64 = 1.0;
/// The only image provenance mode (one deterministic Unlimited-OCR reader).
pub const OCR_MODE: &str = "ocr";
/// `extract::witness::WitnessKind::label` values plus `printed_final_label_only`
/// (`ruling.lone_printed_total`: a printed final-labeled total is accepted alone, never a computed
/// sum; it never carries `witness_computed`). Any other name proves nothing.
pub const WITNESS_KINDS: [&str; 12] = [
    "printed_final_label_only",
    "line_item_sum_witnessed_by_words",
    "line_item_sum_witnessed_by_label",
    "line_item_sum",
    "subtotal_plus_charges",
    "subtotal_plus_tax",
    "gross_minus_deductions",
    "paid_plus_balance",
    "total_minus_paid",
    "amount_in_words",
    "repeated_final_label",
    "cutoff_after_exceeds_witnessed_before",
];
const WITNESS_ACCEPT: &str = "witness_accept";
const FAIL_CLOSED: &str = "fail_closed";

fn fail(id: &str, code: &'static str, detail: String) -> Finding {
    Finding { request_id: id.to_string(), severity: Severity::Error, code, detail }
}

#[derive(Debug, Deserialize)]
pub struct Read {
    pub role: String,
    #[serde(default)]
    pub model_id: String,
    #[serde(default)]
    pub reconciled: bool,
    pub selected_amount: Option<f64>,
    #[serde(default)]
    pub due_date: Option<String>,
    #[serde(default)]
    pub before_amount: Option<f64>,
    #[serde(default)]
    pub after_amount: Option<f64>,
    /// main.rs persists the cutoff as one nested object (null when the read resolved none);
    /// folded onto the flat fields above by `Provenance::normalize`.
    #[serde(default)]
    pub cutoff: Option<CutoffObj>,
    /// The `WitnessKind` label that proved `selected_amount` on this read.
    #[serde(default)]
    pub witness: Option<String>,
    /// The witness identity's own result (image_07: 8,528.10 proving a selected 8,528).
    #[serde(default)]
    pub witness_computed: Option<f64>,
    /// First final-labeled field disagreeing with `selected_amount` (`"field=value"`).
    #[serde(default)]
    pub contradiction: Option<String>,
    /// Every other field, kept so a renamed cutoff field cannot silently switch the due-date
    /// rule off (IA13).
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

#[derive(Debug, Deserialize)]
pub struct CutoffObj {
    #[serde(default)]
    pub due_date: Option<String>,
    #[serde(default)]
    pub before_amount: Option<f64>,
    #[serde(default)]
    pub after_amount: Option<f64>,
}

/// Read fields the gate knows that are not cutoff inputs.
const KNOWN_NON_CUTOFF_FIELDS: [&str; 3] = ["currency", "error", "prompt_version"];

/// Unknown read fields that look like due-date / cutoff inputs.
fn unmapped_cutoff_fields(r: &Read) -> Vec<String> {
    r.extra
        .keys()
        .filter(|k| !KNOWN_NON_CUTOFF_FIELDS.contains(&k.as_str()))
        .filter(|k| {
            let l = k.to_lowercase();
            ["due", "cutoff", "before", "after", "deadline"].iter().any(|w| l.contains(w))
        })
        .cloned()
        .collect()
}

#[derive(Debug, Deserialize)]
pub struct Provenance {
    pub image_id: String,
    #[serde(default)]
    pub mode: Option<String>,
    pub reads: Vec<Read>,
    pub outcome: String,
    #[serde(default)]
    pub evidence: Option<Value>,
    #[serde(default)]
    pub accepted_amount: Option<f64>,
    #[serde(default)]
    pub event_id: Option<String>,
}

impl Provenance {
    /// Fold the nested `cutoff` object into the flat read fields (flat values win).
    fn normalize(&mut self) {
        for r in &mut self.reads {
            if let Some(c) = r.cutoff.take() {
                r.due_date = r.due_date.take().or(c.due_date);
                r.before_amount = r.before_amount.or(c.before_amount);
                r.after_amount = r.after_amount.or(c.after_amount);
            }
        }
    }

    fn evidence_event_amount(&self) -> (Option<String>, Option<f64>) {
        let body = self.evidence.as_ref().and_then(|e| e.get("fact")).and_then(|f| f.get("EventAmount"));
        match body {
            Some(b) => (
                b.get("event_id").and_then(Value::as_str).map(String::from),
                b.get("amount").and_then(Value::as_i64).map(|m| m as f64 / crate::engine::money::SCALE as f64),
            ),
            None => (self.accepted_amount.and(self.event_id.clone()), self.accepted_amount),
        }
    }
}

fn parse_day(s: &str) -> Option<NaiveDate> {
    ["%Y-%m-%d", "%d-%b-%Y", "%d %b %Y", "%d %B %Y", "%d/%m/%Y"].iter().find_map(|f| NaiveDate::parse_from_str(s.trim(), f).ok())
}

/// What the due-date cutoff demands of a read (RULES.md S5).
#[derive(Debug, Clone, Copy, PartialEq)]
enum Cutoff {
    /// No parseable cutoff on this read.
    None,
    /// The figure that applies on the event's cash date.
    Required(f64),
    /// Cash date is after the cutoff but the read has no after-cutoff amount: nothing valid,
    /// never fall back to the by-cutoff amount.
    NothingValid,
}

fn requirement(r: &Read, e: &Event) -> Cutoff {
    let Some(due) = r.due_date.as_deref().and_then(parse_day) else { return Cutoff::None };
    let req = if e.cash_date() > due { r.after_amount } else { r.before_amount };
    match req.filter(|v| *v > 0.0) {
        Some(v) => Cutoff::Required(v),
        None if e.cash_date() > due => Cutoff::NothingValid,
        None => Cutoff::None,
    }
}

fn close(a: f64, b: f64, tol: f64) -> bool {
    (a - b).abs() <= tol + 1e-9
}

/// Recompute (outcome, amount, notes) for one OCR provenance: accept iff a counted read carries a
/// known witness.
pub fn decide(p: &Provenance, e: &Event) -> (&'static str, Option<f64>, Vec<String>) {
    let mut notes = Vec::new();
    let reqs: Vec<f64> = p.reads.iter().filter_map(|r| match requirement(r, e) { Cutoff::Required(v) => Some(v), _ => None }).collect();
    if reqs.windows(2).any(|w| !close(w[0], w[1], TOLERANCE)) {
        notes.push(format!("reads resolve conflicting cutoff requirements {reqs:?}"));
        return ("missing", None, notes);
    }
    let req = reqs.first().copied();
    for r in &p.reads {
        let Some(a) = r.selected_amount else { continue };
        let why = (matches!(e.status.as_str(), "pending" | "scheduled") && a <= 0.0)
            .then(|| format!("{} event figure {a} <= 0", e.status))
            .or_else(|| (requirement(r, e) == Cutoff::NothingValid).then(|| format!("cash date {} is after the cutoff but no after-cutoff amount", e.cash_date())))
            .or_else(|| req.filter(|q| !close(a, *q, TOLERANCE)).map(|q| format!("cutoff requires {q} (cash date {}), read selected {a}", e.cash_date())))
            .or_else(|| r.contradiction.as_ref().map(|c| format!("final label contradicts: {c}")))
            .or_else(|| match r.witness.as_deref() {
                None => Some("no witness".to_string()),
                Some(w) if !WITNESS_KINDS.contains(&w) => Some(format!("unknown witness kind {w:?}")),
                _ => None,
            })
            .or_else(|| r.witness_computed.filter(|c| !close(*c, a, TOLERANCE)).map(|c| format!("witness_computed {c} does not prove {a}")))
            .or_else(|| (r.witness.as_deref() == Some("printed_final_label_only") && r.witness_computed.is_some()).then(|| "printed_final_label_only on a computed figure (ruling.lone_printed_total covers printed totals only)".to_string()));
        match why {
            Some(w) => notes.push(format!("{} {}: {w}", r.role, r.model_id)),
            None => return ("accept", Some(a), notes),
        }
    }
    notes.push("no counted OCR read carries a witness".into());
    ("missing", None, notes)
}

/// Findings for one provenance; records the decided outcome for the facts pass.
fn provenance_findings(p: &Provenance, event: &Event, ev_amount: Option<f64>, decided: &mut HashMap<String, (&'static str, Option<f64>, String)>) -> Vec<Finding> {
    let mut out = Vec::new();
    let id = p.image_id.as_str();
    if p.mode.as_deref() != Some(OCR_MODE) {
        let sev = if ev_amount.is_some() { Severity::Error } else { Severity::Warn };
        out.push(Finding { request_id: id.to_string(), severity: sev, code: "IA6_outcome_mismatch", detail: format!("provenance mode {:?}: only the OCR witness gate may apply image amounts", p.mode) });
        decided.insert(p.image_id.clone(), ("missing", None, "unsupported mode".into()));
        return out;
    }
    let (outcome, amount, notes) = decide(p, event);
    let recorded = match p.outcome.as_str() {
        WITNESS_ACCEPT => "accept",
        FAIL_CLOSED => "missing",
        other => {
            let sev = if ev_amount.is_some() { Severity::Error } else { Severity::Warn };
            out.push(Finding { request_id: id.to_string(), severity: sev, code: "IA6_outcome_mismatch", detail: format!("unknown outcome {other:?}: align the verifier with extraction") });
            "missing"
        }
    };
    if (p.outcome == WITNESS_ACCEPT || p.outcome == FAIL_CLOSED) && recorded != outcome {
        let sev = if recorded == "missing" { Severity::Warn } else { Severity::Error };
        out.push(Finding { request_id: id.to_string(), severity: sev, code: "IA6_outcome_mismatch", detail: format!("recorded {} but the witness gate gives {outcome}: {}", p.outcome, notes.join("; ")) });
    }
    if recorded == "accept" {
        let supporting: Vec<&Read> = p.reads.iter().filter(|r| matches!((r.selected_amount, ev_amount), (Some(a), Some(b)) if close(a, b, TOLERANCE))).collect();
        if !supporting.iter().any(|r| r.witness.as_deref().is_some_and(|w| WITNESS_KINDS.contains(&w))) {
            out.push(fail(id, "IA15_accept_without_witness", format!("accepted {ev_amount:?} but no read selecting it names a known witness kind")));
        }
        if let Some(r) = supporting.iter().find(|r| r.contradiction.is_some()) {
            out.push(fail(id, "IA17_final_label_contradiction", format!("accepted {ev_amount:?} but {} reports {}", r.role, r.contradiction.as_deref().unwrap_or(""))));
        }
        if !matches!((amount, ev_amount), (Some(a), Some(b)) if close(a, b, TOLERANCE)) {
            out.push(fail(id, "IA5_accepted_amount", format!("evidence {ev_amount:?} but the witness gate accepts {amount:?}")));
        }
    }
    decided.insert(p.image_id.clone(), (outcome, amount, notes.join("; ")));
    out
}

/// Check persisted image EventAmounts and provenance under `code_dir` against the OCR witness gate.
pub fn check(code_dir: &Path, dataset_dir: &Path) -> Result<Vec<Finding>> {
    let processed = code_dir.join("store/processed");
    let mut out = Vec::new();

    // Image EventAmounts in applied evidence: (request, image, event, amount).
    let mut facts: Vec<(String, String, String, Option<f64>)> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(processed.join("evidence")) {
        for e in rd.flatten() {
            let rid = e.file_name().to_string_lossy().trim_end_matches(".json").to_string();
            let v: Value = serde_json::from_str(&std::fs::read_to_string(e.path())?)?;
            for rec in v.as_array().into_iter().flatten() {
                let record_id = rec.get("record_id").and_then(Value::as_str).unwrap_or("");
                let Some(body) = rec.get("fact").and_then(|f| f.get("EventAmount")) else { continue };
                if !(record_id.starts_with("image_") || rec.get("source").map(|s| s == "Image").unwrap_or(false)) {
                    continue;
                }
                let amount = body.get("amount").and_then(Value::as_i64).map(|m| m as f64 / crate::engine::money::SCALE as f64);
                facts.push((rid.clone(), record_id.split('#').next().unwrap_or("").to_string(), body.get("event_id").and_then(Value::as_str).unwrap_or("").to_string(), amount));
            }
        }
    }
    let mut provs: Vec<Provenance> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(processed.join("image_reads")) {
        for entry in rd.flatten() {
            match serde_json::from_str::<Provenance>(&std::fs::read_to_string(entry.path())?) {
                Ok(mut p) => {
                    p.normalize();
                    provs.push(p)
                }
                Err(err) => out.push(fail(&entry.file_name().to_string_lossy(), "IA0_provenance_unreadable", err.to_string())),
            }
        }
    }
    if facts.is_empty() && provs.is_empty() {
        return Ok(out); // image path not run
    }

    let ds = Dataset::load(dataset_dir, &dataset_dir.join("requests.csv"))?;
    let mut link: HashMap<String, String> = HashMap::new();
    let mut rdr = csv::Reader::from_path(dataset_dir.join("images.csv"))?;
    for rec in rdr.records() {
        let rec = rec?;
        link.insert(rec[0].to_string(), rec[3].to_string());
    }

    let mut decided: HashMap<String, (&'static str, Option<f64>, String)> = HashMap::new();
    for p in &provs {
        let Some(event_id) = link.get(&p.image_id) else {
            out.push(fail(&p.image_id, "IA2_event_link", "image not in images.csv".into()));
            continue;
        };
        let (ev_event, ev_amount) = p.evidence_event_amount();
        if ev_event.as_ref().is_some_and(|x| x != event_id) {
            out.push(fail(&p.image_id, "IA2_event_link", format!("evidence targets {ev_event:?}, images.csv links {event_id}")));
        }
        let Some(event) = ds.events.get(event_id) else { continue };
        for r in &p.reads {
            let unmapped = unmapped_cutoff_fields(r);
            if !unmapped.is_empty() {
                out.push(fail(&p.image_id, "IA13_unmapped_cutoff_field", format!("{} read carries cutoff-like fields {unmapped:?} the gate does not map: the due-date rule would be skipped", r.model_id)));
            }
        }
        out.extend(provenance_findings(p, event, ev_amount, &mut decided));
    }

    for (rid, image_id, event_id, amount) in facts {
        let Some((outcome, accepted, notes)) = decided.get(&image_id) else {
            out.push(fail(&rid, "IA1_no_agreement_provenance", format!("{image_id} EventAmount for {event_id} has no valid store/processed/image_reads/{image_id}.json")));
            continue;
        };
        if link.get(&image_id) != Some(&event_id) {
            out.push(fail(&rid, "IA2_event_link", format!("{image_id} fact event {event_id} vs images.csv {:?}", link.get(&image_id))));
        }
        let status = ds.events.get(&event_id).map(|e| e.status.as_str()).unwrap_or("");
        if matches!(status, "pending" | "scheduled") && amount.map(|a| a <= 0.0).unwrap_or(true) {
            out.push(fail(&rid, "IA11_nonpositive_cash_moving", format!("{image_id} {status} event applied {amount:?}")));
        }
        if *outcome == "missing" {
            out.push(fail(&rid, "IA4_no_agreement", format!("{image_id} applied although the witness gate accepts nothing: {notes}")));
            continue;
        }
        if let (Some(a), Some(b)) = (amount, accepted) {
            if !close(a, *b, TOLERANCE) {
                out.push(fail(&rid, "IA5_accepted_amount", format!("{image_id} applied {a} but the witness gate accepts {b}")));
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    //! Dev-only fixtures shaped like the audited pages (RULES.md S5 rulings: 05 = 822.05 after the
    //! 06-Feb cutoff, 07 = 8,528, 11 = 3,650, 14 = 4,543 printed total). Events are real rows.
    use super::*;
    use serde_json::json;

    fn read(amt: Option<f64>, witness: Option<&str>, computed: Option<f64>) -> Value {
        json!({"role": "ocr", "model_id": "baidu/Unlimited-OCR", "model_revision": "", "max_dim_px": 0, "reconciled": true,
               "selected_amount": amt, "cutoff": null, "witness": witness, "witness_computed": computed, "contradiction": null, "ocr_notes": []})
    }
    fn cut(mut r: Value, due: &str, before: f64, after: f64) -> Value {
        r["cutoff"] = json!({"due_date": due, "before_amount": before, "after_amount": after});
        r
    }
    /// The shape main.rs persists: top-level accepted_amount/event_id, no evidence record.
    fn prov(image: &str, event: &str, mode: &str, reads: Vec<Value>, outcome: &str, amount: Option<f64>) -> Value {
        json!({"image_id": image, "event_id": event, "class": "", "mode": mode, "reads": reads, "outcome": outcome, "accepted_amount": amount})
    }

    fn run(tag: &str, image: &str, event: &str, p: Value) -> Vec<(&'static str, Severity)> {
        let root = std::env::temp_dir().join(format!("verifier_ia_{tag}_{}", std::process::id()));
        std::fs::create_dir_all(root.join("store/processed/image_reads")).unwrap();
        std::fs::create_dir_all(root.join("store/processed/evidence")).unwrap();
        let evidence: Vec<Value> = p["accepted_amount"]
            .as_f64()
            .map(|a| json!({"record_id": format!("{image}#ocr"), "source": "Image", "observed_at": "2024-01-01T00:00:00",
                "fact": {"EventAmount": {"event_id": event, "amount": (a * 10_000.0).round() as i64, "currency": "INR"}}}))
            .into_iter()
            .collect();
        std::fs::write(root.join(format!("store/processed/image_reads/{image}.json")), p.to_string()).unwrap();
        std::fs::write(root.join("store/processed/evidence/request_x.json"), Value::Array(evidence).to_string()).unwrap();
        let dataset = Path::new(env!("CARGO_MANIFEST_DIR")).join("../dataset");
        let f = check(&root, &dataset).unwrap();
        std::fs::remove_dir_all(&root).ok();
        f.into_iter().map(|x| (x.code, x.severity)).collect()
    }
    fn errors(v: &[(&'static str, Severity)]) -> Vec<&'static str> {
        v.iter().filter(|x| x.1 == Severity::Error).map(|x| x.0).collect()
    }
    fn gate(tag: &str, image: &str, event: &str, reads: Vec<Value>, outcome: &str, amount: Option<f64>) -> Vec<&'static str> {
        errors(&run(tag, image, event, prov(image, event, "ocr", reads, outcome, amount)))
    }

    #[test]
    fn cutoff_traps_image_05() {
        // Pre-cutoff 704.05 (witnessed by words) but event_1786 settles 2026-02-09, after 06-Feb.
        let e = gate("05a", "image_05", "event_1786", vec![cut(read(Some(704.05), Some("amount_in_words"), Some(704.05)), "06-Feb-2026", 704.05, 822.05)], "witness_accept", Some(704.05));
        assert!(e.contains(&"IA4_no_agreement") && e.contains(&"IA6_outcome_mismatch"), "{e:?}");
        assert!(gate("05b", "image_05", "event_1786", vec![cut(read(Some(822.05), Some("cutoff_after_exceeds_witnessed_before"), None), "2026-02-06", 704.05, 822.05)], "witness_accept", Some(822.05)).is_empty());
        // After the cutoff with no after-cutoff amount: nothing valid.
        let mut r = cut(read(Some(704.05), Some("amount_in_words"), None), "2026-02-06", 704.05, 0.0);
        r["cutoff"]["after_amount"] = Value::Null;
        assert!(gate("05c", "image_05", "event_1786", vec![r], "witness_accept", Some(704.05)).contains(&"IA4_no_agreement"));
    }

    #[test]
    fn witness_contradiction_and_markers() {
        // image_11: repeated final label proves 3,650; declining it is a warning, not a pass.
        assert!(gate("11a", "image_11", "event_6859", vec![read(Some(3650.0), Some("repeated_final_label"), None)], "witness_accept", Some(3650.0)).is_empty());
        let v = run("11b", "image_11", "event_6859", prov("image_11", "event_6859", "ocr", vec![read(Some(3650.0), Some("repeated_final_label"), None)], "fail_closed", None));
        assert!(errors(&v).is_empty() && v.contains(&("IA6_outcome_mismatch", Severity::Warn)), "{v:?}");
        let mut c = read(Some(3150.0), Some("line_item_sum"), Some(3150.0));
        c["contradiction"] = json!("amount_due=3650");
        let e = gate("11c", "image_11", "event_6859", vec![c], "witness_accept", Some(3150.0));
        assert!(e.contains(&"IA17_final_label_contradiction") && e.contains(&"IA4_no_agreement"), "{e:?}");
        // image_07: Grand Total 8,528 proven by Total 8,528.10; a non-proving computed result or an
        // invented kind is no accept.
        assert!(gate("07a", "image_07", "event_3231", vec![read(Some(8528.0), Some("subtotal_plus_tax"), Some(8528.10))], "witness_accept", Some(8528.0)).is_empty());
        assert!(gate("07b", "image_07", "event_3231", vec![read(Some(8528.0), Some("subtotal_plus_tax"), Some(8122.0))], "witness_accept", Some(8528.0)).contains(&"IA4_no_agreement"));
        let e = gate("07c", "image_07", "event_3231", vec![read(Some(8528.0), Some("vibes"), None)], "witness_accept", Some(8528.0));
        assert!(e.contains(&"IA15_accept_without_witness") && e.contains(&"IA4_no_agreement"), "{e:?}");
        // No witness at all.
        let e = gate("02a", "image_02", "event_1442", vec![read(Some(200000.0), None, None)], "witness_accept", Some(200000.0));
        assert!(e.contains(&"IA15_accept_without_witness") && e.contains(&"IA4_no_agreement"), "{e:?}");
        assert!(gate("02b", "image_02", "event_1442", vec![read(Some(200000.0), None, None)], "fail_closed", None).is_empty());
        // Ruling kinds: witnessed sums and a lone printed total; never printed_final_label_only on a computed figure.
        for (tag, kind) in [("14a", "printed_final_label_only"), ("14b", "line_item_sum_witnessed_by_words"), ("14c", "line_item_sum_witnessed_by_label")] {
            assert!(gate(tag, "image_14", "event_9421", vec![read(Some(4543.0), Some(kind), None)], "witness_accept", Some(4543.0)).is_empty(), "{kind}");
        }
        assert!(gate("14d", "image_14", "event_9421", vec![read(Some(4543.0), Some("printed_final_label_only"), Some(4543.0))], "witness_accept", Some(4543.0)).contains(&"IA4_no_agreement"));
        // Pending figure 0 never counts.
        let e = gate("10a", "image_10", "event_6033", vec![read(Some(0.0), Some("amount_in_words"), None)], "witness_accept", Some(0.0));
        assert!(e.contains(&"IA4_no_agreement") && e.contains(&"IA11_nonpositive_cash_moving"), "{e:?}");
        // Evidence amount must equal the gate's figure.
        assert!(gate("16a", "image_16", "event_10521", vec![read(Some(393.22), Some("amount_in_words"), None)], "witness_accept", Some(393.22)).is_empty());
    }

    #[test]
    fn drift_unmapped_fields_and_missing_provenance() {
        // Unknown outcome or a removed (non-OCR) mode applying an amount: error.
        let r = || vec![read(Some(8528.0), Some("subtotal_plus_tax"), None)];
        assert!(gate("d1", "image_07", "event_3231", r(), "accepted_v4", Some(8528.0)).contains(&"IA6_outcome_mismatch"));
        let e = errors(&run("d2", "image_07", "event_3231", prov("image_07", "event_3231", "agreement", r(), "agree", Some(8528.0))));
        assert!(e.contains(&"IA6_outcome_mismatch") && e.contains(&"IA4_no_agreement"), "{e:?}");
        // A renamed cutoff field must not silently disable the cutoff rule.
        let mut x = read(Some(704.05), Some("amount_in_words"), None);
        x["amount_payable_after_deadline"] = json!(822.05);
        assert!(gate("d3", "image_05", "event_1786", vec![x], "witness_accept", Some(704.05)).contains(&"IA13_unmapped_cutoff_field"));
        // An applied image amount with no provenance.
        let root = std::env::temp_dir().join(format!("verifier_ia_noprov_{}", std::process::id()));
        std::fs::create_dir_all(root.join("store/processed/evidence")).unwrap();
        std::fs::write(root.join("store/processed/evidence/request_73.json"), json!([{"record_id": "image_11#ocr", "source": "Image", "observed_at": "2023-01-19T00:00:00", "fact": {"EventAmount": {"event_id": "event_6859", "amount": 25_000_000_i64, "currency": "INR"}}}]).to_string()).unwrap();
        let dataset = Path::new(env!("CARGO_MANIFEST_DIR")).join("../dataset");
        assert!(check(&root, &dataset).unwrap().iter().any(|f| f.code == "IA1_no_agreement_provenance"));
        std::fs::remove_dir_all(&root).ok();
    }
}
