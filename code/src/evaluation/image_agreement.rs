//! Signoff check for board `decision.vlm_setup` (user decision) as built: image-derived
//! `EventAmount`s are accepted only per the routing table in `config/models.toml`.
//!
//! Routing table (config; the verifier validates against it, nothing hardcoded):
//! ```toml
//! [vlm_routing]
//! tolerance = 0.01                                        # documented rounding tolerance (<= 1.0)
//! fallback = { model = "moonshotai/Kimi-K3", max_dim_px = 1024 }
//!
//! [[vlm_routing.classes]]                                 # first match wins, in file order
//! name = "pending_bill_with_cutoff"
//! match = { statuses = ["pending", "scheduled"], categories = ["utilities"], settles_after_event = true }
//! readers = [ { model = "Qwen/Qwen3-VL-235B-A22B-Instruct", max_dim_px = 1024 },
//!             { model = "moonshotai/Kimi-K3", max_dim_px = 1024 } ]
//! cutoff_rule = true                                      # every read must satisfy the due-date cutoff
//! fallback = "none"                                       # optional; omitted = global fallback
//!
//! [[vlm_routing.classes]]
//! name = "default"                                        # no `match` = matches everything
//! readers = [ { model = "Qwen/Qwen3-VL-235B-A22B-Instruct", max_dim_px = 1024 },
//!             { model = "google/gemma-4-31B-it", max_dim_px = 768 } ]
//! ```
//! `match` keys (all optional, all must hold): statuses, categories, event_types, directions,
//! settles_after_event (settlement_date > event_date). The class is recomputed from the event row.
//!
//! Provenance (written by the shipped run): `store/processed/image_reads/<image_id>.json`
//! ```json
//! { "image_id": "image_05", "event_id": "event_1786", "class": "pending_bill_with_cutoff",
//!   "reads": [ { "role": "reader" | "fallback", "model_id": "…", "model_revision": "…", "max_dim_px": 1024,
//!                "reconciled": true, "selected_amount": 822.05,
//!                "cutoff": { "due_date": "2026-02-06", "before_amount": 704.05, "after_amount": 822.05 } } ],
//!   "outcome": "agree" | "fallback_tiebreak" | "missing", "accepted_amount": 822.05 }
//! ```
//! A read counts only if: its model and max_dim_px are the ones routed for its role and class; it
//! reconciled and selected a figure; for pending/scheduled events the figure is > 0; and when it
//! carries cutoff data (mandatory in `cutoff_rule` classes) the figure is the after-cutoff amount
//! iff the event's cash date is after the due date, else the before-cutoff amount.
//! Accept: all class readers count and agree within tolerance → `agree`; else a counting fallback
//! read agrees with a counting reader → `fallback_tiebreak`; else `missing`.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{anyhow, Context, Result};
use chrono::NaiveDate;
use serde::Deserialize;
use serde_json::Value;

use super::contract::{Finding, Severity};
use super::data::{Dataset, Event};

