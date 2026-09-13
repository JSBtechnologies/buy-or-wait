//! Signoff check for board decisions `vlm_setup`, `tiebreak_distinct` and `vlm_routing_v2`,
//! against the design as built by extraction (32c17d5, shape posted in extract#234): image-derived
//! `EventAmount`s are accepted only per the routing table in `config/models.toml`.
//!
//! Routing table (`[vlm_routing]`, extraction's `VlmRoutingConfig`; must be explicit in config
//! for signoff, the verifier validates against it and hardcodes no model):
//! ```toml
//! [vlm_routing]
//! default_class = "settled_expense_receipt"
//! tolerance = 1.0                                   # optional; else the documented 1.0, capped at 1.0
//! [[vlm_routing.classes]]
//! name = "pending_bill_due_date"
//! statuses = ["pending", "scheduled"]               # event_types / statuses / categories; empty = any
//! readers = [ { role = "vlm_primary", max_dim_px = 1024, max_tokens = 400 },
//!             { role = "vlm_fallback", max_dim_px = 1024, max_tokens = 1500 } ]
//! tiebreak = { role = "vlm_escalation", max_dim_px = 768, max_tokens = 400 }   # optional
//! ```
//! Roles resolve through `[selected]` (vlm_primary / vlm_escalation / vlm_fallback), so a model
//! swap (e.g. routing v2: claude-opus-5 in the fallback role, served by provider "anthropic")
//! needs no change here. Without an explicit `tiebreak`, the tiebreak is the one VLM role that is
//! not a class reader, at the resolution/tokens that role has in any other class (else
//! `[selected].image_max_dim_px`). decision.tiebreak_distinct: the tiebreak model must differ
//! from both class readers (IA12).
//!
//! Provenance (`store/processed/image_reads/<image_id>.json`, extraction's `ImageResolution`):
//! `{ image_id, class, mode, reads: [{ role, model_id, model_revision, max_dim_px, max_tokens,
//! reconciled, selected_amount, currency, due_date, before_amount, after_amount, error, ... }],
//! outcome: "agree" | "tiebreak_accept" | "no_agreement" | "no_route", evidence }`. Extra read
//! fields (e.g. `provider`) are accepted; agreement and the audit gold gate apply the same way.
//!
//! A read counts only if its model, max_dim_px and max_tokens are those routed for its slot, it
//! reconciled and selected a figure, the figure is > 0 for pending/scheduled events, and it
//! equals the due-date cutoff requirement when any read of the image resolved one (after-cutoff
//! amount iff the cash date is after the due date). Accept: both class readers count and agree
//! within tolerance → agree; else a counting tiebreak read agrees with a counting reader →
//! tiebreak; else nothing is accepted (missing, never a guess).

use std::collections::HashMap;
use std::path::Path;

use anyhow::{anyhow, Context, Result};
use chrono::NaiveDate;
use serde::Deserialize;
use serde_json::Value;

use super::contract::{Finding, Severity};
use super::data::{Dataset, Event};

pub const MAX_TOLERANCE: f64 = 1.0;
pub const DEFAULT_TOLERANCE: f64 = 1.0;
const VLM_ROLES: [&str; 3] = ["vlm_primary", "vlm_escalation", "vlm_fallback"];

