//! Signoff check for board `decision.vlm_setup` (user decision): an image-derived `EventAmount`
//! is accepted only on 2-model agreement.
//!
//! Rule: Qwen3-VL-235B-A22B @1024 px ("primary") and gemma-4-31B-it @768 px ("second") both read
//! every blank-amount image. Accept when both reconcile and select the same amount within the
//! documented tolerance. If they disagree, or one is missing/unreconciled, a Kimi-K3 read
//! ("fallback") breaks the tie and is accepted only if it matches one of the two. Otherwise the
//! event stays in missing_amounts (never guess).
//!
//! Provenance contract (written by the shipped run next to the applied evidence):
//! `store/processed/image_reads/<image_id>.json`
//! ```json
//! { "image_id": "image_10", "event_id": "event_6033", "tolerance": 0.01,
//!   "reads": [ { "role": "primary", "model_id": "Qwen/Qwen3-VL-235B-A22B-Instruct",
//!                "model_revision": "…", "max_dim_px": 1024, "reconciled": true, "selected_amount": 79679.26 },
//!              { "role": "second",  "model_id": "google/gemma-4-31B-it", "max_dim_px": 768, … },
//!              { "role": "fallback", "model_id": "…Kimi-K3…", … } ],
//!   "outcome": "agree" | "fallback_tiebreak" | "missing", "accepted_amount": 79679.26 }
//! ```

use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;
use serde::Deserialize;
use serde_json::Value;

use super::contract::{Finding, Severity};

pub const PRIMARY_MODEL: &str = "Qwen/Qwen3-VL-235B-A22B-Instruct";
pub const PRIMARY_DIM: u32 = 1024;
pub const SECOND_MODEL: &str = "google/gemma-4-31B-it";
pub const SECOND_DIM: u32 = 768;
/// Fallback model id must contain this (case-insensitive); exact id lives in models.toml.
pub const FALLBACK_MODEL_MARK: &str = "kimi-k3";
/// Largest rounding tolerance the provenance may claim (image_07 needs 0.10: 8,528 vs 8,528.10).
pub const MAX_TOLERANCE: f64 = 1.0;

#[derive(Debug, Deserialize)]
pub struct Read {
    pub role: String,
    pub model_id: String,
    #[serde(default)]
    pub model_revision: Option<String>,
    pub max_dim_px: Option<u32>,
    pub reconciled: bool,
    pub selected_amount: Option<f64>,
}

#[derive(Debug, Deserialize)]
pub struct Provenance {
    pub image_id: String,
    pub event_id: String,
    #[serde(default = "default_tol")]
    pub tolerance: f64,
    pub reads: Vec<Read>,
    pub outcome: String,
    pub accepted_amount: Option<f64>,
}

fn default_tol() -> f64 {
    0.01
}

fn fail(id: &str, code: &'static str, detail: String) -> Finding {
    Finding { request_id: id.to_string(), severity: Severity::Error, code, detail }
}

/// The amount a read contributes: only a reconciled read with a selected figure counts.
fn usable(r: Option<&Read>) -> Option<f64> {
    r.filter(|r| r.reconciled).and_then(|r| r.selected_amount)
}

/// What the rule accepts for this provenance: (outcome, amount).
pub fn decide(p: &Provenance) -> (&'static str, Option<f64>) {
    let tol = p.tolerance;
    let role = |name: &str, model_ok: &dyn Fn(&Read) -> bool| p.reads.iter().find(|r| r.role == name && model_ok(r));
    let primary = usable(role("primary", &|r| r.model_id == PRIMARY_MODEL && r.max_dim_px == Some(PRIMARY_DIM)));
    let second = usable(role("second", &|r| r.model_id == SECOND_MODEL && r.max_dim_px == Some(SECOND_DIM)));
    let fallback = usable(role("fallback", &|r| r.model_id.to_lowercase().contains(FALLBACK_MODEL_MARK)));
    let close = |a: f64, b: f64| (a - b).abs() <= tol + 1e-9;
    if let (Some(a), Some(b)) = (primary, second) {
        if close(a, b) {
            return ("agree", Some(a));
        }
    }
    if let Some(f) = fallback {
        for x in [primary, second].into_iter().flatten() {
            if close(f, x) {
                return ("fallback_tiebreak", Some(f));
            }
        }
    }
    ("missing", None)
}

