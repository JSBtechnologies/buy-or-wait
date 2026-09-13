//! Board `decision.accuracy_first` (user priority): a flagged missing value always beats a wrong
//! one, and 0 false accepts is a hard signoff gate.
//!
//! A false accept is a model-derived fact the pipeline applied that is not verifiably right:
//! - FA1 an image `EventAmount` whose amount differs from the hand-read audit gold for that image
//!   (RULES.md S5 table, analyst 9b123f6; pinned copy below, the table wins when present)
//! - FA2 an image `EventAmount` for an image or event the gold does not cover (unverifiable)
//! - plus, collected by signoff from the other checks: ungrounded model message records (EC5),
//!   facts outside their message family (EC4), image amounts applied without routed agreement
//!   (IA4), a different amount than agreement accepted (IA5), non-positive cash-moving amounts (IA11).
//!
//! Missing values (events left in missing_amounts, messages with no fact) are never false accepts.
//! The gold is audit reference only: it is read here, never in the prediction path (hardcode scan).

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::Result;
use serde_json::Value;

use super::contract::{Finding, Severity};
use super::data::{parse_cents, Cents};

/// (image_id, event_id, accepted amounts in cents in the event's currency). First is the expected
/// figure; later entries are equally correct renderings noted by the analyst (image_07 8,528.10).
pub const PINNED_GOLD: [(&str, &str, &[Cents]); 16] = [
    ("image_01", "event_253", &[4_365_000_00]),
    ("image_02", "event_1442", &[100_000_00]),
    ("image_03", "event_1545", &[41_272_00]),
    ("image_04", "event_1700", &[2_854_00]),
    ("image_05", "event_1786", &[822_05]),
    ("image_06", "event_3051", &[1_995_00]),
    ("image_07", "event_3231", &[8_528_00, 8_528_10]),
    ("image_08", "event_4535", &[15_339_00]),
    ("image_09", "event_5170", &[723_00]),
    ("image_10", "event_6033", &[79_679_26]),
    ("image_11", "event_6859", &[3_650_00]),
    ("image_12", "event_7307", &[33_50]),
    ("image_13", "event_7941", &[2_298_00]),
    ("image_14", "event_9421", &[4_543_00]),
    ("image_15", "event_9806", &[9_968_00]),
    ("image_16", "event_10521", &[393_22]),
];

#[derive(Debug, Clone, PartialEq)]
pub struct Gold {
    pub event_id: String,
    pub amounts: Vec<Cents>,
}

/// Gold per image: pinned, overridden by the RULES.md S5 table rows when present.
pub fn gold(repo_root: Option<&Path>) -> BTreeMap<String, Gold> {
    let mut map: BTreeMap<String, Gold> = PINNED_GOLD
        .iter()
        .map(|(i, e, a)| (i.to_string(), Gold { event_id: e.to_string(), amounts: a.to_vec() }))
        .collect();
    if let Some(text) = repo_root.and_then(|r| std::fs::read_to_string(r.join("RULES.md")).ok()) {
        for (image, g) in gold_from_rules(&text) {
            let entry = map.entry(image).or_insert_with(|| g.clone());
            if entry.event_id == g.event_id {
                // Keep pinned alternates; the table's expected figure leads.
                let mut amounts = g.amounts.clone();
                amounts.extend(entry.amounts.iter().filter(|a| !g.amounts.contains(a)));
                entry.amounts = amounts;
            } else {
                *entry = g;
            }
        }
    }
    map
}

/// `| NN | event_X … | **figure** … |` rows of the S5 image table.
pub fn gold_from_rules(md: &str) -> Vec<(String, Gold)> {
    let mut out = Vec::new();
    for line in md.lines() {
        let cells: Vec<&str> = line.split('|').map(str::trim).collect();
        if cells.len() < 4 || cells[1].len() != 2 || !cells[1].bytes().all(|b| b.is_ascii_digit()) || !cells[2].starts_with("event_") {
            continue;
        }
        let event_id: String = cells[2].chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect();
        let Some(start) = cells[3].find("**") else { continue };
        let rest = &cells[3][start + 2..];
        let Some(end) = rest.find("**") else { continue };
        let figure: String = rest[..end].chars().take_while(|c| c.is_ascii_digit() || *c == ',' || *c == '.').filter(|c| *c != ',').collect();
        if let Ok(c) = parse_cents(&figure) {
            out.push((format!("image_{}", cells[1]), Gold { event_id, amounts: vec![c] }));
        }
    }
    out
}