pub const MAX_TOLERANCE: f64 = 1.0;

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Reader {
    pub model: String,
    pub max_dim_px: u32,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Match {
    #[serde(default)]
    pub statuses: Vec<String>,
    #[serde(default)]
    pub categories: Vec<String>,
    #[serde(default)]
    pub event_types: Vec<String>,
    #[serde(default)]
    pub directions: Vec<String>,
    pub settles_after_event: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum FallbackSpec {
    Reader(Reader),
    Keyword(String),
}

#[derive(Debug, Clone, Deserialize)]
pub struct Class {
    pub name: String,
    #[serde(default, rename = "match")]
    pub matcher: Option<Match>,
    pub readers: Vec<Reader>,
    #[serde(default)]
    pub cutoff_rule: bool,
    pub fallback: Option<FallbackSpec>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Routing {
    pub tolerance: f64,
    pub fallback: Option<Reader>,
    pub classes: Vec<Class>,
}

impl Routing {
    pub fn from_models_toml(text: &str) -> Result<Option<Routing>> {
        let v: toml::Value = toml::from_str(text).context("parse models.toml")?;
        match v.get("vlm_routing") {
            None => Ok(None),
            Some(r) => Ok(Some(r.clone().try_into().map_err(|e| anyhow!("[vlm_routing]: {e}"))?)),
        }
    }

    pub fn class_for(&self, e: &Event) -> Option<&Class> {
        self.classes.iter().find(|c| {
            let Some(m) = &c.matcher else { return true };
            let inlist = |list: &Vec<String>, v: &str| list.is_empty() || list.iter().any(|x| x == v);
            inlist(&m.statuses, &e.status)
                && inlist(&m.categories, &e.category)
                && inlist(&m.event_types, &e.event_type)
                && inlist(&m.directions, &e.direction)
                && m.settles_after_event.map(|want| (e.cash_date() > e.event_date) == want).unwrap_or(true)
        })
    }

    pub fn fallback_for<'a>(&'a self, c: &'a Class) -> Option<&'a Reader> {
        match &c.fallback {
            Some(FallbackSpec::Keyword(k)) if k == "none" => None,
            Some(FallbackSpec::Reader(r)) => Some(r),
            _ => self.fallback.as_ref(),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct Cutoff {
    pub due_date: Option<String>,
    pub before_amount: Option<f64>,
    pub after_amount: Option<f64>,
}

#[derive(Debug, Deserialize)]
pub struct Read {
    pub role: String,
    pub model_id: String,
    #[serde(default)]
    pub model_revision: Option<String>,
    pub max_dim_px: Option<u32>,
    pub reconciled: bool,
    pub selected_amount: Option<f64>,
    #[serde(default)]
    pub cutoff: Option<Cutoff>,
}

#[derive(Debug, Deserialize)]
pub struct Provenance {
    pub image_id: String,
    pub event_id: String,
    pub class: String,
    pub reads: Vec<Read>,
    pub outcome: String,
    pub accepted_amount: Option<f64>,
}

fn fail(id: &str, code: &'static str, detail: String) -> Finding {
    Finding { request_id: id.to_string(), severity: Severity::Error, code, detail }
}

fn parse_day(s: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(s, "%Y-%m-%d").or_else(|_| NaiveDate::parse_from_str(s, "%d-%b-%Y")).ok()
}

/// Why a read does not count (None = it counts).
fn read_problem(read: &Read, routed: Option<&Reader>, class: &Class, event: &Event, tol: f64) -> Option<String> {
    let Some(routed) = routed else { return Some("model/role not routed for this class".into()) };
    if read.model_id != routed.model || read.max_dim_px != Some(routed.max_dim_px) {
        return Some(format!("routed {}@{} but read is {}@{:?}", routed.model, routed.max_dim_px, read.model_id, read.max_dim_px));
    }
    if !read.reconciled {
        return Some("not reconciled".into());
    }
    let Some(amount) = read.selected_amount else { return Some("no selected figure".into()) };
    if matches!(event.status.as_str(), "pending" | "scheduled") && amount <= 0.0 {
        return Some(format!("{} event figure {amount} <= 0", event.status));
    }
    match &read.cutoff {
        None if class.cutoff_rule => Some("cutoff_rule class but read carries no cutoff data".into()),
        None => None,
        Some(c) => {
            let (Some(due), Some(before), Some(after)) = (c.due_date.as_deref().and_then(parse_day), c.before_amount, c.after_amount) else {
                return if class.cutoff_rule { Some("cutoff data incomplete (due date / before / after)".into()) } else { None };
            };
            let expected = if event.cash_date() > due { after } else { before };
            if (amount - expected).abs() > tol + 1e-9 {
                Some(format!("cutoff rule: cash date {} vs due {due} expects {expected}, read selected {amount}", event.cash_date()))
            } else {
                None
            }
        }
    }
}

/// Recompute (outcome, amount) for a provenance under the routing table.
pub fn decide(routing: &Routing, class: &Class, p: &Provenance, event: &Event) -> (&'static str, Option<f64>, Vec<String>) {
    let tol = routing.tolerance;
    let mut notes = Vec::new();
    let mut reader_amounts: Vec<Option<f64>> = Vec::new();
    for routed in &class.readers {
        let read = p.reads.iter().find(|r| r.role == "reader" && r.model_id == routed.model);
        let amt = match read {
            None => {
                notes.push(format!("{} read missing", routed.model));
                None
            }
            Some(r) => match read_problem(r, Some(routed), class, event, tol) {
                Some(why) => {
                    notes.push(format!("{}: {why}", r.model_id));
                    None
                }
                None => r.selected_amount,
            },
        };
        reader_amounts.push(amt);
    }
    let close = |a: f64, b: f64| (a - b).abs() <= tol + 1e-9;
    if !reader_amounts.is_empty() && reader_amounts.iter().all(Option::is_some) {
        let first = reader_amounts[0].unwrap();
        if reader_amounts.iter().all(|a| close(a.unwrap(), first)) {
            return ("agree", Some(first), notes);
        }
        notes.push("readers disagree".into());
    }
    if let Some(fb_routed) = routing.fallback_for(class) {
        if let Some(fr) = p.reads.iter().find(|r| r.role == "fallback") {
            match read_problem(fr, Some(fb_routed), class, event, tol) {
                Some(why) => notes.push(format!("fallback {}: {why}", fr.model_id)),
                None => {
                    let f = fr.selected_amount.unwrap();
                    if reader_amounts.iter().flatten().any(|a| close(*a, f)) {
                        return ("fallback_tiebreak", Some(f), notes);
                    }
                    notes.push(format!("fallback {f} matches no counting reader"));
                }
            }
        }
    }
    ("missing", None, notes)
}

/// Check persisted image EventAmounts and provenance under `code_dir` against the routing table.
pub fn check(code_dir: &Path, dataset_dir: &Path, models_toml: &Path) -> Result<Vec<Finding>> {
    let processed = code_dir.join("store/processed");
    let mut out = Vec::new();

    // Image EventAmounts in applied evidence.
    let mut facts: Vec<(String, String, String, Option<f64>)> = Vec::new(); // (request, image, event, amount)
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
    let prov_dir = processed.join("image_reads");
    let has_provenance = prov_dir.exists();
    if facts.is_empty() && !has_provenance {
        return Ok(out); // model path not run: nothing to check
    }

    let routing = match std::fs::read_to_string(models_toml).map_err(anyhow::Error::from).and_then(|t| Routing::from_models_toml(&t)) {
        Ok(Some(r)) => r,
        Ok(None) => {
            out.push(fail("config", "IA9_no_routing_table", format!("{} has no [vlm_routing] but image evidence exists", models_toml.display())));
            return Ok(out);
        }
        Err(e) => {
            out.push(fail("config", "IA9_no_routing_table", e.to_string()));
            return Ok(out);
        }
    };
    if !(0.0..=MAX_TOLERANCE).contains(&routing.tolerance) {
        out.push(fail("config", "IA8_tolerance_out_of_range", format!("tolerance {} not in [0, {MAX_TOLERANCE}]", routing.tolerance)));
    }

    let ds = Dataset::load(dataset_dir, &dataset_dir.join("requests.csv"))?;
    let mut link: HashMap<String, String> = HashMap::new();
    let mut rdr = csv::Reader::from_path(dataset_dir.join("images.csv"))?;
    for rec in rdr.records() {
        let rec = rec?;
        link.insert(rec[0].to_string(), rec[3].to_string());
    }

    let mut decided: HashMap<String, (&'static str, Option<f64>, String)> = HashMap::new();
    if let Ok(rd) = std::fs::read_dir(&prov_dir) {
        for e in rd.flatten() {
            let p: Provenance = match serde_json::from_str(&std::fs::read_to_string(e.path())?) {
                Ok(p) => p,
                Err(err) => {
                    out.push(fail(&e.file_name().to_string_lossy(), "IA0_provenance_unreadable", err.to_string()));
                    continue;
                }
            };
            if link.get(&p.image_id) != Some(&p.event_id) {
                out.push(fail(&p.image_id, "IA2_event_link", format!("provenance event {} but images.csv links {:?}", p.event_id, link.get(&p.image_id))));
                continue;
            }
            let Some(event) = ds.events.get(&p.event_id) else {
                out.push(fail(&p.image_id, "IA2_event_link", format!("unknown event {}", p.event_id)));
                continue;
            };
            let Some(class) = routing.class_for(event) else {
                out.push(fail(&p.image_id, "IA10_no_class", format!("no routing class matches {} ({} {} {})", event.event_id, event.status, event.category, event.event_type)));
                continue;
            };
            if p.class != class.name {
                out.push(fail(&p.image_id, "IA10_class_mismatch", format!("provenance class {} but routing table gives {} for {}", p.class, class.name, event.event_id)));
            }
            let fallback = routing.fallback_for(class);
            for r in &p.reads {
                let routed = match r.role.as_str() {
                    "reader" => class.readers.iter().find(|x| x.model == r.model_id),
                    "fallback" => fallback,
                    _ => None,
                };
                if routed.map(|x| x.model != r.model_id || Some(x.max_dim_px) != r.max_dim_px).unwrap_or(true) {
                    out.push(fail(&p.image_id, "IA3_reader_not_routed", format!("{} read {}@{:?} is not routed for class {}", r.role, r.model_id, r.max_dim_px, class.name)));
                }
            }
            let (outcome, amount, notes) = decide(&routing, class, &p, event);
            if outcome != p.outcome {
                out.push(fail(&p.image_id, "IA6_outcome_mismatch", format!("recorded {} but routing gives {outcome}: {}", p.outcome, notes.join("; "))));
            }
            let same = match (amount, p.accepted_amount) {
                (Some(a), Some(b)) => (a - b).abs() <= routing.tolerance + 1e-9,
                (None, None) => true,
                _ => false,
            };
            if !same {
                out.push(fail(&p.image_id, "IA5_accepted_amount", format!("recorded {:?} but routing accepts {amount:?}", p.accepted_amount)));
            }
            decided.insert(p.image_id.clone(), (outcome, amount, notes.join("; ")));
        }
    }

    for (rid, image_id, event_id, amount) in facts {
        let Some((outcome, accepted, notes)) = decided.get(&image_id) else {
            out.push(fail(&rid, "IA1_no_agreement_provenance", format!("{image_id} EventAmount for {event_id} has no valid store/processed/image_reads/{image_id}.json")));
            continue;
        };
        if link.get(&image_id) != Some(&event_id) {
            out.push(fail(&rid, "IA2_event_link", format!("{image_id} fact event {event_id} vs images.csv {:?}", link.get(&image_id))));
        }
        if *outcome == "missing" {
            out.push(fail(&rid, "IA4_no_agreement", format!("{image_id} applied although routing accepts nothing: {notes}")));
            continue;
        }
        let status = ds.events.get(&event_id).map(|e| e.status.as_str()).unwrap_or("");
        if matches!(status, "pending" | "scheduled") && amount.map(|a| a <= 0.0).unwrap_or(true) {
            out.push(fail(&rid, "IA11_nonpositive_cash_moving", format!("{image_id} {status} event applied {amount:?}")));
        }
        if let (Some(a), Some(b)) = (amount, accepted) {
            if (a - b).abs() > routing.tolerance + 1e-9 {
                out.push(fail(&rid, "IA5_accepted_amount", format!("{image_id} applied {a} but routing accepts {b}")));
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const Q: &str = "Qwen/Qwen3-VL-235B-A22B-Instruct";
    const G: &str = "google/gemma-4-31B-it";
    const K: &str = "moonshotai/Kimi-K3";

    const ROUTING: &str = r#"
[selected]
vlm_primary = "Qwen/Qwen3-VL-235B-A22B-Instruct"

[vlm_routing]
tolerance = 0.01
fallback = { model = "moonshotai/Kimi-K3", max_dim_px = 1024 }

[[vlm_routing.classes]]
name = "pending_bill_with_cutoff"
match = { statuses = ["pending", "scheduled"], categories = ["utilities"], settles_after_event = true }
readers = [ { model = "Qwen/Qwen3-VL-235B-A22B-Instruct", max_dim_px = 1024 }, { model = "moonshotai/Kimi-K3", max_dim_px = 1024 } ]
cutoff_rule = true
fallback = "none"

[[vlm_routing.classes]]
name = "default"
readers = [ { model = "Qwen/Qwen3-VL-235B-A22B-Instruct", max_dim_px = 1024 }, { model = "google/gemma-4-31B-it", max_dim_px = 768 } ]
"#;

    fn read(role: &str, model: &str, dim: u32, ok: bool, amt: f64) -> Value {
        json!({"role": role, "model_id": model, "model_revision": "x", "max_dim_px": dim, "reconciled": ok, "selected_amount": amt})
    }

    fn with_cutoff(mut r: Value, due: &str, before: f64, after: f64) -> Value {
        r["cutoff"] = json!({"due_date": due, "before_amount": before, "after_amount": after});
        r
    }

    fn prov(image: &str, event: &str, class: &str, reads: Vec<Value>, outcome: &str, acc: Option<f64>) -> Value {
        json!({"image_id": image, "event_id": event, "class": class, "reads": reads, "outcome": outcome, "accepted_amount": acc})
    }

    /// Returns error codes for one image with the given provenance and applied amount (None = not applied).
    fn run(tag: &str, image: &str, event: &str, provenance: Value, applied: Option<f64>, routing: &str) -> Vec<&'static str> {
        let root = std::env::temp_dir().join(format!("verifier_ia2_{tag}_{}", std::process::id()));
        std::fs::create_dir_all(root.join("store/processed/image_reads")).unwrap();
        std::fs::create_dir_all(root.join("store/processed/evidence")).unwrap();
        std::fs::write(root.join(format!("store/processed/image_reads/{image}.json")), provenance.to_string()).unwrap();
        let ev: Vec<Value> = applied
            .map(|a| vec![json!({"record_id": image, "source": "Image", "observed_at": "2024-01-01T00:00:00",
                "fact": {"EventAmount": {"event_id": event, "amount": (a * 10_000.0).round() as i64, "currency": "INR"}}})])
            .unwrap_or_default();
        std::fs::write(root.join("store/processed/evidence/request_x.json"), Value::Array(ev).to_string()).unwrap();
        let toml_path = root.join("models.toml");
        std::fs::write(&toml_path, routing).unwrap();
        let dataset = Path::new(env!("CARGO_MANIFEST_DIR")).join("../dataset");
        let f = check(&root, &dataset, &toml_path).unwrap();
        std::fs::remove_dir_all(&root).ok();
        f.into_iter().map(|x| x.code).collect()
    }

    #[test]
    fn classes_come_from_the_config_table() {
        let routing = Routing::from_models_toml(ROUTING).unwrap().unwrap();
        let dataset = Path::new(env!("CARGO_MANIFEST_DIR")).join("../dataset");
        let ds = Dataset::load(&dataset, &dataset.join("requests.csv")).unwrap();
        let class = |e: &str| routing.class_for(&ds.events[e]).unwrap().name.clone();
        assert_eq!(class("event_1786"), "pending_bill_with_cutoff"); // image_05 telecom bill, settles after due
        for e in ["event_6033", "event_6859", "event_1442", "event_253", "event_7307"] {
            assert_eq!(class(e), "default", "{e}");
        }
        assert!(Routing::from_models_toml("[selected]\nvlm_primary = \"x\"\n").unwrap().is_none());
    }

    #[test]
    fn default_class_agreement_and_tiebreak() {
        // image_10 (pending grocery invoice, default class): 235B@1024 + gemma@768 agree.
        let ok = prov("image_10", "event_6033", "default", vec![read("reader", Q, 1024, true, 79679.26), read("reader", G, 768, true, 79679.26)], "agree", Some(79679.26));
        assert!(run("a", "image_10", "event_6033", ok, Some(79679.26), ROUTING).is_empty());
        // Per-reader px: gemma at 1024 is not routed and does not count.
        let px = prov("image_10", "event_6033", "default", vec![read("reader", Q, 1024, true, 79679.26), read("reader", G, 1024, true, 79679.26)], "agree", Some(79679.26));
        let c = run("b", "image_10", "event_6033", px, Some(79679.26), ROUTING);
        assert!(c.contains(&"IA3_reader_not_routed") && c.contains(&"IA4_no_agreement"), "{c:?}");
        // gemma rejected (reconcile fail), Kimi matches 235B: tiebreak accept.
        let tie = prov("image_10", "event_6033", "default", vec![read("reader", Q, 1024, true, 79679.26), read("reader", G, 768, false, 72045.0), read("fallback", K, 1024, true, 79679.26)], "fallback_tiebreak", Some(79679.26));
        assert!(run("c", "image_10", "event_6033", tie, Some(79679.26), ROUTING).is_empty());
        // Kimi matches gemma's figure but gemma did not reconcile: nothing counts -> missing.
        let bad = prov("image_10", "event_6033", "default", vec![read("reader", Q, 1024, true, 79679.26), read("reader", G, 768, false, 72045.0), read("fallback", K, 1024, true, 72045.0)], "missing", None);
        assert!(run("d", "image_10", "event_6033", bad, None, ROUTING).is_empty());
        // Recorded as a tiebreak and applied anyway: outcome mismatch and no agreement.
        let lie = prov("image_10", "event_6033", "default", vec![read("reader", Q, 1024, true, 79679.26), read("reader", G, 768, false, 72045.0), read("fallback", K, 1024, true, 72045.0)], "fallback_tiebreak", Some(72045.0));
        let c = run("e", "image_10", "event_6033", lie, Some(72045.0), ROUTING);
        assert!(c.contains(&"IA6_outcome_mismatch") && c.contains(&"IA4_no_agreement"), "{c:?}");
    }

    #[test]
    fn pending_bill_class_routes_235b_and_kimi_with_cutoff() {
        // image_05: due 06-Feb-2026, event settles 2026-02-09 -> after-cutoff 822.05.
        let q = with_cutoff(read("reader", Q, 1024, true, 822.05), "2026-02-06", 704.05, 822.05);
        let k = with_cutoff(read("reader", K, 1024, true, 822.05), "06-Feb-2026", 704.05, 822.05);
        let ok = prov("image_05", "event_1786", "pending_bill_with_cutoff", vec![q, k], "agree", Some(822.05));
        assert!(run("f", "image_05", "event_1786", ok, Some(822.05), ROUTING).is_empty());
        // gemma routed into this class is not allowed.
        let g = prov("image_05", "event_1786", "pending_bill_with_cutoff", vec![with_cutoff(read("reader", Q, 1024, true, 822.05), "2026-02-06", 704.05, 822.05), with_cutoff(read("reader", G, 768, true, 822.05), "2026-02-06", 704.05, 822.05)], "agree", Some(822.05));
        let c = run("g", "image_05", "event_1786", g, Some(822.05), ROUTING);
        assert!(c.contains(&"IA3_reader_not_routed") && c.contains(&"IA4_no_agreement"), "{c:?}");
        // The image_05 trap: both select 704.05 (before-cutoff) although cash date is after due.
        let trap = prov("image_05", "event_1786", "pending_bill_with_cutoff", vec![with_cutoff(read("reader", Q, 1024, true, 704.05), "2026-02-06", 704.05, 822.05), with_cutoff(read("reader", K, 1024, true, 704.05), "2026-02-06", 704.05, 822.05)], "agree", Some(704.05));
        let c = run("h", "image_05", "event_1786", trap, Some(704.05), ROUTING);
        assert!(c.contains(&"IA4_no_agreement"), "{c:?}");
        // Cutoff data missing in a cutoff_rule class: read does not count.
        let nocut = prov("image_05", "event_1786", "pending_bill_with_cutoff", vec![read("reader", Q, 1024, true, 822.05), read("reader", K, 1024, true, 822.05)], "agree", Some(822.05));
        assert!(run("i", "image_05", "event_1786", nocut, Some(822.05), ROUTING).contains(&"IA4_no_agreement"));
        // 235B reads 0.0 on a pending bill: never counts; applying 0 fails.
        let zero = prov("image_05", "event_1786", "pending_bill_with_cutoff", vec![with_cutoff(read("reader", Q, 1024, true, 0.0), "2026-02-06", 0.0, 0.0), with_cutoff(read("reader", K, 1024, true, 0.0), "2026-02-06", 0.0, 0.0)], "agree", Some(0.0));
        let c = run("j", "image_05", "event_1786", zero, Some(0.0), ROUTING);
        assert!(c.contains(&"IA4_no_agreement") && c.contains(&"IA6_outcome_mismatch"), "{c:?}");
        // Wrong class recorded.
        let wrong = prov("image_05", "event_1786", "default", vec![with_cutoff(read("reader", Q, 1024, true, 822.05), "2026-02-06", 704.05, 822.05), with_cutoff(read("reader", K, 1024, true, 822.05), "2026-02-06", 704.05, 822.05)], "agree", Some(822.05));
        assert!(run("k", "image_05", "event_1786", wrong, Some(822.05), ROUTING).contains(&"IA10_class_mismatch"));
    }

    #[test]
    fn tiebreak_matched_read_must_satisfy_cutoff() {
        // Default class, a bill page with a cutoff (image_02 rent, scheduled, settles 2023-08-16):
        // 235B and gemma disagree; Kimi picks the before-cutoff figure matching gemma, but the
        // cash date is after the due date -> neither counts -> missing, applying it fails.
        let q = with_cutoff(read("reader", Q, 1024, true, 100000.0), "2023-08-11", 90000.0, 100000.0);
        let g = with_cutoff(read("reader", G, 768, true, 90000.0), "2023-08-11", 90000.0, 100000.0);
        let k = with_cutoff(read("fallback", K, 1024, true, 90000.0), "2023-08-11", 90000.0, 100000.0);
        let p = prov("image_02", "event_1442", "default", vec![q, g, k], "fallback_tiebreak", Some(90000.0));
        let c = run("l", "image_02", "event_1442", p, Some(90000.0), ROUTING);
        assert!(c.contains(&"IA4_no_agreement") && c.contains(&"IA6_outcome_mismatch"), "{c:?}");
    }

    #[test]
    fn missing_routing_or_provenance_fails() {
        let ok = prov("image_10", "event_6033", "default", vec![read("reader", Q, 1024, true, 79679.26), read("reader", G, 768, true, 79679.26)], "agree", Some(79679.26));
        assert!(run("m", "image_10", "event_6033", ok, Some(79679.26), "[selected]\nvlm_primary = \"x\"\n").contains(&"IA9_no_routing_table"));
        // Image fact without provenance.
        let root = std::env::temp_dir().join(format!("verifier_ia2_noprov_{}", std::process::id()));
        std::fs::create_dir_all(root.join("store/processed/evidence")).unwrap();
        std::fs::write(root.join("store/processed/evidence/request_73.json"), json!([{"record_id": "image_11", "source": "Image", "observed_at": "2023-01-19T00:00:00", "fact": {"EventAmount": {"event_id": "event_6859", "amount": 36_500_000_i64, "currency": "INR"}}}]).to_string()).unwrap();
        let toml_path = root.join("models.toml");
        std::fs::write(&toml_path, ROUTING).unwrap();
        let dataset = Path::new(env!("CARGO_MANIFEST_DIR")).join("../dataset");
        assert!(check(&root, &dataset, &toml_path).unwrap().iter().any(|f| f.code == "IA1_no_agreement_provenance"));
        std::fs::remove_dir_all(&root).ok();
    }
}
