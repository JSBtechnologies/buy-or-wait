//! Board `decision.accuracy_first` (user priority): a flagged missing value always beats a wrong
//! one, and 0 false accepts is a hard signoff gate.
//!
//! VALIDATION ONLY. The image audit reference (the analyst's hand reads, RULES.md S5 table) is read
//! at runtime by `verify signoff` from the repository's RULES.md. It is not compiled into the crate,
//! not shipped in code.zip (which archives `code/` only), and never read by the batch run; the
//! hardcode scan fails if any prediction module references it. A gate failure means "model and
//! analyst disagree: investigate" — nothing here corrects an amount.
//!
//! A false accept is a model-derived fact the pipeline applied that is not verifiably right:
//! - FA0 image facts exist but the audit reference is unavailable (cannot verify → fail)
//! - FA1 an image `EventAmount` differing from the audit reference for that image
//! - FA2 an image `EventAmount` for an image or event the reference does not cover
//! - plus, collected by signoff from the other checks: ungrounded model message records (EC5),
//!   facts outside their message family (EC4), image amounts applied without routed agreement
//!   (IA4), a different amount than agreement accepted (IA5), non-positive cash-moving amounts (IA11).
//!
//! Missing values (events left in missing_amounts, messages with no fact) are never false accepts.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::Result;
use serde_json::Value;

use super::contract::{Finding, Severity};
use super::data::{parse_cents, Cents};

#[derive(Debug, Clone, PartialEq)]
pub struct Gold {
    pub event_id: String,
    /// Expected figure first, then any "alt" renderings the analyst marked as equally correct.
    pub amounts: Vec<Cents>,
}

fn figure_at(s: &str) -> Option<Cents> {
    let f: String = s.chars().take_while(|c| c.is_ascii_digit() || *c == ',' || *c == '.').filter(|c| *c != ',').collect();
    parse_cents(f.trim_end_matches('.')).ok()
}

/// `| NN | event_X … | **figure** … (alt figure …) … |` rows of the RULES.md S5 image table.
pub fn gold_from_rules(md: &str) -> BTreeMap<String, Gold> {
    let mut out = BTreeMap::new();
    for line in md.lines() {
        let cells: Vec<&str> = line.split('|').map(str::trim).collect();
        if cells.len() < 4 || cells[1].len() != 2 || !cells[1].bytes().all(|b| b.is_ascii_digit()) || !cells[2].starts_with("event_") {
            continue;
        }
        let event_id: String = cells[2].chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect();
        let Some(start) = cells[3].find("**") else { continue };
        let Some(expected) = figure_at(&cells[3][start + 2..]) else { continue };
        let mut amounts = vec![expected];
        let mut rest = cells[3];
        while let Some(pos) = rest.find("alt ") {
            if let Some(a) = figure_at(&rest[pos + 4..]) {
                if !amounts.contains(&a) {
                    amounts.push(a);
                }
            }
            rest = &rest[pos + 4..];
        }
        out.insert(format!("image_{}", cells[1]), Gold { event_id, amounts });
    }
    out
}

/// The audit reference from `<repo_root>/RULES.md`, if present.
pub fn gold(repo_root: Option<&Path>) -> Option<BTreeMap<String, Gold>> {
    let text = std::fs::read_to_string(repo_root?.join("RULES.md")).ok()?;
    let g = gold_from_rules(&text);
    (!g.is_empty()).then_some(g)
}