/// FA1/FA2 over every image EventAmount in `code_dir/store/processed/evidence`.
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
            match gold.get(image) {
                None => out.push(Finding { request_id: rid.clone(), severity: Severity::Error, code: "FA2_unverifiable_image_fact", detail: format!("{image} has no audit gold") }),
                Some(g) if g.event_id != event => out.push(Finding { request_id: rid.clone(), severity: Severity::Error, code: "FA2_unverifiable_image_fact", detail: format!("{image} fact targets {event}, gold is for {}", g.event_id) }),
                Some(g) => {
                    if !cents.map(|c| g.amounts.contains(&c)).unwrap_or(false) {
                        out.push(Finding {
                            request_id: rid.clone(),
                            severity: Severity::Error,
                            code: "FA1_image_amount_wrong",
                            detail: format!("{image} ({event}) applied {:?} but audit gold is {:?}", cents.map(super::data::fmt_cents_2dp), g.amounts.iter().map(|c| super::data::fmt_cents_2dp(*c)).collect::<Vec<_>>()),
                        });
                    }
                }
            }
        }
    }
    Ok((out, checked))
}

/// Codes from other checks that are false accepts under decision.accuracy_first.
pub const FALSE_ACCEPT_CODES: [&str; 5] = ["EC4_unexpected_fact", "EC5_model_record_ungrounded", "IA4_no_agreement", "IA5_accepted_amount", "IA11_nonpositive_cash_moving"];

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn gold_table_parses_and_matches_pinned() {
        let md = "| 05 | event_1786 utilities, **pending**, INR, settles 2026-02-09 | **822.05** amount due after 06-Feb-2026 | x | cash |\n| 12 | event_7307 transport, settled, **USD** | **33.50 USD** total | y | z |";
        let g = gold_from_rules(md);
        assert_eq!(g[0], ("image_05".to_string(), Gold { event_id: "event_1786".into(), amounts: vec![822_05] }));
        assert_eq!(g[1].1.amounts, vec![33_50]);
        let out = std::process::Command::new("git").args(["show", "9b123f6:RULES.md"]).current_dir(env!("CARGO_MANIFEST_DIR")).output();
        if let Some(o) = out.ok().filter(|o| o.status.success()) {
            let parsed = gold_from_rules(&String::from_utf8_lossy(&o.stdout));
            assert_eq!(parsed.len(), 16);
            for (image, g) in parsed {
                let pinned = PINNED_GOLD.iter().find(|p| p.0 == image).unwrap();
                assert_eq!(g.event_id, pinned.1, "{image}");
                assert_eq!(g.amounts[0], pinned.2[0], "{image}");
            }
        }
    }

    fn with_fact(tag: &str, image: &str, event: &str, amount: f64) -> Vec<&'static str> {
        let root = std::env::temp_dir().join(format!("verifier_fa_{tag}_{}", std::process::id()));
        std::fs::create_dir_all(root.join("store/processed/evidence")).unwrap();
        let ev = json!([{"record_id": image, "source": "Image", "observed_at": "2024-01-01T00:00:00",
            "fact": {"EventAmount": {"event_id": event, "amount": (amount * 10_000.0).round() as i64, "currency": "INR"}}}]);
        std::fs::write(root.join("store/processed/evidence/request_x.json"), ev.to_string()).unwrap();
        let (f, n) = image_gold_findings(&root, None).unwrap();
        assert_eq!(n, 1);
        std::fs::remove_dir_all(&root).ok();
        f.into_iter().map(|x| x.code).collect()
    }

    #[test]
    fn wrong_or_unverifiable_image_amounts_are_false_accepts() {
        assert!(with_fact("ok", "image_10", "event_6033", 79679.26).is_empty());
        assert!(with_fact("alt", "image_07", "event_3231", 8528.10).is_empty());
        assert_eq!(with_fact("trap", "image_05", "event_1786", 704.05), vec!["FA1_image_amount_wrong"]);
        assert_eq!(with_fact("subtotal", "image_10", "event_6033", 72045.0), vec!["FA1_image_amount_wrong"]);
        assert_eq!(with_fact("cash", "image_12", "event_7307", 40.0), vec!["FA1_image_amount_wrong"]);
        assert_eq!(with_fact("breakup", "image_11", "event_6859", 3150.0), vec!["FA1_image_amount_wrong"]);
        assert_eq!(with_fact("event", "image_10", "event_1786", 79679.26), vec!["FA2_unverifiable_image_fact"]);
        assert_eq!(with_fact("unknown", "image_99", "event_1", 1.0), vec!["FA2_unverifiable_image_fact"]);
    }
}