#[derive(Debug, Clone, Deserialize)]
pub struct SlotCfg {
    pub role: String,
    pub max_dim_px: Option<u32>,
    pub max_tokens: Option<u32>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ClassCfg {
    pub name: String,
    #[serde(default)]
    pub event_types: Vec<String>,
    #[serde(default)]
    pub statuses: Vec<String>,
    #[serde(default)]
    pub categories: Vec<String>,
    pub readers: Vec<SlotCfg>,
    pub tiebreak: Option<SlotCfg>,
}

#[derive(Debug, Clone, Deserialize)]
struct RoutingCfg {
    default_class: Option<String>,
    #[serde(default)]
    classes: Vec<ClassCfg>,
    tolerance: Option<f64>,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct SelectedCfg {
    vlm_primary: Option<String>,
    vlm_escalation: Option<String>,
    vlm_fallback: Option<String>,
    image_max_dim_px: Option<u32>,
}

/// A resolved reading slot.
#[derive(Debug, Clone, PartialEq)]
pub struct Slot {
    pub role: String,
    pub model: String,
    pub max_dim_px: u32,
    pub max_tokens: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct Class {
    pub name: String,
    pub cfg: ClassCfg,
    pub readers: Vec<Slot>,
    pub tiebreak: Option<Slot>,
}

#[derive(Debug, Clone)]
pub struct Routing {
    pub tolerance: f64,
    pub default_class: Option<String>,
    pub classes: Vec<Class>,
    /// Config problems found while resolving (unresolvable roles, tiebreak == reader).
    pub problems: Vec<Finding>,
}

fn fail(id: &str, code: &'static str, detail: String) -> Finding {
    Finding { request_id: id.to_string(), severity: Severity::Error, code, detail }
}

impl Routing {
    pub fn from_models_toml(text: &str) -> Result<Option<Routing>> {
        let v: toml::Value = toml::from_str(text).context("parse models.toml")?;
        let Some(r) = v.get("vlm_routing") else { return Ok(None) };
        let cfg: RoutingCfg = r.clone().try_into().map_err(|e| anyhow!("[vlm_routing]: {e}"))?;
        let sel: SelectedCfg = v.get("selected").cloned().map(|s| s.try_into()).transpose().map_err(|e| anyhow!("[selected]: {e}"))?.unwrap_or_default();
        let model_of = |role: &str| match role {
            "vlm_primary" => sel.vlm_primary.clone(),
            "vlm_escalation" => sel.vlm_escalation.clone(),
            "vlm_fallback" => sel.vlm_fallback.clone(),
            _ => None,
        };
        let global_px = sel.image_max_dim_px.unwrap_or(1024);
        let mut problems = Vec::new();
        let resolve = |s: &SlotCfg, problems: &mut Vec<Finding>, class: &str| -> Option<Slot> {
            match model_of(&s.role) {
                Some(model) => Some(Slot { role: s.role.clone(), model, max_dim_px: s.max_dim_px.unwrap_or(global_px), max_tokens: s.max_tokens }),
                None => {
                    problems.push(fail("config", "IA9_unresolved_role", format!("class {class}: role {} does not resolve through [selected]", s.role)));
                    None
                }
            }
        };
        // Resolution/tokens a role has anywhere in the table (for a derived tiebreak slot).
        let role_spec = |role: &str| cfg.classes.iter().flat_map(|c| c.readers.iter().chain(c.tiebreak.iter())).find(|s| s.role == role && s.max_dim_px.is_some()).cloned();
        let mut classes = Vec::new();
        for c in &cfg.classes {
            let readers: Vec<Slot> = c.readers.iter().filter_map(|s| resolve(s, &mut problems, &c.name)).collect();
            if readers.len() != 2 {
                problems.push(fail("config", "IA9_class_needs_two_readers", format!("class {} resolves {} readers", c.name, readers.len())));
            }
            let tiebreak = match &c.tiebreak {
                Some(t) => resolve(t, &mut problems, &c.name),
                None => {
                    let remaining: Vec<&str> = VLM_ROLES.iter().copied().filter(|r| !c.readers.iter().any(|s| s.role == *r)).collect();
                    match remaining.as_slice() {
                        [role] => {
                            let spec = role_spec(role).unwrap_or(SlotCfg { role: role.to_string(), max_dim_px: None, max_tokens: None });
                            model_of(role).map(|model| Slot { role: role.to_string(), model, max_dim_px: spec.max_dim_px.unwrap_or(global_px), max_tokens: spec.max_tokens })
                        }
                        _ => None,
                    }
                }
            };
            if let Some(t) = &tiebreak {
                if readers.iter().any(|r| r.model == t.model) {
                    problems.push(fail("config", "IA12_tiebreak_equals_reader", format!("class {} tiebreak {} ({}) is also one of its readers", c.name, t.model, t.role)));
                }
            }
            classes.push(Class { name: c.name.clone(), cfg: c.clone(), readers, tiebreak });
        }
        let tolerance = cfg.tolerance.unwrap_or(DEFAULT_TOLERANCE);
        if !(0.0..=MAX_TOLERANCE).contains(&tolerance) {
            problems.push(fail("config", "IA8_tolerance_out_of_range", format!("tolerance {tolerance} not in [0, {MAX_TOLERANCE}]")));
        }
        Ok(Some(Routing { tolerance, default_class: cfg.default_class, classes, problems }))
    }