/// FA0/FA1/FA2 over every image EventAmount in `code_dir/store/processed/evidence`.
/// Returns (findings, image facts checked).
pub fn image_gold_findings(code_dir: &Path, repo_root: Option<&Path>) -> Result<(Vec<Finding>, usize)> {
    let gold = gold(repo_root);
    let mut out = Vec::new();
    let mut checked = 0;
    let Ok(rd) = std::fs::read_dir(code_dir.join("store/processed/evidence")) else { return Ok((out, 0)) };
    for e in rd.flatten() {
        let rid = e.file_name().to_string_lossy().trim_end_matches(".json").to_string();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(e.path())?)?;
        for rec in v.as_array().into_iter().flatten() {
            let record_id = rec.get("record_id").and_then(Value::as_str).unwrap_or("");
            let Some(body) = rec.get("fact").and_then(|f| f.get("EventAmount")) else { continue };
            if !(record_id.starts_with("image_") || rec.get("source").map(|s| s == "Image").unwrap_or(false)) {
                continue;
            }
            checked += 1;
            let image = record_id.split('#').next().unwrap_or("");
            let event = body.get("event_id").and_then(Value::as_str).unwrap_or("");
            // Money is serialized at engine scale 1e4; compare at the cent.
            let cents = body.get("amount").and_then(Value::as_i64).map(|m| (m as f64 / 100.0).round() as Cents);
            let finding = |code, detail| Finding { request_id: rid.clone(), severity: Severity::Error, code, detail };
            let Some(gold) = &gold else {
                out.push(finding("FA0_audit_reference_unavailable", format!("{image} applied but RULES.md S5 audit table not found: cannot verify")));
                continue;
            };
            match gold.get(image) {
                None => out.push(finding("FA2_unverifiable_image_fact", format!("{image} has no audit reference"))),
                Some(g) if g.event_id != event => out.push(finding("FA2_unverifiable_image_fact", format!("{image} fact targets {event}, reference is for {}", g.event_id))),
                // User rulings name one figure per image (AGENT_RULES §8: image_07 = 8,528, the
                // Grand Total): an analyst "alt" rendering is never accepted as equal.
                Some(g) if cents.is_some_and(|c| g.amounts[1..].contains(&c)) => out.push(finding(
                    "FA3_image_amount_alt",
                    format!(
                        "{image} ({event}) applied the alt rendering {:?}; the ruled figure is {}",
                        cents.map(super::data::fmt_cents_2dp),
                        super::data::fmt_cents_2dp(g.amounts[0])
                    ),
                )),
                Some(g) if cents != Some(g.amounts[0]) => out.push(finding(
                    "FA1_image_amount_wrong",
                    format!(
                        "{image} ({event}) applied {:?}, analyst reference {:?}: model and analyst disagree, investigate (not auto-corrected)",
                        cents.map(super::data::fmt_cents_2dp),
                        g.amounts.iter().map(|c| super::data::fmt_cents_2dp(*c)).collect::<Vec<_>>()
                    ),
                )),
                Some(_) => {}
            }
        }
    }
    Ok((out, checked))
}

/// Ship gate (AGENT_RULES §9: 02/05/10/11 accepted): images in the audit reference whose linked
/// event is pending or scheduled (cash-moving, derived from the dataset, not ids) but have no
/// applied image EventAmount in persisted evidence. A fail-closed row there is safe (reserved),
/// not wrong, but it does not meet the gate.
pub fn cash_moving_unaccepted(code_dir: &Path, repo_root: &Path, dataset_dir: &Path) -> Result<Vec<String>> {
    let Some(gold) = gold(Some(repo_root)) else { anyhow::bail!("RULES.md S5 audit table not found") };
    let ds = super::data::Dataset::load(dataset_dir, &dataset_dir.join("requests.csv"))?;
    let mut applied = std::collections::BTreeSet::new();
    if let Ok(rd) = std::fs::read_dir(code_dir.join("store/processed/evidence")) {
        for e in rd.flatten() {
            let v: Value = serde_json::from_str(&std::fs::read_to_string(e.path())?)?;
            for rec in v.as_array().into_iter().flatten() {
                if rec.get("fact").and_then(|f| f.get("EventAmount")).is_some() {
                    let id = rec.get("record_id").and_then(Value::as_str).unwrap_or("");
                    applied.insert(id.split('#').next().unwrap_or("").to_string());
                }
            }
        }
    }
    Ok(gold
        .iter()
        .filter(|(_, g)| ds.events.get(&g.event_id).is_some_and(|e| matches!(e.status.as_str(), "pending" | "scheduled")))
        .filter(|(image, _)| !applied.contains(*image))
        .map(|(image, g)| format!("{image} ({})", g.event_id))
        .collect())
}

/// Codes from other checks that are false accepts under decision.accuracy_first.
pub const FALSE_ACCEPT_CODES: [&str; 7] = [
    "EC4_unexpected_fact",
    "EC5_model_record_ungrounded",
    "IA4_no_agreement",
    "IA5_accepted_amount",
    "IA11_nonpositive_cash_moving",
    "IA15_accept_without_witness",
    "IA17_final_label_contradiction",
];

#[cfg(test)]
mod tests {
    //! No audit figures are written here: the table is synthetic, and real-data tests take the
    //! reference from the repository's RULES.md at runtime.
    use super::*;
    use serde_json::json;

