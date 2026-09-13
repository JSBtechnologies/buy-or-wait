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
//! within tolerance AND on the parsed due cutoff (both absent counts as equal; analyst #317) →
//! agree; else a counting tiebreak read agrees with a counting reader on amount and cutoff →
//! tiebreak; else nothing is accepted (missing, never a guess).
//!
//! Witness gate (image_accuracy_plan.md §2, extraction 12488dd; OCR per
//! fleet/specs/ocr_vllm_pipeline.md A4, names confirmed extract#29): provenance with `mode`
//! `witness` or `ocr` is re-derived by `decide_witness` and needs no routing table. Outcomes
//! `witness_accept` | `no_agreement` | `no_route` | `fail_closed`; any other string is naming
//! drift (IA6). Codes: IA14 Claude dependency in `[selected]`/`[ocr]` or a claude read selecting a
//! figure (RULES.md S8); IA15 an accepted figure no supporting read witnesses with a known
//! `WitnessKind`; IA17 an accepted figure a supporting read reports a final-label contradiction
//! for. A non-summing breakdown is never a contradiction (extraction never reports one), so a
//! declined but witnessed figure shows as an IA6 warning, never a pass.

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
    /// OCR reads (`role = "ocr"`, mode `ocr`, ml-engineer extract#29) may omit it.
    #[serde(default)]
    pub model_id: String,
    #[serde(default)]
    pub model_revision: Option<String>,
    #[serde(default)]
    pub provider: Option<String>,
    pub max_dim_px: Option<u32>,
    pub max_tokens: Option<u32>,
    #[serde(default)]
    pub reconciled: bool,
    pub selected_amount: Option<f64>,
    /// Witness gate (extraction 12488dd `ImageReadProvenance`): the `WitnessKind::label` that
    /// proved `selected_amount` on this read's own figures.
    #[serde(default)]
    pub witness: Option<String>,
    /// The witness identity's own result (image_07: 8,528.10 proving a selected 8,528).
    #[serde(default)]
    pub witness_computed: Option<f64>,
    /// First final-labeled field disagreeing with `selected_amount` (`"field=value"`).
    #[serde(default)]
    pub contradiction: Option<String>,
    /// v1 `due_date`; prompt v2 (RULES.md S5, analyst 40edcb5) `due_cutoff_date`.
    #[serde(default, alias = "due_cutoff_date")]
    pub due_date: Option<String>,
    /// v1 `before_amount`; v2 `amount_due_by_cutoff`.
    #[serde(default, alias = "amount_due_by_cutoff")]
    pub before_amount: Option<f64>,
    /// v1 `after_amount`; v2 `amount_due_after_cutoff`.
    #[serde(default, alias = "amount_due_after_cutoff")]
    pub after_amount: Option<f64>,
    /// main.rs (26fe32f) persists the cutoff as one nested object
    /// `{due_date, before_amount, after_amount}` (null when the read resolved none); mapped onto
    /// the flat fields above by `Provenance::normalize`.
    #[serde(default)]
    pub cutoff: Option<CutoffObj>,
    /// Every other field of the read, kept so a renamed cutoff field cannot silently switch
    /// the due-date rule off (IA13).
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

/// Read fields the gate knows that are not cutoff inputs.
const KNOWN_NON_CUTOFF_FIELDS: [&str; 3] = ["currency", "error", "prompt_version"];

/// Provenance modes decided by the witness gate, which needs no routing table:
/// `witness` (HF-only VLM pairs, image_accuracy_plan.md §2) and `ocr` (one deterministic
/// Unlimited-OCR reader, fleet/specs/ocr_vllm_pipeline.md A4).
pub const WITNESS_MODES: [&str; 2] = ["witness", "ocr"];
/// `extract::witness::WitnessKind::label` values (12488dd; 75e4e18 adds the two
/// ruling.total_or_witnessed_sum kinds: a line-item sum accepted only with an independent words or
/// printed-label witness; 14eb075 adds `printed_final_label_only`, not an identity but the user's
/// `ruling.lone_printed_total`: a printed final-labeled total is accepted alone, never a computed
/// sum or a bare-subtotal duplicate; it never carries `witness_computed`). Any other name proves
/// nothing.
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
/// Recorded witness-gate outcomes: the accept, and every decline extraction ships.
const WITNESS_ACCEPT: &str = "witness_accept";
const WITNESS_DECLINES: [&str; 3] = ["no_agreement", "no_route", "fail_closed"];

/// RULES.md S8: no Claude dependency (Anthropic org cap until 2026-10-01).
fn is_anthropic(model: &str, provider: Option<&str>) -> bool {
    provider.is_some_and(|p| p.eq_ignore_ascii_case("anthropic")) || model.to_lowercase().contains("claude")
}

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
pub struct CutoffObj {
    #[serde(default, alias = "due_cutoff_date")]
    pub due_date: Option<String>,
    #[serde(default, alias = "amount_due_by_cutoff")]
    pub before_amount: Option<f64>,
    #[serde(default, alias = "amount_due_after_cutoff")]
    pub after_amount: Option<f64>,
}