    /// Deterministic class from the event row: first class whose non-empty filters all match,
    /// else `default_class`.
    pub fn class_for(&self, e: &Event) -> Option<&Class> {
        let ok = |list: &Vec<String>, v: &str| list.is_empty() || list.iter().any(|x| x == v);
        self.classes
            .iter()
            .find(|c| ok(&c.cfg.event_types, &e.event_type) && ok(&c.cfg.statuses, &e.status) && ok(&c.cfg.categories, &e.category))
            .or_else(|| self.default_class.as_ref().and_then(|d| self.classes.iter().find(|c| &c.name == d)))
    }
}

#[derive(Debug, Deserialize)]
pub struct Read {
    pub role: String,
    pub model_id: String,
    #[serde(default)]
    pub model_revision: Option<String>,
    #[serde(default)]
    pub provider: Option<String>,
    pub max_dim_px: Option<u32>,
    pub max_tokens: Option<u32>,
    pub reconciled: bool,
    pub selected_amount: Option<f64>,
    /// v1 `due_date`; prompt v2 (RULES.md S5, analyst 40edcb5) `due_cutoff_date`.
    #[serde(default, alias = "due_cutoff_date")]
    pub due_date: Option<String>,
    /// v1 `before_amount`; v2 `amount_due_by_cutoff`.
    #[serde(default, alias = "amount_due_by_cutoff")]
    pub before_amount: Option<f64>,
    /// v1 `after_amount`; v2 `amount_due_after_cutoff`.
    #[serde(default, alias = "amount_due_after_cutoff")]
    pub after_amount: Option<f64>,
    /// Every other field of the read, kept so a renamed cutoff field cannot silently switch
    /// the due-date rule off (IA13).
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
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
    pub class: Option<String>,
    #[serde(default)]
    pub mode: Option<String>,
    pub reads: Vec<Read>,
    pub outcome: String,
    pub evidence: Option<Value>,
}

impl Provenance {
    fn evidence_event_amount(&self) -> (Option<String>, Option<f64>) {
        let body = self.evidence.as_ref().and_then(|e| e.get("fact")).and_then(|f| f.get("EventAmount"));
        (
            body.and_then(|b| b.get("event_id")).and_then(Value::as_str).map(String::from),
            body.and_then(|b| b.get("amount")).and_then(Value::as_i64).map(|m| m as f64 / crate::engine::money::SCALE as f64),
        )
    }
}

fn parse_day(s: &str) -> Option<NaiveDate> {
    ["%Y-%m-%d", "%d-%b-%Y", "%d %b %Y", "%d %B %Y", "%d/%m/%Y"].iter().find_map(|f| NaiveDate::parse_from_str(s.trim(), f).ok())
}

/// What the due-date cutoff demands of a read (RULES.md S5 v2).
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

fn slot_problem(r: &Read, slot: &Slot) -> Option<String> {
    if r.model_id != slot.model || r.max_dim_px != Some(slot.max_dim_px) {
        return Some(format!("slot {} routes {}@{} but read is {}@{:?}", slot.role, slot.model, slot.max_dim_px, r.model_id, r.max_dim_px));
    }
    if let (Some(want), Some(got)) = (slot.max_tokens, r.max_tokens) {
        if want != got {
            return Some(format!("slot {} routes max_tokens {want} but read used {got}", slot.role));
        }
    }
    None
}

/// Recompute (outcome, amount, notes) for one provenance under the routing.
pub fn decide(routing: &Routing, class: &Class, p: &Provenance, e: &Event) -> (&'static str, Option<f64>, Vec<String>) {
    let tol = routing.tolerance;
    let mut notes = Vec::new();
    let reqs: Vec<f64> = p.reads.iter().filter_map(|r| match requirement(r, e) { Cutoff::Required(v) => Some(v), _ => None }).collect();
    if reqs.windows(2).any(|w| !close(w[0], w[1], tol)) {
        notes.push(format!("reads resolve conflicting cutoff requirements {reqs:?}"));
        return ("missing", None, notes);
    }
    let req = reqs.first().copied();
    let counts = |r: &Read, slot: &Slot, notes: &mut Vec<String>| -> Option<f64> {
        let why = slot_problem(r, slot)
            .or_else(|| (!r.reconciled).then(|| "not reconciled".to_string()))
            .or_else(|| r.selected_amount.is_none().then(|| "no selected figure".to_string()))
            .or_else(|| {
                let a = r.selected_amount.unwrap();
                (matches!(e.status.as_str(), "pending" | "scheduled") && a <= 0.0).then(|| format!("{} event figure {a} <= 0", e.status))
            })
            .or_else(|| {
                (requirement(r, e) == Cutoff::NothingValid).then(|| format!("cash date {} is after this read's cutoff but it has no after-cutoff amount: nothing valid", e.cash_date()))
            })
            .or_else(|| {
                let a = r.selected_amount.unwrap();
                req.filter(|q| !close(a, *q, tol)).map(|q| format!("cutoff requires {q} (cash date {}), read selected {a}", e.cash_date()))
            });
        match why {
            Some(w) => {
                notes.push(format!("{} {}: {w}", r.role, r.model_id));
                None
            }
            None => r.selected_amount,
        }
    };
    let mut reader_amounts = Vec::new();
    for slot in &class.readers {
        match p.reads.iter().find(|r| r.role == slot.role && r.model_id == slot.model) {
            None => {
                notes.push(format!("{} read missing", slot.role));
                reader_amounts.push(None);
            }
            Some(r) => reader_amounts.push(counts(r, slot, &mut notes)),
        }
    }
    if reader_amounts.len() == 2 {
        if let (Some(a), Some(b)) = (reader_amounts[0], reader_amounts[1]) {
            if close(a, b, tol) {
                return ("agree", Some(a), notes);
            }
            notes.push("readers disagree".into());
        }
    }
    if let Some(tb) = &class.tiebreak {
        if !class.readers.iter().any(|r| r.model == tb.model) {
            let reader_models: Vec<&str> = class.readers.iter().map(|r| r.model.as_str()).collect();
            if let Some(tr) = p.reads.iter().find(|r| !reader_models.contains(&r.model_id.as_str())) {
                if let Some(f) = counts(tr, tb, &mut notes) {
                    if reader_amounts.iter().flatten().any(|a| close(*a, f, tol)) {
                        return ("tiebreak", Some(f), notes);
                    }
                    notes.push(format!("tiebreak {f} matches no counting reader"));
                }
            }
        }
    }
    ("missing", None, notes)
}

fn outcome_class(recorded: &str) -> &'static str {
    match recorded {
        "agree" => "agree",
        "tiebreak_accept" => "tiebreak",
        _ => "missing",
    }
}

/// Check persisted image EventAmounts and provenance under `code_dir` against the routing table.
pub fn check(code_dir: &Path, dataset_dir: &Path, models_toml: &Path) -> Result<Vec<Finding>> {
    let processed = code_dir.join("store/processed");
    let mut out = Vec::new();

    let routing = match std::fs::read_to_string(models_toml).map_err(anyhow::Error::from).and_then(|t| Routing::from_models_toml(&t)) {
        Ok(r) => r,
        Err(e) => {
            out.push(fail("config", "IA9_no_routing_table", e.to_string()));
            return Ok(out);
        }
    };
    if let Some(r) = &routing {
        out.extend(r.problems.iter().cloned());
    }

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
    let prov_dir = processed.join("image_reads");
    if facts.is_empty() && !prov_dir.exists() {
        return Ok(out); // model path not run: only the table (if any) is checked
    }
    let Some(routing) = routing else {
        out.push(fail("config", "IA9_no_routing_table", format!("{} has no explicit [vlm_routing] but image evidence exists", models_toml.display())));
        return Ok(out);
    };

    let ds = Dataset::load(dataset_dir, &dataset_dir.join("requests.csv"))?;
    let mut link: HashMap<String, String> = HashMap::new();
    let mut rdr = csv::Reader::from_path(dataset_dir.join("images.csv"))?;
    for rec in rdr.records() {
        let rec = rec?;
        link.insert(rec[0].to_string(), rec[3].to_string());
    }

    let mut decided: HashMap<String, (&'static str, Option<f64>, String)> = HashMap::new();
    if let Ok(rd) = std::fs::read_dir(&prov_dir) {
        for entry in rd.flatten() {
            let p: Provenance = match serde_json::from_str(&std::fs::read_to_string(entry.path())?) {
                Ok(p) => p,
                Err(err) => {
                    out.push(fail(&entry.file_name().to_string_lossy(), "IA0_provenance_unreadable", err.to_string()));
                    continue;
                }
            };
            let Some(event_id) = link.get(&p.image_id) else {
                out.push(fail(&p.image_id, "IA2_event_link", "image not in images.csv".into()));
                continue;
            };
            let (ev_event, ev_amount) = p.evidence_event_amount();
            if ev_event.as_ref().is_some_and(|x| x != event_id) {
                out.push(fail(&p.image_id, "IA2_event_link", format!("evidence targets {ev_event:?}, images.csv links {event_id}")));
            }
            let Some(event) = ds.events.get(event_id) else { continue };
            let Some(class) = routing.class_for(event) else {
                out.push(fail(&p.image_id, "IA10_no_class", format!("no routing class for {event_id}")));
                continue;
            };
            if p.class.as_deref() != Some(class.name.as_str()) {
                out.push(fail(&p.image_id, "IA10_class_mismatch", format!("provenance class {:?} but routing table gives {} for {event_id}", p.class, class.name)));
            }
            let reader_models: Vec<&str> = class.readers.iter().map(|r| r.model.as_str()).collect();
            for r in &p.reads {
                let unmapped = unmapped_cutoff_fields(r);
                if !unmapped.is_empty() {
                    out.push(fail(&p.image_id, "IA13_unmapped_cutoff_field", format!("{} read carries cutoff-like fields {unmapped:?} the gate does not map: the due-date rule would be skipped; update the gate for the prompt/schema change", r.model_id)));
                }
            }
            for r in &p.reads {
                let routed = class.readers.iter().find(|s| s.role == r.role && s.model == r.model_id).or(class.tiebreak.as_ref().filter(|t| t.model == r.model_id));
                match routed {
                    None => out.push(fail(&p.image_id, "IA3_reader_not_routed", format!("read {} {}@{:?} is neither a reader nor the tiebreak of class {}", r.role, r.model_id, r.max_dim_px, class.name))),
                    Some(slot) => {
                        if let Some(why) = slot_problem(r, slot) {
                            out.push(fail(&p.image_id, "IA3_reader_not_routed", why));
                        }
                    }
                }
            }
            if p.reads.iter().filter(|r| !reader_models.contains(&r.model_id.as_str())).count() > 1 {
                out.push(fail(&p.image_id, "IA3_reader_not_routed", "more than one non-reader read".into()));
            }
            let (outcome, amount, notes) = decide(&routing, class, &p, event);
            let recorded = outcome_class(&p.outcome);
            if recorded != outcome {
                // Recording an accept the rule does not give is an error; declining one it would
                // give is safe (missing beats wrong) but not the decided routing.
                let sev = if recorded == "missing" { Severity::Warn } else { Severity::Error };
                out.push(Finding { request_id: p.image_id.clone(), severity: sev, code: "IA6_outcome_mismatch", detail: format!("recorded {} but routing gives {outcome}: {}", p.outcome, notes.join("; ")) });
            }
            if recorded != "missing" {
                let same = match (amount, ev_amount) {
                    (Some(a), Some(b)) => close(a, b, routing.tolerance),
                    _ => false,
                };
                if !same {
                    out.push(fail(&p.image_id, "IA5_accepted_amount", format!("evidence {ev_amount:?} but routing accepts {amount:?}")));
                }
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
        let status = ds.events.get(&event_id).map(|e| e.status.as_str()).unwrap_or("");
        if matches!(status, "pending" | "scheduled") && amount.map(|a| a <= 0.0).unwrap_or(true) {
            out.push(fail(&rid, "IA11_nonpositive_cash_moving", format!("{image_id} {status} event applied {amount:?}")));
        }
        if *outcome == "missing" {
            out.push(fail(&rid, "IA4_no_agreement", format!("{image_id} applied although routing accepts nothing: {notes}")));
            continue;
        }
        if let (Some(a), Some(b)) = (amount, accepted) {
            if !close(a, *b, routing.tolerance) {
                out.push(fail(&rid, "IA5_accepted_amount", format!("{image_id} applied {a} but routing accepts {b}")));
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    //! Synthetic amounts only (no image audit figures). Events are real rows so classes derive
    //! from the dataset: event_1786 pending utilities, event_6033 pending groceries, event_6859
    //! scheduled healthcare, event_253 settled income, event_3231 settled dining.
    use super::*;
    use serde_json::json;

    const Q: &str = "Qwen/Qwen3-VL-235B-A22B-Instruct";
    const G: &str = "google/gemma-4-31B-it";
    const C: &str = "claude-opus-5";

    const CONFIG: &str = r#"
[selected]
vlm_primary = "Qwen/Qwen3-VL-235B-A22B-Instruct"
vlm_escalation = "google/gemma-4-31B-it"
vlm_fallback = "claude-opus-5"
image_max_dim_px = 1024

[vlm_routing]
default_class = "settled_expense_receipt"

[[vlm_routing.classes]]
name = "income_payslip"
event_types = ["income"]
readers = [ { role = "vlm_primary", max_dim_px = 1024, max_tokens = 400 }, { role = "vlm_escalation", max_dim_px = 768, max_tokens = 400 } ]

[[vlm_routing.classes]]
name = "pending_bill_due_date"
statuses = ["pending", "scheduled"]
readers = [ { role = "vlm_primary", max_dim_px = 1024, max_tokens = 400 }, { role = "vlm_fallback", max_dim_px = 1024, max_tokens = 1500 } ]

[[vlm_routing.classes]]
name = "settled_expense_receipt"
statuses = ["settled"]
readers = [ { role = "vlm_primary", max_dim_px = 1024, max_tokens = 400 }, { role = "vlm_escalation", max_dim_px = 768, max_tokens = 400 } ]
"#;

    fn read(role: &str, model: &str, px: u32, tokens: u32, ok: bool, amt: Option<f64>) -> Value {
        json!({"role": role, "model_id": model, "model_revision": "x", "max_dim_px": px, "max_tokens": tokens, "reconciled": ok,
               "selected_amount": amt, "currency": "INR", "due_date": null, "before_amount": null, "after_amount": null, "error": null})
    }
    /// Prompt v2 read: drop the v1 cutoff keys so only the v2 names are present.
    fn v2(mut r: Value, due: &str, by: Option<f64>, after: Option<f64>) -> Value {
        let o = r.as_object_mut().unwrap();
        for k in ["due_date", "before_amount", "after_amount"] {
            o.remove(k);
        }
        o.insert("due_cutoff_date".into(), json!(due));
        o.insert("amount_due_by_cutoff".into(), json!(by));
        o.insert("amount_due_after_cutoff".into(), json!(after));
        r
    }
    fn anthropic(mut r: Value) -> Value {
        r["provider"] = json!("anthropic");
        r
    }
    fn cut(mut r: Value, due: &str, before: f64, after: f64) -> Value {
        r["due_date"] = json!(due);
        r["before_amount"] = json!(before);
        r["after_amount"] = json!(after);
        r
    }
    fn resolution(image: &str, event: &str, class: &str, reads: Vec<Value>, outcome: &str, amount: Option<f64>) -> Value {
        let evidence = amount.map(|a| json!({"record_id": format!("{image}#agree:x"), "source": "Image", "observed_at": "2024-01-01T00:00:00",
            "fact": {"EventAmount": {"event_id": event, "amount": (a * 10_000.0).round() as i64, "currency": "INR"}}}));
        json!({"image_id": image, "class": class, "mode": "agreement", "reads": reads, "outcome": outcome, "evidence": evidence})
    }

    fn run(tag: &str, image: &str, prov: Value, config: &str) -> Vec<(&'static str, Severity)> {
        let root = std::env::temp_dir().join(format!("verifier_ia3_{tag}_{}", std::process::id()));
        std::fs::create_dir_all(root.join("store/processed/image_reads")).unwrap();
        std::fs::create_dir_all(root.join("store/processed/evidence")).unwrap();
        let evidence = prov.get("evidence").cloned().filter(|e| !e.is_null()).map(|e| vec![e]).unwrap_or_default();
        std::fs::write(root.join(format!("store/processed/image_reads/{image}.json")), prov.to_string()).unwrap();
        std::fs::write(root.join("store/processed/evidence/request_x.json"), Value::Array(evidence).to_string()).unwrap();
        std::fs::write(root.join("models.toml"), config).unwrap();
        let dataset = Path::new(env!("CARGO_MANIFEST_DIR")).join("../dataset");
        let f = check(&root, &dataset, &root.join("models.toml")).unwrap();
        std::fs::remove_dir_all(&root).ok();
        f.into_iter().map(|x| (x.code, x.severity)).collect()
    }
    fn errors(v: &[(&'static str, Severity)]) -> Vec<&'static str> {
        v.iter().filter(|x| x.1 == Severity::Error).map(|x| x.0).collect()
    }

    #[test]
    fn table_resolves_classes_readers_and_distinct_tiebreaks() {
        let r = Routing::from_models_toml(CONFIG).unwrap().unwrap();
        assert!(r.problems.is_empty(), "{:?}", r.problems);
        let dataset = Path::new(env!("CARGO_MANIFEST_DIR")).join("../dataset");
        let ds = Dataset::load(&dataset, &dataset.join("requests.csv")).unwrap();
        let class = |e: &str| r.class_for(&ds.events[e]).unwrap();
        assert_eq!(class("event_1786").name, "pending_bill_due_date");
        assert_eq!(class("event_6859").name, "pending_bill_due_date");
        assert_eq!(class("event_253").name, "income_payslip");
        assert_eq!(class("event_3231").name, "settled_expense_receipt");
        // Routing v2 derived tiebreaks: gemma@768 for pending bills, claude-opus-5 elsewhere.
        assert_eq!(class("event_1786").tiebreak.as_ref().map(|t| (t.model.as_str(), t.max_dim_px, t.max_tokens)), Some((G, 768, Some(400))));
        assert_eq!(class("event_3231").tiebreak.as_ref().map(|t| (t.model.as_str(), t.max_dim_px, t.max_tokens)), Some((C, 1024, Some(1500))));
        // Explicit tiebreak equal to a reader: IA12.
        let bad = CONFIG.replace(
            "{ role = \"vlm_fallback\", max_dim_px = 1024, max_tokens = 1500 } ]\n\n[[vlm_routing.classes]]\nname = \"settled_expense_receipt\"",
            "{ role = \"vlm_fallback\", max_dim_px = 1024, max_tokens = 1500 } ]\ntiebreak = { role = \"vlm_fallback\", max_dim_px = 1024, max_tokens = 1500 }\n\n[[vlm_routing.classes]]\nname = \"settled_expense_receipt\"",
        );
        let rb = Routing::from_models_toml(&bad).unwrap().unwrap();
        assert!(rb.problems.iter().any(|f| f.code == "IA12_tiebreak_equals_reader"), "{:?}", rb.problems);
    }

    #[test]
    fn agreement_tiebreak_and_routing_violations() {
        // Pending bill: 235B + claude (provider anthropic) agree on the after-cutoff figure.
        let q = cut(read("vlm_primary", Q, 1024, 400, true, Some(150.25)), "2026-02-06", 120.75, 150.25);
        let c = anthropic(read("vlm_fallback", C, 1024, 1500, true, Some(150.25)));
        assert!(errors(&run("a", "image_05", resolution("image_05", "event_1786", "pending_bill_due_date", vec![q, c], "agree", Some(150.25)), CONFIG)).is_empty());

        // Both agree on the before-cutoff figure although the cash date is after due: no accept.
        let q = cut(read("vlm_primary", Q, 1024, 400, true, Some(120.75)), "2026-02-06", 120.75, 150.25);
        let c = anthropic(read("vlm_fallback", C, 1024, 1500, true, Some(120.75)));
        let e = errors(&run("b", "image_05", resolution("image_05", "event_1786", "pending_bill_due_date", vec![q, c], "agree", Some(120.75)), CONFIG));
        assert!(e.contains(&"IA4_no_agreement") && e.contains(&"IA6_outcome_mismatch"), "{e:?}");

        // Readers disagree; gemma tiebreak (distinct, @768) matches 235B's after-cutoff figure: accept.
        let q = cut(read("vlm_primary", Q, 1024, 400, true, Some(150.25)), "2026-02-06", 120.75, 150.25);
        let c = anthropic(read("vlm_fallback", C, 1024, 1500, false, None));
        let g = read("vlm_escalation", G, 768, 400, true, Some(150.25));
        assert!(errors(&run("c", "image_05", resolution("image_05", "event_1786", "pending_bill_due_date", vec![q, c, g], "tiebreak_accept", Some(150.25)), CONFIG)).is_empty());

        // gemma tiebreak at 1024 px (global default instead of its own 768): not routed, no accept.
        let q = cut(read("vlm_primary", Q, 1024, 400, true, Some(150.25)), "2026-02-06", 120.75, 150.25);
        let c = anthropic(read("vlm_fallback", C, 1024, 1500, false, None));
        let g = read("vlm_fallback", G, 1024, 400, true, Some(150.25));
        let e = errors(&run("d", "image_05", resolution("image_05", "event_1786", "pending_bill_due_date", vec![q, c, g], "tiebreak_accept", Some(150.25)), CONFIG));
        assert!(e.contains(&"IA3_reader_not_routed") && e.contains(&"IA4_no_agreement"), "{e:?}");

        // claude at 400 tokens where the pending class routes 1500: not its routed budget.
        let q = read("vlm_primary", Q, 1024, 400, true, Some(5000.5));
        let c = anthropic(read("vlm_fallback", C, 1024, 400, true, Some(5000.5)));
        let e = errors(&run("e", "image_10", resolution("image_10", "event_6033", "pending_bill_due_date", vec![q, c], "agree", Some(5000.5)), CONFIG));
        assert!(e.contains(&"IA3_reader_not_routed") && e.contains(&"IA4_no_agreement"), "{e:?}");

        // Pending figure 0 from both readers: never counts.
        let q = read("vlm_primary", Q, 1024, 400, true, Some(0.0));
        let c = anthropic(read("vlm_fallback", C, 1024, 1500, true, Some(0.0)));
        let e = errors(&run("f", "image_10", resolution("image_10", "event_6033", "pending_bill_due_date", vec![q, c], "agree", Some(0.0)), CONFIG));
        assert!(e.contains(&"IA4_no_agreement") && e.contains(&"IA11_nonpositive_cash_moving"), "{e:?}");

        // Settled receipt: 235B@1024 + gemma@768 agree within tolerance 1.0: accept; evidence amount must match.
        let q = read("vlm_primary", Q, 1024, 400, true, Some(812.40));
        let g = read("vlm_escalation", G, 768, 400, true, Some(812.00));
        assert!(errors(&run("g", "image_07", resolution("image_07", "event_3231", "settled_expense_receipt", vec![q.clone(), g.clone()], "agree", Some(812.40)), CONFIG)).is_empty());
        let e = errors(&run("h", "image_07", resolution("image_07", "event_3231", "settled_expense_receipt", vec![q, g], "agree", Some(700.0)), CONFIG));
        assert!(e.contains(&"IA5_accepted_amount"), "{e:?}");

        // Settled receipt, readers disagree, claude tiebreak matches gemma: accept.
        let q = read("vlm_primary", Q, 1024, 400, true, Some(640.0));
        let g = read("vlm_escalation", G, 768, 400, true, Some(812.40));
        let c = anthropic(read("vlm_fallback", C, 1024, 1500, true, Some(812.40)));
        assert!(errors(&run("h2", "image_07", resolution("image_07", "event_3231", "settled_expense_receipt", vec![q, g, c], "tiebreak_accept", Some(812.40)), CONFIG)).is_empty());

        // Declining an accept the rule would give is safe (warning), never an error.
        let q = read("vlm_primary", Q, 1024, 400, true, Some(812.40));
        let g = read("vlm_escalation", G, 768, 400, true, Some(812.40));
        let v = run("i", "image_07", resolution("image_07", "event_3231", "settled_expense_receipt", vec![q, g], "no_agreement", None), CONFIG);
        assert!(errors(&v).is_empty() && v.contains(&("IA6_outcome_mismatch", Severity::Warn)), "{v:?}");

        // Prompt v2 field names are mapped: both readers pick the by-cutoff figure after the cutoff -> no accept.
        let q = v2(read("vlm_primary", Q, 1024, 400, true, Some(120.75)), "2026-02-06", Some(120.75), Some(150.25));
        let c = anthropic(read("vlm_fallback", C, 1024, 1500, true, Some(120.75)));
        let e = errors(&run("v2a", "image_05", resolution("image_05", "event_1786", "pending_bill_due_date", vec![q, c], "agree", Some(120.75)), CONFIG));
        assert!(e.contains(&"IA4_no_agreement") && !e.contains(&"IA13_unmapped_cutoff_field") && !e.contains(&"IA0_provenance_unreadable"), "{e:?}");
        // ...and on the after-cutoff figure: accept.
        let q = v2(read("vlm_primary", Q, 1024, 400, true, Some(150.25)), "2026-02-06", Some(120.75), Some(150.25));
        let c = anthropic(read("vlm_fallback", C, 1024, 1500, true, Some(150.25)));
        assert!(errors(&run("v2b", "image_05", resolution("image_05", "event_1786", "pending_bill_due_date", vec![q, c], "agree", Some(150.25)), CONFIG)).is_empty());
        // v2: after the cutoff with no after-cutoff amount -> nothing valid, never the by-cutoff figure.
        let q = v2(read("vlm_primary", Q, 1024, 400, true, Some(120.75)), "2026-02-06", Some(120.75), None);
        let c = anthropic(v2(read("vlm_fallback", C, 1024, 1500, true, Some(120.75)), "2026-02-06", Some(120.75), None));
        let e = errors(&run("v2c", "image_05", resolution("image_05", "event_1786", "pending_bill_due_date", vec![q, c], "agree", Some(120.75)), CONFIG));
        assert!(e.contains(&"IA4_no_agreement"), "{e:?}");

        // A renamed cutoff field (e.g. a prompt v2 schema) must not silently disable the cutoff rule.
        let mut q = read("vlm_primary", Q, 1024, 400, true, Some(120.75));
        q["cutoff_date"] = json!("2026-02-06");
        q["amount_payable_after_deadline"] = json!(150.25);
        let c = anthropic(read("vlm_fallback", C, 1024, 1500, true, Some(120.75)));
        let e = errors(&run("r", "image_05", resolution("image_05", "event_1786", "pending_bill_due_date", vec![q, c], "agree", Some(120.75)), CONFIG));
        assert!(e.contains(&"IA13_unmapped_cutoff_field"), "{e:?}");

        // Wrong class recorded.
        let q = read("vlm_primary", Q, 1024, 400, true, Some(812.40));
        let g = read("vlm_escalation", G, 768, 400, true, Some(812.40));
        assert!(errors(&run("j", "image_07", resolution("image_07", "event_3231", "income_payslip", vec![q, g], "agree", Some(812.40)), CONFIG)).contains(&"IA10_class_mismatch"));
    }

    #[test]
    fn missing_table_or_provenance_fails() {
        let q = read("vlm_primary", Q, 1024, 400, true, Some(812.40));
        let g = read("vlm_escalation", G, 768, 400, true, Some(812.40));
        let p = resolution("image_07", "event_3231", "settled_expense_receipt", vec![q, g], "agree", Some(812.40));
        assert!(errors(&run("k", "image_07", p, "[selected]\nvlm_primary = \"x\"\n")).contains(&"IA9_no_routing_table"));
        let root = std::env::temp_dir().join(format!("verifier_ia3_noprov_{}", std::process::id()));
        std::fs::create_dir_all(root.join("store/processed/evidence")).unwrap();
        std::fs::write(root.join("store/processed/evidence/request_73.json"), json!([{"record_id": "image_11#agree:vlm_primary+vlm_fallback", "source": "Image", "observed_at": "2023-01-19T00:00:00", "fact": {"EventAmount": {"event_id": "event_6859", "amount": 25_000_000_i64, "currency": "INR"}}}]).to_string()).unwrap();
        std::fs::write(root.join("models.toml"), CONFIG).unwrap();
        let dataset = Path::new(env!("CARGO_MANIFEST_DIR")).join("../dataset");
        assert!(check(&root, &dataset, &root.join("models.toml")).unwrap().iter().any(|f| f.code == "IA1_no_agreement_provenance"));
        std::fs::remove_dir_all(&root).ok();
    }
}