    fn repo() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("..")
    }

    #[test]
    fn parses_expected_and_alt_figures() {
        let md = "| 03 | event_9 groceries, **pending**, INR | **1,234.50** balance due (alt 1,234.5 \"Total\") | x | y |\n| 04 | event_8 transport, settled, **USD** | **12.25 USD** total | z | w |";
        let g = gold_from_rules(md);
        assert_eq!(g["image_03"], Gold { event_id: "event_9".into(), amounts: vec![1_234_50] });
        assert_eq!(g["image_04"].amounts, vec![12_25]);
        let md2 = "| 05 | event_7 dining | **500** grand total (alt 500.10 \"Total\") | a | b |";
        assert_eq!(gold_from_rules(md2)["image_05"].amounts, vec![500_00, 500_10]);
    }

    #[test]
    fn repository_reference_covers_every_image_and_event_link() {
        let Some(g) = gold(Some(&repo())) else { return };
        assert_eq!(g.len(), 16);
        let dataset = repo().join("dataset/images.csv");
        let mut rdr = csv::Reader::from_path(dataset).unwrap();
        for rec in rdr.records() {
            let rec = rec.unwrap();
            assert_eq!(g[&rec[0]].event_id, &rec[3], "{}", &rec[0]);
        }
    }

    fn with_fact(tag: &str, image: &str, event: &str, cents: Cents, repo_root: Option<&Path>) -> Vec<&'static str> {
        let root = std::env::temp_dir().join(format!("verifier_fa_{tag}_{}", std::process::id()));
        std::fs::create_dir_all(root.join("store/processed/evidence")).unwrap();
        let ev = json!([{"record_id": image, "source": "Image", "observed_at": "2024-01-01T00:00:00",
            "fact": {"EventAmount": {"event_id": event, "amount": cents * 100, "currency": "INR"}}}]);
        std::fs::write(root.join("store/processed/evidence/request_x.json"), ev.to_string()).unwrap();
        let (f, n) = image_gold_findings(&root, repo_root).unwrap();
        assert_eq!(n, 1);
        std::fs::remove_dir_all(&root).ok();
        f.into_iter().map(|x| x.code).collect()
    }

    #[test]
    fn cash_moving_images_must_be_accepted() {
        let Some(g) = gold(Some(&repo())) else { return };
        let dataset = repo().join("dataset");
        let root = std::env::temp_dir().join(format!("verifier_cm_{}", std::process::id()));
        std::fs::create_dir_all(root.join("store/processed/evidence")).unwrap();
        let all = cash_moving_unaccepted(&root, &repo(), &dataset).unwrap();
        assert!(!all.is_empty(), "the audit table has pending/scheduled images");
        // Apply every cash-moving image's figure: nothing is left unaccepted.
        let ev: Vec<Value> = all
            .iter()
            .map(|s| s.split(' ').next().unwrap())
            .map(|image| json!({"record_id": format!("{image}#ocr"), "source": "Image", "fact": {"EventAmount": {"event_id": g[image].event_id, "amount": g[image].amounts[0] * 100, "currency": "INR"}}}))
            .collect();
        std::fs::write(root.join("store/processed/evidence/request_x.json"), Value::Array(ev).to_string()).unwrap();
        assert!(cash_moving_unaccepted(&root, &repo(), &dataset).unwrap().is_empty());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn wrong_unverifiable_or_unreferenced_image_amounts_fail() {
        let Some(g) = gold(Some(&repo())) else { return };
        for (image, ref_) in &g {
            assert!(with_fact(&format!("ok{image}"), image, &ref_.event_id, ref_.amounts[0], Some(&repo())).is_empty(), "{image}");
            for a in &ref_.amounts[1..] {
                assert_eq!(with_fact(&format!("alt{image}{a}"), image, &ref_.event_id, *a, Some(&repo())), vec!["FA3_image_amount_alt"], "{image}");
            }
            // Off by one cent or by a whole unit: model and analyst disagree.
            let off = ref_.amounts[0] + 1;
            assert_eq!(with_fact(&format!("c{image}"), image, &ref_.event_id, off, Some(&repo())), vec!["FA1_image_amount_wrong"]);
            assert_eq!(with_fact(&format!("u{image}"), image, &ref_.event_id, ref_.amounts[0] - 100, Some(&repo())), vec!["FA1_image_amount_wrong"]);
        }
        let (first, r) = g.iter().next().unwrap();
        assert_eq!(with_fact("event", first, "event_0", r.amounts[0], Some(&repo())), vec!["FA2_unverifiable_image_fact"]);
        assert_eq!(with_fact("unknown", "image_99", "event_0", 100, Some(&repo())), vec!["FA2_unverifiable_image_fact"]);
        // No reference available: cannot verify, so the fact fails.
        assert_eq!(with_fact("noref", first, &r.event_id, r.amounts[0], None), vec!["FA0_audit_reference_unavailable"]);
    }
}