#[derive(Debug, Deserialize)]
pub struct Provenance {
    pub image_id: String,
    pub class: Option<String>,
    #[serde(default)]
    pub mode: Option<String>,
    pub reads: Vec<Read>,
    pub outcome: String,
    #[serde(default)]
    pub evidence: Option<Value>,
    /// main.rs (26fe32f) shape: the applied figure and its event instead of an `evidence` record.
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

/// Parsed due cutoff for agreement: absent (missing, empty, "null") = Ok(None); a raw value that
/// does not parse is kept as its own key so it never equals an absent or parsed cutoff.
fn cutoff_key(r: &Read) -> Result<Option<NaiveDate>, String> {
    match r.due_date.as_deref().map(str::trim) {
        None | Some("") => Ok(None),
        Some(s) if s.eq_ignore_ascii_case("null") => Ok(None),
        Some(s) => parse_day(s).map(Some).ok_or_else(|| s.to_string()),
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
    // Counted reads must agree on the amount AND on the parsed due cutoff (analyst #317: an
    // invented cutoff plus one coincident error must escalate, not accept).
    let mut reader_amounts: Vec<Option<(f64, &Read)>> = Vec::new();
    for slot in &class.readers {
        match p.reads.iter().find(|r| r.role == slot.role && r.model_id == slot.model) {
            None => {
                notes.push(format!("{} read missing", slot.role));
                reader_amounts.push(None);
            }
            Some(r) => reader_amounts.push(counts(r, slot, &mut notes).map(|a| (a, r))),
        }
    }
    if reader_amounts.len() == 2 {
        if let (Some((a, ra)), Some((b, rb))) = (reader_amounts[0], reader_amounts[1]) {
            if !close(a, b, tol) {
                notes.push("readers disagree".into());
            } else if cutoff_key(ra) != cutoff_key(rb) {
                notes.push(format!("readers agree on {a} but not on the due cutoff ({:?} vs {:?})", cutoff_key(ra), cutoff_key(rb)));
            } else {
                return ("agree", Some(a), notes);
            }
        }
    }
    if let Some(tb) = &class.tiebreak {
        if !class.readers.iter().any(|r| r.model == tb.model) {
            let reader_models: Vec<&str> = class.readers.iter().map(|r| r.model.as_str()).collect();
            if let Some(tr) = p.reads.iter().find(|r| !reader_models.contains(&r.model_id.as_str())) {
                if let Some(f) = counts(tr, tb, &mut notes) {
                    if reader_amounts.iter().flatten().any(|(a, ra)| close(*a, f, tol) && cutoff_key(ra) == cutoff_key(tr)) {
                        return ("tiebreak", Some(f), notes);
                    }
                    notes.push(format!("tiebreak {f} (cutoff {:?}) matches no counting reader on amount and due cutoff", cutoff_key(tr)));
                }
            }
        }
    }
    ("missing", None, notes)
}

/// Recompute (outcome, amount, notes) for one witness-gate provenance (`mode` in
/// `WITNESS_MODES`), mirroring extraction's `find_witnessed_pair` (12488dd): a read counts when
/// it selected a figure (> 0 for pending/scheduled), meets any cutoff requirement, carries no
/// final-label contradiction, names only a known witness kind whose computed result proves the
/// figure, and is not an Anthropic model. `witness` mode accepts the first pair of counted reads
/// agreeing on amount and parsed cutoff with a witness on either side; `ocr` mode (one
/// deterministic reader) accepts a single counted read with its own witness.
pub fn decide_witness(p: &Provenance, e: &Event, tol: f64) -> (&'static str, Option<f64>, Vec<String>) {
    let mut notes = Vec::new();
    let reqs: Vec<f64> = p.reads.iter().filter_map(|r| match requirement(r, e) { Cutoff::Required(v) => Some(v), _ => None }).collect();
    if reqs.windows(2).any(|w| !close(w[0], w[1], tol)) {
        notes.push(format!("reads resolve conflicting cutoff requirements {reqs:?}"));
        return ("missing", None, notes);
    }
    let req = reqs.first().copied();
    let ocr = p.mode.as_deref() == Some("ocr");
    let mut counted: Vec<(f64, &Read)> = Vec::new();
    for r in &p.reads {
        let Some(a) = r.selected_amount else { continue };
        let provider = r.provider.as_deref();
        let why = is_anthropic(&r.model_id, provider)
            .then(|| "Anthropic model read (RULES.md S8: no Claude dependency)".to_string())
            .or_else(|| (!ocr && !r.reconciled).then(|| "not reconciled".to_string()))
            .or_else(|| (matches!(e.status.as_str(), "pending" | "scheduled") && a <= 0.0).then(|| format!("{} event figure {a} <= 0", e.status)))
            .or_else(|| (requirement(r, e) == Cutoff::NothingValid).then(|| format!("cash date {} is after the cutoff but no after-cutoff amount", e.cash_date())))
            .or_else(|| req.filter(|q| !close(a, *q, tol)).map(|q| format!("cutoff requires {q} (cash date {}), read selected {a}", e.cash_date())))
            .or_else(|| r.contradiction.as_ref().map(|c| format!("final label contradicts: {c}")))
            .or_else(|| r.witness.as_ref().filter(|w| !WITNESS_KINDS.contains(&w.as_str())).map(|w| format!("unknown witness kind {w:?}")))
            .or_else(|| r.witness_computed.filter(|c| r.witness.is_none() || !close(*c, a, tol)).map(|c| format!("witness_computed {c} does not prove {a}")))
            .or_else(|| (r.witness.as_deref() == Some("printed_final_label_only") && r.witness_computed.is_some()).then(|| "printed_final_label_only on a computed figure (ruling.lone_printed_total covers printed totals only)".to_string()));
        match why {
            Some(w) => notes.push(format!("{} {}@{:?}: {w}", r.role, r.model_id, r.max_dim_px)),
            None => counted.push((a, r)),
        }
    }
    if ocr {
        if let Some((a, _)) = counted.iter().find(|(_, r)| r.witness.is_some()) {
            return ("accept", Some(*a), notes);
        }
        notes.push("no counted OCR read carries a witness".into());
        return ("missing", None, notes);
    }
    for i in 0..counted.len() {
        for j in (i + 1)..counted.len() {
            let ((a, ra), (b, rb)) = (counted[i], counted[j]);
            if close(a, b, tol) && cutoff_key(ra) == cutoff_key(rb) && (ra.witness.is_some() || rb.witness.is_some()) {
                if ra.model_id == rb.model_id {
                    notes.push(format!("accepted pair is one model ({}): no cross-model agreement", ra.model_id));
                }
                return ("accept", Some(a), notes);
            }
        }
    }
    notes.push("no pair of counted reads agrees on amount and cutoff with a witness".into());
    ("missing", None, notes)
}

/// RULES.md S8 (no Claude dependency; supersedes decision.vlm_routing_v3's claude tiebreak):
/// any `[selected]` model or `[ocr]` model that is an Anthropic model is an error.
pub fn claude_dependency_problems(models_toml: &str) -> Vec<Finding> {
    let Ok(v) = toml::from_str::<toml::Value>(models_toml) else { return Vec::new() };
    let mut out = Vec::new();
    for table in ["selected", "ocr"] {
        for (k, val) in v.get(table).and_then(|t| t.as_table()).into_iter().flatten() {
            if let Some(s) = val.as_str().filter(|s| is_anthropic(s, None)) {
                out.push(fail("config", "IA14_claude_dependency", format!("[{table}].{k} = {s:?}: RULES.md S8 forbids a Claude dependency")));
            }
        }
    }
    out
}

fn outcome_class(recorded: &str) -> &'static str {
    match recorded {
        "agree" => "agree",
        "tiebreak_accept" => "tiebreak",
        WITNESS_ACCEPT => "accept",
        _ => "missing",
    }
}

/// Findings for one witness-gate provenance; records the decided outcome for the facts pass.
fn witness_findings(p: &Provenance, event: &Event, ev_amount: Option<f64>, tol: f64, decided: &mut HashMap<String, (&'static str, Option<f64>, String)>) -> Vec<Finding> {
    let mut out = Vec::new();
    let id = p.image_id.as_str();
    let (outcome, amount, notes) = decide_witness(p, event, tol);
    let known = p.outcome == WITNESS_ACCEPT || WITNESS_DECLINES.contains(&p.outcome.as_str());
    let recorded = outcome_class(&p.outcome);
    if !known {
        let sev = if ev_amount.is_some() { Severity::Error } else { Severity::Warn };
        out.push(Finding { request_id: id.to_string(), severity: sev, code: "IA6_outcome_mismatch", detail: format!("unknown witness-gate outcome {:?}: align the verifier with extraction", p.outcome) });
    } else if recorded != outcome {
        let sev = if recorded == "missing" { Severity::Warn } else { Severity::Error };
        out.push(Finding { request_id: id.to_string(), severity: sev, code: "IA6_outcome_mismatch", detail: format!("recorded {} but the witness gate gives {outcome}: {}", p.outcome, notes.join("; ")) });
    }
    if recorded == "accept" {
        // Every accepted figure names its witness, and no read supporting it is contradicted.
        let supporting: Vec<&Read> = p.reads.iter().filter(|r| matches!((r.selected_amount, ev_amount), (Some(a), Some(b)) if close(a, b, tol))).collect();
        if !supporting.iter().any(|r| r.witness.as_deref().is_some_and(|w| WITNESS_KINDS.contains(&w))) {
            out.push(fail(id, "IA15_accept_without_witness", format!("accepted {ev_amount:?} but no read selecting it names a known witness kind")));
        }
        if let Some(r) = supporting.iter().find(|r| r.contradiction.is_some()) {
            out.push(fail(id, "IA17_final_label_contradiction", format!("accepted {ev_amount:?} but {} {} reports {}", r.role, r.model_id, r.contradiction.as_deref().unwrap_or(""))));
        }
        if !matches!((amount, ev_amount), (Some(a), Some(b)) if close(a, b, tol)) {
            out.push(fail(id, "IA5_accepted_amount", format!("evidence {ev_amount:?} but the witness gate accepts {amount:?}")));
        }
    }
    let outcome = if outcome == "accept" { "agree" } else { outcome };
    decided.insert(p.image_id.clone(), (outcome, amount, notes.join("; ")));
    out
}

/// Check persisted image EventAmounts and provenance under `code_dir` against the routing table
/// (routed modes) or the witness gate (`WITNESS_MODES`).
pub fn check(code_dir: &Path, dataset_dir: &Path, models_toml: &Path) -> Result<Vec<Finding>> {
    let processed = code_dir.join("store/processed");
    let mut out = Vec::new();

    let toml_text = std::fs::read_to_string(models_toml).unwrap_or_default();
    out.extend(claude_dependency_problems(&toml_text));
    let routing = Routing::from_models_toml(&toml_text);

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
    let mut provs: Vec<Provenance> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&prov_dir) {
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
    let witness_mode = |p: &Provenance| p.mode.as_deref().is_some_and(|m| WITNESS_MODES.contains(&m));
    // The routing table is only needed for provenance the witness gate does not decide.
    let needs_routing = provs.iter().any(|p| !witness_mode(p));
    let routing = match routing {
        Ok(r) => r,
        Err(e) if needs_routing => {
            out.push(fail("config", "IA9_no_routing_table", e.to_string()));
            return Ok(out);
        }
        Err(_) => None,
    };
    if let Some(r) = routing.as_ref().filter(|_| needs_routing) {
        out.extend(r.problems.iter().cloned());
    }
    if facts.is_empty() && provs.is_empty() {
        return Ok(out); // model path not run: only the config is checked
    }
    if needs_routing && routing.is_none() {
        out.push(fail("config", "IA9_no_routing_table", format!("{} has no explicit [vlm_routing] but routed image provenance exists", models_toml.display())));
        return Ok(out);
    }
    let tolerance = routing.as_ref().map(|r| r.tolerance).unwrap_or(DEFAULT_TOLERANCE);

    let ds = Dataset::load(dataset_dir, &dataset_dir.join("requests.csv"))?;
    let mut link: HashMap<String, String> = HashMap::new();
    let mut rdr = csv::Reader::from_path(dataset_dir.join("images.csv"))?;
    for rec in rdr.records() {
        let rec = rec?;
        link.insert(rec[0].to_string(), rec[3].to_string());
    }

    let mut decided: HashMap<String, (&'static str, Option<f64>, String)> = HashMap::new();
    {
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
                    out.push(fail(&p.image_id, "IA13_unmapped_cutoff_field", format!("{} read carries cutoff-like fields {unmapped:?} the gate does not map: the due-date rule would be skipped; update the gate for the prompt/schema change", r.model_id)));
                }
                if r.selected_amount.is_some() && is_anthropic(&r.model_id, r.provider.as_deref()) {
                    out.push(fail(&p.image_id, "IA14_claude_dependency", format!("read {} {} selected a figure: RULES.md S8 forbids a Claude dependency", r.role, r.model_id)));
                }
            }
            if witness_mode(p) {
                out.extend(witness_findings(p, event, ev_amount, tolerance, &mut decided));
                continue;
            }
            let routing = routing.as_ref().expect("needs_routing checked above");
            let Some(class) = routing.class_for(event) else {
                out.push(fail(&p.image_id, "IA10_no_class", format!("no routing class for {event_id}")));
                continue;
            };
            if p.class.as_deref() != Some(class.name.as_str()) {
                out.push(fail(&p.image_id, "IA10_class_mismatch", format!("provenance class {:?} but routing table gives {} for {event_id}", p.class, class.name)));
            }
            let reader_models: Vec<&str> = class.readers.iter().map(|r| r.model.as_str()).collect();
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
            let (outcome, amount, notes) = decide(routing, class, p, event);
            let recorded = outcome_class(&p.outcome);
            if recorded != outcome {
                // Recording an accept the rule does not give is an error; declining one it would
                // give is safe (missing beats wrong) but not the decided routing.
                let sev = if recorded == "missing" { Severity::Warn } else { Severity::Error };
                out.push(Finding { request_id: p.image_id.clone(), severity: sev, code: "IA6_outcome_mismatch", detail: format!("recorded {} but routing gives {outcome}: {}", p.outcome, notes.join("; ")) });
            }
            if recorded != "missing" {
                let same = match (amount, ev_amount) {
                    (Some(a), Some(b)) => close(a, b, tolerance),
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
            if !close(a, *b, tolerance) {
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
    /// Legacy routed tiebreak fixture: an HF model (RULES.md S8 makes a claude read IA14).
    const C: &str = "moonshotai/Kimi-K3";

    const CONFIG_V2: &str = r#"
[selected]
vlm_primary = "Qwen/Qwen3-VL-235B-A22B-Instruct"
vlm_escalation = "google/gemma-4-31B-it"
vlm_fallback = "moonshotai/Kimi-K3"
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

    /// Routing v3 (decision.vlm_routing_v3), shaped like config/models.toml.
    const CONFIG: &str = r#"
[selected]
vlm_primary = "Qwen/Qwen3-VL-235B-A22B-Instruct"
vlm_escalation = "google/gemma-4-31B-it"
vlm_fallback = "moonshotai/Kimi-K3"
image_max_dim_px = 1024

[vlm_routing]
default_class = "settled_expense_receipt"
tolerance = 0.01

[[vlm_routing.classes]]
name = "income_payslip"
event_types = ["income"]
statuses = []
categories = []
readers = [ { role = "vlm_primary", max_dim_px = 1024, max_tokens = 400 }, { role = "vlm_escalation", max_dim_px = 768, max_tokens = 400 } ]
tiebreak = { role = "vlm_fallback", max_dim_px = 1024, max_tokens = 4000 }

[[vlm_routing.classes]]
name = "pending_bill_due_date"
event_types = []
statuses = ["pending", "scheduled"]
categories = []
readers = [ { role = "vlm_primary", max_dim_px = 1024, max_tokens = 400 }, { role = "vlm_escalation", max_dim_px = 768, max_tokens = 400 } ]
tiebreak = { role = "vlm_fallback", max_dim_px = 1024, max_tokens = 4000 }

[[vlm_routing.classes]]
name = "settled_expense_receipt"
event_types = []
statuses = ["settled"]
categories = []
readers = [ { role = "vlm_primary", max_dim_px = 1024, max_tokens = 400 }, { role = "vlm_escalation", max_dim_px = 768, max_tokens = 400 } ]
tiebreak = { role = "vlm_fallback", max_dim_px = 1024, max_tokens = 4000 }
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
        r["provider"] = json!("novita");
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
        let r = Routing::from_models_toml(CONFIG_V2).unwrap().unwrap();
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
        let bad = CONFIG_V2.replace(
            "{ role = \"vlm_fallback\", max_dim_px = 1024, max_tokens = 1500 } ]\n\n[[vlm_routing.classes]]\nname = \"settled_expense_receipt\"",
            "{ role = \"vlm_fallback\", max_dim_px = 1024, max_tokens = 1500 } ]\ntiebreak = { role = \"vlm_fallback\", max_dim_px = 1024, max_tokens = 1500 }\n\n[[vlm_routing.classes]]\nname = \"settled_expense_receipt\"",
        );
        let rb = Routing::from_models_toml(&bad).unwrap().unwrap();
        assert!(rb.problems.iter().any(|f| f.code == "IA12_tiebreak_equals_reader"), "{:?}", rb.problems);

        // Routing v3 shape: every class reads 235B@1024 + gemma@768, tiebreak fallback@1024 only.
        let v3 = Routing::from_models_toml(CONFIG).unwrap().unwrap();
        assert!(v3.problems.is_empty() && claude_dependency_problems(CONFIG).is_empty(), "{:?}", v3.problems);
        assert_eq!(v3.tolerance, 0.01);
        for e in ["event_1786", "event_253", "event_3231"] {
            let c = v3.class_for(&ds.events[e]).unwrap();
            assert_eq!(c.readers.iter().map(|s| (s.model.as_str(), s.max_dim_px)).collect::<Vec<_>>(), vec![(Q, 1024), (G, 768)], "{e}");
            assert_eq!(c.tiebreak.as_ref().map(|t| (t.model.as_str(), t.max_dim_px, t.max_tokens)), Some((C, 1024, Some(4000))), "{e}");
        }
    }

    #[test]
    fn agreement_tiebreak_and_routing_violations() {
        // Routing v3 slots: readers 235B@1024/400 + gemma@768/400, tiebreak claude@1024/4000.
        let q = |amt: Option<f64>| read("vlm_primary", Q, 1024, 400, true, amt);
        let g = |amt: Option<f64>| read("vlm_escalation", G, 768, 400, true, amt);
        let c = |amt: Option<f64>| anthropic(read("vlm_fallback", C, 1024, 4000, true, amt));
        let pending = |tag: &str, reads: Vec<Value>, outcome: &str, amount: Option<f64>| errors(&run(tag, "image_05", resolution("image_05", "event_1786", "pending_bill_due_date", reads, outcome, amount), CONFIG));
        let settled = |tag: &str, reads: Vec<Value>, outcome: &str, amount: Option<f64>| errors(&run(tag, "image_07", resolution("image_07", "event_3231", "settled_expense_receipt", reads, outcome, amount), CONFIG));

        // Pending bill: 235B + gemma agree on the after-cutoff figure and the cutoff (formats differ, parsed equal).
        let e = pending("a", vec![cut(q(Some(150.25)), "2026-02-06", 120.75, 150.25), cut(g(Some(150.25)), "06-Feb-2026", 120.75, 150.25)], "agree", Some(150.25));
        assert!(e.is_empty(), "{e:?}");

        // Both agree on the before-cutoff figure although the cash date is after due: no accept.
        let e = pending("b", vec![cut(q(Some(120.75)), "2026-02-06", 120.75, 150.25), cut(g(Some(120.75)), "2026-02-06", 120.75, 150.25)], "agree", Some(120.75));
        assert!(e.contains(&"IA4_no_agreement") && e.contains(&"IA6_outcome_mismatch"), "{e:?}");

        // gemma failed; claude tiebreak (provider anthropic) matches 235B on amount and cutoff: accept.
        let e = pending("c", vec![cut(q(Some(150.25)), "2026-02-06", 120.75, 150.25), read("vlm_escalation", G, 768, 400, false, None), cut(c(Some(150.25)), "2026-02-06", 120.75, 150.25)], "tiebreak_accept", Some(150.25));
        assert!(e.is_empty(), "{e:?}");

        // Anthropic unavailable (usage cap): claude read errors -> flagged missing, never an accept.
        let mut capped = c(None);
        capped["reconciled"] = json!(false);
        capped["error"] = json!("usage cap");
        let e = pending("c2", vec![cut(q(Some(150.25)), "2026-02-06", 120.75, 150.25), read("vlm_escalation", G, 768, 400, false, None), capped.clone()], "tiebreak_accept", Some(150.25));
        assert!(e.contains(&"IA4_no_agreement"), "{e:?}");
        let v = run("c3", "image_05", resolution("image_05", "event_1786", "pending_bill_due_date", vec![cut(q(Some(150.25)), "2026-02-06", 120.75, 150.25), read("vlm_escalation", G, 768, 400, false, None), capped], "no_agreement", None), CONFIG);
        assert!(errors(&v).is_empty(), "{v:?}");

        // claude tiebreak at 1500 tokens where v3 routes 4000: not routed, no accept.
        let mut cb = cut(c(Some(150.25)), "2026-02-06", 120.75, 150.25);
        cb["max_tokens"] = json!(1500);
        let e = pending("d", vec![cut(q(Some(150.25)), "2026-02-06", 120.75, 150.25), read("vlm_escalation", G, 768, 400, false, None), cb], "tiebreak_accept", Some(150.25));
        assert!(e.contains(&"IA3_reader_not_routed") && e.contains(&"IA4_no_agreement"), "{e:?}");

        // gemma reader at 1024 px where v3 routes 768: not its routed resolution.
        let e = errors(&run("e", "image_10", resolution("image_10", "event_6033", "pending_bill_due_date", vec![q(Some(5000.5)), read("vlm_escalation", G, 1024, 400, true, Some(5000.5))], "agree", Some(5000.5)), CONFIG));
        assert!(e.contains(&"IA3_reader_not_routed") && e.contains(&"IA4_no_agreement"), "{e:?}");

        // v2 habit: 235B + claude recorded as a reader pair "agree". Under v3 claude only tiebreaks
        // (gemma missing), so the decided outcome is a tiebreak, not an agreement.
        let e = errors(&run("e2", "image_10", resolution("image_10", "event_6033", "pending_bill_due_date", vec![q(Some(5000.5)), c(Some(5000.5))], "agree", Some(5000.5)), CONFIG));
        assert!(e.contains(&"IA6_outcome_mismatch") && !e.contains(&"IA4_no_agreement"), "{e:?}");

        // Pending figure 0 from both readers: never counts.
        let e = errors(&run("f", "image_10", resolution("image_10", "event_6033", "pending_bill_due_date", vec![q(Some(0.0)), g(Some(0.0))], "agree", Some(0.0)), CONFIG));
        assert!(e.contains(&"IA4_no_agreement") && e.contains(&"IA11_nonpositive_cash_moving"), "{e:?}");

        // Settled receipt: readers agree; evidence amount must match what agreement accepts.
        assert!(settled("g", vec![q(Some(812.40)), g(Some(812.40))], "agree", Some(812.40)).is_empty());
        let e = settled("h", vec![q(Some(812.40)), g(Some(812.40))], "agree", Some(700.0));
        assert!(e.contains(&"IA5_accepted_amount"), "{e:?}");
        // v3 tolerance 0.01: a 0.40 difference is a disagreement.
        let e = settled("g2", vec![q(Some(812.40)), g(Some(812.00))], "agree", Some(812.40));
        assert!(e.contains(&"IA4_no_agreement"), "{e:?}");

        // Settled receipt, readers disagree, claude tiebreak matches gemma: accept.
        assert!(settled("h2", vec![q(Some(640.0)), g(Some(812.40)), c(Some(812.40))], "tiebreak_accept", Some(812.40)).is_empty());

        // Declining an accept the rule would give is safe (warning), never an error.
        let v = run("i", "image_07", resolution("image_07", "event_3231", "settled_expense_receipt", vec![q(Some(812.40)), g(Some(812.40))], "no_agreement", None), CONFIG);
        assert!(errors(&v).is_empty() && v.contains(&("IA6_outcome_mismatch", Severity::Warn)), "{v:?}");

        // Prompt v2 field names are mapped: both readers pick the by-cutoff figure after the cutoff -> no accept.
        let e = pending("v2a", vec![v2(q(Some(120.75)), "2026-02-06", Some(120.75), Some(150.25)), v2(g(Some(120.75)), "2026-02-06", Some(120.75), Some(150.25))], "agree", Some(120.75));
        assert!(e.contains(&"IA4_no_agreement") && !e.contains(&"IA13_unmapped_cutoff_field") && !e.contains(&"IA0_provenance_unreadable"), "{e:?}");
        // ...and on the after-cutoff figure: accept.
        let e = pending("v2b", vec![v2(q(Some(150.25)), "2026-02-06", Some(120.75), Some(150.25)), v2(g(Some(150.25)), "2026-02-06", Some(120.75), Some(150.25))], "agree", Some(150.25));
        assert!(e.is_empty(), "{e:?}");
        // v2: after the cutoff with no after-cutoff amount -> nothing valid, never the by-cutoff figure.
        let e = pending("v2c", vec![v2(q(Some(120.75)), "2026-02-06", Some(120.75), None), v2(g(Some(120.75)), "2026-02-06", Some(120.75), None)], "agree", Some(120.75));
        assert!(e.contains(&"IA4_no_agreement"), "{e:?}");

        // A renamed cutoff field (e.g. a prompt schema change) must not silently disable the cutoff rule.
        let mut qr = q(Some(120.75));
        qr["cutoff_date"] = json!("2026-02-06");
        qr["amount_payable_after_deadline"] = json!(150.25);
        let e = pending("r", vec![qr, g(Some(120.75))], "agree", Some(120.75));
        assert!(e.contains(&"IA13_unmapped_cutoff_field"), "{e:?}");

        // Analyst #317 / v3: counted reads must also agree on the parsed due cutoff.
        // Same amount, different cutoff dates -> no accept.
        let e = pending("dc1", vec![cut(q(Some(150.25)), "2026-02-06", 120.75, 150.25), cut(g(Some(150.25)), "2026-02-07", 120.75, 150.25)], "agree", Some(150.25));
        assert!(e.contains(&"IA4_no_agreement") && e.contains(&"IA6_outcome_mismatch"), "{e:?}");
        // Invented cutoff: one reader adds a cutoff the other read does not have; same figure -> no accept.
        let inv = cut(q(Some(900.0)), "2023-01-01", 800.0, 900.0);
        let e = errors(&run("dc2", "image_10", resolution("image_10", "event_6033", "pending_bill_due_date", vec![inv.clone(), g(Some(900.0))], "agree", Some(900.0)), CONFIG));
        assert!(e.contains(&"IA4_no_agreement"), "{e:?}");
        // ...a claude tiebreak (no cutoff, same figure) sides with the non-inventing reader: legitimate accept.
        assert!(errors(&run("dc3", "image_10", resolution("image_10", "event_6033", "pending_bill_due_date", vec![inv.clone(), g(Some(900.0)), c(Some(900.0))], "tiebreak_accept", Some(900.0)), CONFIG)).is_empty());
        // Tiebreak without cutoff matching only the inventing reader (gemma failed): no accept.
        let e = errors(&run("dc4", "image_10", resolution("image_10", "event_6033", "pending_bill_due_date", vec![inv, read("vlm_escalation", G, 768, 400, false, None), c(Some(900.0))], "tiebreak_accept", Some(900.0)), CONFIG));
        assert!(e.contains(&"IA4_no_agreement"), "{e:?}");
        // An unparseable cutoff never equals an absent one.
        let mut qx = q(Some(812.40));
        qx["due_date"] = json!("CHARGED ON");
        let e = settled("dc5", vec![qx, g(Some(812.40))], "agree", Some(812.40));
        assert!(e.contains(&"IA4_no_agreement"), "{e:?}");

        // Wrong class recorded.
        let e = errors(&run("j", "image_07", resolution("image_07", "event_3231", "income_payslip", vec![q(Some(812.40)), g(Some(812.40))], "agree", Some(812.40)), CONFIG));
        assert!(e.contains(&"IA10_class_mismatch"), "{e:?}");

        // RULES.md S8: a claude model in [selected] is a Claude dependency.
        let claude_cfg = CONFIG.replace("vlm_fallback = \"moonshotai/Kimi-K3\"", "vlm_fallback = \"claude-opus-5\"");
        let v = run("cfg", "image_07", resolution("image_07", "event_3231", "settled_expense_receipt", vec![q(Some(812.40)), g(Some(812.40))], "agree", Some(812.40)), &claude_cfg);
        assert!(errors(&v).contains(&"IA14_claude_dependency"), "{v:?}");
    }

    /// Witness gate (12488dd) and OCR mode: dev-only fixtures shaped like the audited pages
    /// (RULES.md S5 rulings: 05 = 822.05 after the 06-Feb cutoff, 07 = 8,528, 11 = 3,650).
    #[test]
    fn witness_gate_traps() {
        let w = |mut r: Value, kind: Option<&str>, computed: Option<f64>| {
            r["witness"] = json!(kind);
            r["witness_computed"] = json!(computed);
            r["contradiction"] = Value::Null;
            r
        };
        let wres = |image: &str, event: &str, mode: &str, reads: Vec<Value>, outcome: &str, amount: Option<f64>| {
            let mut v = resolution(image, event, "x", reads, outcome, amount);
            v["mode"] = json!(mode);
            v["class"] = Value::Null;
            v
        };
        // No [vlm_routing] at all: the witness gate needs none (no IA9).
        const HF: &str = "[selected]\nvlm_primary = \"Qwen/Qwen3-VL-235B-A22B-Instruct\"\nvlm_escalation = \"google/gemma-4-31B-it\"\n";
        let q = |amt: f64| read("vlm_primary", Q, 1024, 1400, true, Some(amt));
        let g = |amt: f64| read("vlm_escalation", G, 768, 1400, true, Some(amt));
        let img05 = |tag: &str, reads: Vec<Value>, outcome: &str, amount: Option<f64>| errors(&run(tag, "image_05", wres("image_05", "event_1786", "witness", reads, outcome, amount), HF));

        // image_05: both reads agree on the pre-cutoff 704.05 (witnessed by words) but the event
        // settles 2026-02-09, after the 06-Feb cutoff: never accepted.
        let e = img05("w05a", vec![w(cut(q(704.05), "06-Feb-2026", 704.05, 822.05), Some("amount_in_words"), Some(704.05)), w(cut(g(704.05), "2026-02-06", 704.05, 822.05), None, None)], "witness_accept", Some(704.05));
        assert!(e.contains(&"IA4_no_agreement") && e.contains(&"IA6_outcome_mismatch"), "{e:?}");
        // ...822.05 with the same cutoff and the late-fee witness: accepted, clean.
        let e = img05("w05b", vec![w(cut(q(822.05), "06-Feb-2026", 704.05, 822.05), Some("cutoff_after_exceeds_witnessed_before"), None), w(cut(g(822.05), "2026-02-06", 704.05, 822.05), None, None)], "witness_accept", Some(822.05));
        assert!(e.is_empty(), "{e:?}");
        // ...agreeing figures but no witness on either read: IA15 + no accept.
        let e = img05("w05c", vec![w(cut(q(822.05), "06-Feb-2026", 704.05, 822.05), None, None), w(cut(g(822.05), "2026-02-06", 704.05, 822.05), None, None)], "witness_accept", Some(822.05));
        assert!(e.contains(&"IA15_accept_without_witness") && e.contains(&"IA4_no_agreement"), "{e:?}");

        // image_11: summary witness proves 3,650; a non-summing breakup is not a contradiction.
        let img11 = |tag: &str, reads: Vec<Value>, outcome: &str, amount: Option<f64>| run(tag, "image_11", wres("image_11", "event_6859", "witness", reads, outcome, amount), HF);
        assert!(errors(&img11("w11a", vec![w(q(3650.0), Some("line_item_sum"), Some(3650.0)), w(g(3650.0), Some("repeated_final_label"), None)], "witness_accept", Some(3650.0))).is_empty());
        // Declining it is safe (warn) but visible.
        let v = img11("w11b", vec![w(q(3650.0), Some("line_item_sum"), Some(3650.0)), w(g(3650.0), Some("repeated_final_label"), None)], "no_agreement", None);
        assert!(errors(&v).is_empty() && v.contains(&("IA6_outcome_mismatch", Severity::Warn)), "{v:?}");
        // A contradicted read accepted anyway: IA17.
        let mut qc = w(q(3150.0), Some("line_item_sum"), Some(3150.0));
        qc["contradiction"] = json!("amount_due=3650");
        let e = errors(&img11("w11c", vec![qc, w(g(3150.0), None, None)], "witness_accept", Some(3150.0)));
        assert!(e.contains(&"IA17_final_label_contradiction") && e.contains(&"IA4_no_agreement"), "{e:?}");

        // image_07: Grand Total 8,528 proven by the plain Total 8,528.10 (rounds): accepted.
        let img07 = |tag: &str, reads: Vec<Value>, outcome: &str, amount: Option<f64>| errors(&run(tag, "image_07", wres("image_07", "event_3231", "witness", reads, outcome, amount), HF));
        assert!(img07("w07a", vec![w(q(8528.0), Some("subtotal_plus_tax"), Some(8528.10)), w(g(8528.0), None, None)], "witness_accept", Some(8528.0)).is_empty());
        // An invented witness kind or a computed result that does not prove the figure: no accept.
        let e = img07("w07b", vec![w(q(8528.0), Some("vibes"), Some(8528.0)), w(g(8528.0), None, None)], "witness_accept", Some(8528.0));
        assert!(e.contains(&"IA15_accept_without_witness") && e.contains(&"IA4_no_agreement"), "{e:?}");
        let e = img07("w07c", vec![w(q(8528.0), Some("subtotal_plus_tax"), Some(8122.0)), w(g(8528.0), None, None)], "witness_accept", Some(8528.0));
        assert!(e.contains(&"IA4_no_agreement"), "{e:?}");
        // A claude read in the pair: Claude dependency, and it never counts.
        let e = img07("w07d", vec![w(q(8528.0), Some("subtotal_plus_tax"), Some(8528.10)), w(read("vlm_fallback", "claude-opus-5", 1024, 4000, true, Some(8528.0)), None, None)], "witness_accept", Some(8528.0));
        assert!(e.contains(&"IA14_claude_dependency") && e.contains(&"IA4_no_agreement"), "{e:?}");
        // Unknown outcome string with applied evidence: naming drift is an error.
        let e = img07("w07e", vec![w(q(8528.0), Some("subtotal_plus_tax"), Some(8528.10)), w(g(8528.0), None, None)], "accepted_v4", Some(8528.0));
        assert!(e.contains(&"IA6_outcome_mismatch"), "{e:?}");

        // OCR mode: one deterministic reader with its own witness is enough; without one, nothing.
        let o = |amt: f64| read("ocr", "baidu/Unlimited-OCR", 0, 8192, false, Some(amt));
        let ocr = |tag: &str, reads: Vec<Value>, outcome: &str, amount: Option<f64>| errors(&run(tag, "image_02", wres("image_02", "event_1442", "ocr", reads, outcome, amount), HF));
        assert!(ocr("o02a", vec![w(o(100000.0), Some("total_minus_paid"), Some(100000.0))], "witness_accept", Some(100000.0)).is_empty());
        let e = ocr("o02b", vec![w(o(200000.0), None, None)], "witness_accept", Some(200000.0));
        assert!(e.contains(&"IA15_accept_without_witness") && e.contains(&"IA4_no_agreement"), "{e:?}");
        assert!(ocr("o02c", vec![w(o(200000.0), None, None)], "fail_closed", None).is_empty());
        // 75e4e18 kinds (ruling.total_or_witnessed_sum): a sum witnessed by words or a label counts.
        assert!(ocr("o02d", vec![w(o(100000.0), Some("line_item_sum_witnessed_by_words"), Some(100000.0))], "witness_accept", Some(100000.0)).is_empty());
        assert!(ocr("o02e", vec![w(o(100000.0), Some("line_item_sum_witnessed_by_label"), None)], "witness_accept", Some(100000.0)).is_empty());
        // 14eb075 ruling.lone_printed_total: a printed total alone is accepted; never with a computed result.
        assert!(ocr("o02f", vec![w(o(100000.0), Some("printed_final_label_only"), None)], "witness_accept", Some(100000.0)).is_empty());
        let e = ocr("o02g", vec![w(o(100000.0), Some("printed_final_label_only"), Some(100000.0))], "witness_accept", Some(100000.0));
        assert!(e.contains(&"IA4_no_agreement") && e.contains(&"IA6_outcome_mismatch"), "{e:?}");

        // The shape main.rs persists (26fe32f): nested `cutoff`, top-level accepted_amount/event_id,
        // no evidence record. image_05 at 704.05 after the cutoff is still caught; 822.05 passes.
        let persisted = |tag: &str, amount: f64| {
            let prov = json!({"image_id": "image_05", "event_id": "event_1786", "class": "", "mode": "ocr", "outcome": "witness_accept", "accepted_amount": amount,
                "reads": [{"role": "ocr", "model_id": "baidu/Unlimited-OCR", "model_revision": "", "max_dim_px": 0, "reconciled": true, "selected_amount": amount,
                    "cutoff": {"due_date": "06-Feb-2026", "before_amount": 704.05, "after_amount": 822.05}, "witness": "cutoff_after_exceeds_witnessed_before", "ocr_notes": []}]});
            errors(&run(tag, "image_05", prov, HF))
        };
        assert!(persisted("p05ok", 822.05).is_empty(), "{:?}", persisted("p05ok2", 822.05));
        let e = persisted("p05bad", 704.05);
        assert!(e.contains(&"IA6_outcome_mismatch") && !e.contains(&"IA13_unmapped_cutoff_field"), "{e:?}");
    }

    #[test]
    fn missing_table_or_provenance_fails() {
        let q = read("vlm_primary", Q, 1024, 400, true, Some(812.40));
        let g = read("vlm_escalation", G, 768, 400, true, Some(812.40));
        let p = resolution("image_07", "event_3231", "settled_expense_receipt", vec![q, g], "agree", Some(812.40));
        assert!(errors(&run("k", "image_07", p, "[selected]\nvlm_primary = \"x\"\n")).contains(&"IA9_no_routing_table"));
        let root = std::env::temp_dir().join(format!("verifier_ia3_noprov_{}", std::process::id()));
        std::fs::create_dir_all(root.join("store/processed/evidence")).unwrap();
        std::fs::write(root.join("store/processed/evidence/request_73.json"), json!([{"record_id": "image_11#agree:vlm_primary+vlm_escalation", "source": "Image", "observed_at": "2023-01-19T00:00:00", "fact": {"EventAmount": {"event_id": "event_6859", "amount": 25_000_000_i64, "currency": "INR"}}}]).to_string()).unwrap();
        std::fs::write(root.join("models.toml"), CONFIG).unwrap();
        let dataset = Path::new(env!("CARGO_MANIFEST_DIR")).join("../dataset");
        assert!(check(&root, &dataset, &root.join("models.toml")).unwrap().iter().any(|f| f.code == "IA1_no_agreement_provenance"));
        std::fs::remove_dir_all(&root).ok();
    }
}