/// Check every persisted image `EventAmount` (and every provenance file) under `code_dir/store/processed`.
pub fn check(code_dir: &Path, images_csv: &Path) -> Result<Vec<Finding>> {
    let processed = code_dir.join("store/processed");
    let mut out = Vec::new();
    let mut link: HashMap<String, String> = HashMap::new();
    let mut rdr = csv::Reader::from_path(images_csv)?;
    for rec in rdr.records() {
        let rec = rec?;
        link.insert(rec[0].to_string(), rec[3].to_string());
    }

    let mut provenance: HashMap<String, Provenance> = HashMap::new();
    if let Ok(rd) = std::fs::read_dir(processed.join("image_reads")) {
        for e in rd.flatten() {
            let text = std::fs::read_to_string(e.path())?;
            match serde_json::from_str::<Provenance>(&text) {
                Ok(p) => {
                    if p.tolerance < 0.0 || p.tolerance > MAX_TOLERANCE {
                        out.push(fail(&p.image_id, "IA8_tolerance_out_of_range", format!("tolerance {} not in [0, {MAX_TOLERANCE}]", p.tolerance)));
                    }
                    if link.get(&p.image_id) != Some(&p.event_id) {
                        out.push(fail(&p.image_id, "IA2_event_link", format!("provenance event {} but images.csv links {:?}", p.event_id, link.get(&p.image_id))));
                    }
                    let (outcome, amount) = decide(&p);
                    if outcome != p.outcome {
                        out.push(fail(&p.image_id, "IA6_outcome_mismatch", format!("recorded outcome {} but reads give {outcome}", p.outcome)));
                    }
                    let amounts_match = match (amount, p.accepted_amount) {
                        (Some(a), Some(b)) => (a - b).abs() <= p.tolerance + 1e-9,
                        (None, None) => true,
                        _ => false,
                    };
                    if !amounts_match {
                        out.push(fail(&p.image_id, "IA5_accepted_amount", format!("recorded {:?} but rule accepts {amount:?}", p.accepted_amount)));
                    }
                    provenance.insert(p.image_id.clone(), p);
                }
                Err(err) => out.push(fail(&e.file_name().to_string_lossy(), "IA0_provenance_unreadable", err.to_string())),
            }
        }
    }

    if let Ok(rd) = std::fs::read_dir(processed.join("evidence")) {
        for e in rd.flatten() {
            let rid = e.file_name().to_string_lossy().trim_end_matches(".json").to_string();
            let v: Value = serde_json::from_str(&std::fs::read_to_string(e.path())?)?;
            for rec in v.as_array().into_iter().flatten() {
                let record_id = rec.get("record_id").and_then(Value::as_str).unwrap_or("");
                let Some(body) = rec.get("fact").and_then(|f| f.get("EventAmount")) else { continue };
                let is_image = record_id.starts_with("image_") || rec.get("source").map(|s| s == "Image").unwrap_or(false);
                if !is_image {
                    continue;
                }
                let image_id = record_id.split('#').next().unwrap_or("");
                let event_id = body.get("event_id").and_then(Value::as_str).unwrap_or("");
                // Money serializes at engine scale (1e4).
                let amount = body.get("amount").and_then(Value::as_i64).map(|m| m as f64 / crate::engine::money::SCALE as f64);
                let Some(p) = provenance.get(image_id) else {
                    out.push(fail(&rid, "IA1_no_agreement_provenance", format!("{image_id} EventAmount for {event_id} has no store/processed/image_reads/{image_id}.json")));
                    continue;
                };
                if p.event_id != event_id {
                    out.push(fail(&rid, "IA2_event_link", format!("{image_id} fact event {event_id} vs provenance {}", p.event_id)));
                }
                match decide(p) {
                    ("missing", _) => out.push(fail(&rid, "IA4_no_agreement", format!("{image_id} applied although reads do not satisfy 2-model agreement or fallback tiebreak: {:?}", p.reads.iter().map(|r| (&r.role, &r.model_id, r.max_dim_px, r.reconciled, r.selected_amount)).collect::<Vec<_>>()))),
                    (_, Some(accepted)) => {
                        if amount.map(|a| (a - accepted).abs() > p.tolerance + 1e-9).unwrap_or(true) {
                            out.push(fail(&rid, "IA5_accepted_amount", format!("{image_id} applied {amount:?} but agreement accepts {accepted}")));
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn read(role: &str, model: &str, dim: u32, ok: bool, amt: Option<f64>) -> Value {
        json!({"role": role, "model_id": model, "model_revision": "x", "max_dim_px": dim, "reconciled": ok, "selected_amount": amt})
    }

    fn setup(tag: &str, prov: Value, amount: f64) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("verifier_ia_{tag}_{}", std::process::id()));
        std::fs::create_dir_all(root.join("store/processed/image_reads")).unwrap();
        std::fs::create_dir_all(root.join("store/processed/evidence")).unwrap();
        std::fs::write(root.join("store/processed/image_reads/image_10.json"), prov.to_string()).unwrap();
        let ev = json!([{"record_id": "image_10", "source": "Image", "observed_at": "2024-06-03T00:00:00",
            "fact": {"EventAmount": {"event_id": "event_6033", "amount": (amount * 10_000.0).round() as i64, "currency": "INR"}}}]);
        std::fs::write(root.join("store/processed/evidence/request_64.json"), ev.to_string()).unwrap();
        root
    }

    fn codes(tag: &str, prov: Value, amount: f64) -> Vec<&'static str> {
        let root = setup(tag, prov, amount);
        let images = Path::new(env!("CARGO_MANIFEST_DIR")).join("../dataset/images.csv");
        let f = check(&root, &images).unwrap();
        std::fs::remove_dir_all(&root).ok();
        f.into_iter().map(|x| x.code).collect()
    }

    const K: &str = "moonshotai/Kimi-K3-Instruct";

    #[test]
    fn agreement_rules() {
        let base = |reads: Vec<Value>, outcome: &str, acc: Option<f64>| json!({"image_id": "image_10", "event_id": "event_6033", "tolerance": 0.01, "reads": reads, "outcome": outcome, "accepted_amount": acc});
        // Both primary models agree: pass.
        let ok = base(vec![read("primary", PRIMARY_MODEL, 1024, true, Some(79679.26)), read("second", SECOND_MODEL, 768, true, Some(79679.26))], "agree", Some(79679.26));
        assert!(codes("ok", ok, 79679.26).is_empty());
        // Only one read (the current first-success cascade): fail.
        let single = base(vec![read("primary", PRIMARY_MODEL, 1024, true, Some(79679.26))], "agree", Some(79679.26));
        let c = codes("single", single, 79679.26);
        assert!(c.contains(&"IA4_no_agreement") && c.contains(&"IA6_outcome_mismatch"), "{c:?}");
        // Disagreement, fallback matches the second model: tiebreak pass.
        let tie = base(vec![read("primary", PRIMARY_MODEL, 1024, true, Some(72045.0)), read("second", SECOND_MODEL, 768, true, Some(79679.26)), read("fallback", K, 1024, true, Some(79679.26))], "fallback_tiebreak", Some(79679.26));
        assert!(codes("tie", tie, 79679.26).is_empty());
        // Disagreement, fallback matches neither: must be missing, so an applied amount fails.
        let none = base(vec![read("primary", PRIMARY_MODEL, 1024, true, Some(72045.0)), read("second", SECOND_MODEL, 768, true, Some(79679.26)), read("fallback", K, 1024, true, Some(1513.13))], "missing", None);
        assert!(codes("none", none, 79679.26).contains(&"IA4_no_agreement"));
        // Wrong resolution for gemma (1024 instead of 768): its read does not count.
        let dim = base(vec![read("primary", PRIMARY_MODEL, 1024, true, Some(79679.26)), read("second", SECOND_MODEL, 1024, true, Some(79679.26))], "agree", Some(79679.26));
        assert!(codes("dim", dim, 79679.26).contains(&"IA4_no_agreement"));
        // Dropped 30B model standing in for gemma: fail.
        let m30 = base(vec![read("primary", PRIMARY_MODEL, 1024, true, Some(79679.26)), read("second", "Qwen/Qwen3-VL-30B-A3B-Instruct", 768, true, Some(79679.26))], "agree", Some(79679.26));
        assert!(codes("m30", m30, 79679.26).contains(&"IA4_no_agreement"));
        // Agreement on one amount but a different amount applied: fail.
        let ok2 = base(vec![read("primary", PRIMARY_MODEL, 1024, true, Some(79679.26)), read("second", SECOND_MODEL, 768, true, Some(79679.26))], "agree", Some(79679.26));
        assert!(codes("amt", ok2, 72045.0).contains(&"IA5_accepted_amount"));
        // An unreconciled read never counts toward agreement.
        let unrec = base(vec![read("primary", PRIMARY_MODEL, 1024, false, Some(79679.26)), read("second", SECOND_MODEL, 768, true, Some(79679.26))], "agree", Some(79679.26));
        assert!(codes("unrec", unrec, 79679.26).contains(&"IA4_no_agreement"));
    }

    #[test]
    fn image_amount_without_provenance_fails_and_tolerance_is_bounded() {
        let root = std::env::temp_dir().join(format!("verifier_ia_noprov_{}", std::process::id()));
        std::fs::create_dir_all(root.join("store/processed/evidence")).unwrap();
        let ev = json!([{"record_id": "image_11", "source": "Image", "observed_at": "2023-01-19T00:00:00",
            "fact": {"EventAmount": {"event_id": "event_6859", "amount": 36_500_000_i64, "currency": "INR"}}}]);
        std::fs::write(root.join("store/processed/evidence/request_73.json"), ev.to_string()).unwrap();
        let images = Path::new(env!("CARGO_MANIFEST_DIR")).join("../dataset/images.csv");
        assert!(check(&root, &images).unwrap().iter().any(|f| f.code == "IA1_no_agreement_provenance"));
        std::fs::remove_dir_all(&root).ok();

        let wide = json!({"image_id": "image_10", "event_id": "event_6033", "tolerance": 5000.0,
            "reads": [read("primary", PRIMARY_MODEL, 1024, true, Some(79679.26)), read("second", SECOND_MODEL, 768, true, Some(76000.0))],
            "outcome": "agree", "accepted_amount": 79679.26});
        assert!(codes("wide", wide, 79679.26).contains(&"IA8_tolerance_out_of_range"));
    }
}
