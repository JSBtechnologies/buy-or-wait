//! Ship-time sign-off: everything that must hold for the submitted `output.csv` and
//! `evaluation/usage_report.md`, in one pass/fail report.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};

use super::contract::{self, Severity};
use super::data::Dataset;

#[derive(Debug, Default)]
pub struct Signoff {
    pub checks: Vec<(String, bool, String)>,
    pub contract: Option<contract::Report>,
}

impl Signoff {
    fn check(&mut self, name: &str, ok: bool, detail: impl Into<String>) {
        self.checks.push((name.to_string(), ok, detail.into()));
    }

    pub fn passed(&self) -> bool {
        self.checks.iter().all(|(_, ok, _)| *ok)
    }

    pub fn render(&self) -> String {
        let mut out = String::new();
        if let Some(rep) = &self.contract {
            for f in rep.findings.iter().filter(|f| f.severity == Severity::Error).take(40) {
                out.push_str(&format!("{f}\n"));
            }
        }
        for (name, ok, detail) in &self.checks {
            out.push_str(&format!("[{}] {name}: {detail}\n", if *ok { "PASS" } else { "FAIL" }));
        }
        out.push_str(&format!("SIGN-OFF: {}\n", if self.passed() { "PASS" } else { "FAIL" }));
        out
    }
}

/// Phrases that should never reach an explanation: they only come from untrusted evidence.
const INJECTION_MARKERS: [&str; 10] = [
    "ignore previous",
    "ignore all",
    "instruction",
    "system prompt",
    "as an ai",
    "http://",
    "https://",
    "approve this",
    "release charge",
    "processing charge",
];

fn looks_like_secret(text: &str) -> Option<&'static str> {
    let bytes = text.as_bytes();
    let token_after = |prefix: &str| {
        text.match_indices(prefix).any(|(i, _)| {
            bytes[i + prefix.len()..].iter().take_while(|b| b.is_ascii_alphanumeric() || **b == b'_' || **b == b'-').count() >= 20
        })
    };
    if token_after("hf_") {
        Some("hf_ token")
    } else if token_after("sk-") {
        Some("sk- key")
    } else if text.contains("Authorization: Bearer") || text.contains("HF_TOKEN=") {
        Some("credential header/assignment")
    } else {
        None
    }
}

pub fn run(dataset_dir: &Path, output: &Path, usage: &Path, rerun: Option<&Path>) -> Result<Signoff> {
    let mut s = Signoff::default();
    let ds = Dataset::load(dataset_dir, &dataset_dir.join("requests.csv"))?;
    let (header, rows) = contract::read_output(output)?;
    let rep = contract::validate_rows(&ds, &header, &rows);
    s.check(
        "contract",
        rep.passed(),
        format!("{} rows (expected {}), {} errors, {} warnings", rep.rows, ds.requests.len(), rep.errors().count(), rep.warnings().count()),
    );

    let mut by_status: BTreeMap<&str, usize> = BTreeMap::new();
    for r in &rows {
        *by_status.entry(r.affordability_status.as_str()).or_default() += 1;
    }
    let dominant = by_status.values().copied().max().unwrap_or(0);
    s.check(
        "status distribution",
        rows.is_empty() || dominant * 10 < rows.len() * 9,
        format!("{by_status:?} (fails if one status is >= 90% of rows)"),
    );

    let tainted: Vec<&str> = rows
        .iter()
        .filter(|r| {
            let t = r.decision_explanation.to_lowercase();
            INJECTION_MARKERS.iter().any(|m| t.contains(m))
        })
        .map(|r| r.request_id.as_str())
        .collect();
    s.check("explanations free of injected text", tainted.is_empty(), format!("{tainted:?}"));

    // main.rs writes a contract-valid fallback row when the engine errors; that must not ship.
    let fallbacks: Vec<&str> = rows
        .iter()
        .filter(|r| r.decision_explanation.to_lowercase().contains("engine error"))
        .map(|r| r.request_id.as_str())
        .collect();
    s.check("no engine-error fallback rows", fallbacks.is_empty(), format!("{fallbacks:?}"));

    let ex = super::explanation::check(&ds, &rows);
    let ex_err: Vec<String> = ex.iter().filter(|f| f.severity == Severity::Error).map(|f| f.to_string()).collect();
    let ex_warn = ex.len() - ex_err.len();
    s.check(
        "explanations grounded, worded for their method, not copied across rows",
        ex_err.is_empty(),
        if ex_err.is_empty() { format!("0 errors, {ex_warn} duplicate-template warnings") } else { ex_err.iter().take(10).cloned().collect::<Vec<_>>().join(" | ") },
    );

    // Evidence the pipeline applies == independent regeneration (snapshots + family coverage).
    let repo_for_code = dataset_dir.parent().unwrap_or(Path::new(".."));
    // decision.accuracy_first: every false accept found by any check is collected for the hard gate.
    let mut false_accepts: Vec<String> = Vec::new();
    let mut gate_ran = true;
    let collect = |f: &[super::contract::Finding], into: &mut Vec<String>| {
        into.extend(f.iter().filter(|x| super::false_accepts::FALSE_ACCEPT_CODES.contains(&x.code)).map(|x| x.to_string()));
    };
    match super::evidence_consistency::check(dataset_dir, &repo_for_code.join("code")) {
        Err(e) => {
            gate_ran = false;
            s.check("evidence consistency", false, format!("could not run: {e}"))
        }
        Ok(f) => {
            collect(&f, &mut false_accepts);
            let errs: Vec<String> = f.iter().filter(|x| x.severity == Severity::Error).map(|x| x.to_string()).collect();
            let warns = f.len() - errs.len();
            s.check(
                "evidence applied == independent regeneration (snapshot drift + family coverage)",
                errs.is_empty(),
                if errs.is_empty() { format!("0 errors, {warns} unclassified-message warnings") } else { errs.iter().take(8).cloned().collect::<Vec<_>>().join(" | ") },
            );
        }
    }

    // Board decision.vlm_setup: image amounts only on 2-model agreement (or fallback tiebreak).
    match super::image_agreement::check(&repo_for_code.join("code"), dataset_dir, &repo_for_code.join("code/config/models.toml")) {
        Err(e) => {
            gate_ran = false;
            s.check("image amounts: 2-model agreement", false, format!("could not run: {e}"))
        }
        Ok(f) => {
            collect(&f, &mut false_accepts);
            let image_facts = std::fs::read_dir(repo_for_code.join("code/store/processed/evidence"))
                .map(|rd| rd.flatten().filter_map(|e| std::fs::read_to_string(e.path()).ok()).map(|t| t.matches("\"EventAmount\"").count()).sum::<usize>())
                .unwrap_or(0);
            s.check(
                "image amounts: routed agreement per [vlm_routing] (decision.vlm_setup)",
                f.is_empty(),
                if f.is_empty() { format!("ok; {image_facts} EventAmount facts in persisted evidence") } else { f.iter().take(8).map(|x| x.to_string()).collect::<Vec<_>>().join(" | ") },
            );
        }
    }

    // Image amounts vs hand-read audit gold, then the hard gate itself.
    let mut image_facts_checked = 0;
    match super::false_accepts::image_gold_findings(&repo_for_code.join("code"), Some(repo_for_code)) {
        Err(e) => {
            gate_ran = false;
            s.check("image amounts equal audit gold", false, format!("could not run: {e}"));
        }
        Ok((f, n)) => {
            image_facts_checked = n;
            false_accepts.extend(f.iter().map(|x| x.to_string()));
        }
    }
    s.check(
        "HARD GATE decision.accuracy_first: 0 false accepts (a flagged missing value beats a wrong one)",
        gate_ran && false_accepts.is_empty(),
        if !gate_ran {
            "a contributing check could not run: gate cannot pass".to_string()
        } else if false_accepts.is_empty() {
            format!("0 false accepts; {image_facts_checked} image facts compared with audit gold; model message facts grounded; no agreement-less image amounts")
        } else {
            format!("{} false accepts: {}", false_accepts.len(), false_accepts.join(" | "))
        },
    );

    // Engine-backed stage: re-run the batch path and check what the file alone cannot show.
    match super::mirror::run(dataset_dir, &dataset_dir.join("requests.csv"), &rows) {
        Err(e) => s.check("engine mirror", false, format!("could not run: {e}")),
        Ok(m) => {
            let errs: Vec<String> = m.findings.iter().filter(|f| f.severity == Severity::Error).map(|f| f.to_string()).collect();
            let blank: Vec<&String> = errs.iter().filter(|e| e.contains("[BA")).collect();
            let inv: Vec<&String> = errs.iter().filter(|e| !e.contains("[BA") && !e.contains("[EX1_number_not_in_facts]")).collect();
            let exf: Vec<&String> = errs.iter().filter(|e| e.contains("[EX1_number_not_in_facts]")).collect();
            s.check("engine errors", m.engine_errors.is_empty(), format!("{:?}", m.engine_errors));
            s.check("invariants on every row (engine forecast replay)", inv.is_empty(), format!("{} rows; {}", m.rows, inv.iter().take(8).map(|x| x.as_str()).collect::<Vec<_>>().join(" | ")));
            s.check(
                "blank amounts: reconciled image figure or flagged missing, never zero",
                blank.is_empty(),
                format!(
                    "{}; rows with missing_amounts: {:?}",
                    if blank.is_empty() { "ok".to_string() } else { blank.iter().take(8).map(|x| x.as_str()).collect::<Vec<_>>().join(" | ") },
                    m.missing_amount_rows
                ),
            );
            s.check("explanation numbers match DecisionFacts", exf.is_empty(), exf.iter().take(8).map(|x| x.as_str()).collect::<Vec<_>>().join(" | "));
            s.check("shipped rows equal the engine's rows", m.diverged.is_empty(), format!("{} diverged {:?}", m.diverged.len(), m.diverged.iter().take(10).collect::<Vec<_>>()));
        }
    }

    let raw_output = std::fs::read_to_string(output)?;
    s.check("output.csv has no secrets", looks_like_secret(&raw_output).is_none(), looks_like_secret(&raw_output).unwrap_or("none found"));

    match std::fs::read_to_string(usage) {
        Err(e) => s.check("usage report present", false, format!("{}: {e}", usage.display())),
        Ok(text) => {
            let lower = text.to_lowercase();
            let required = [
                ("provider", "provider"),
                ("model", "model"),
                ("calls", "call"),
                ("input tokens", "input"),
                ("output tokens", "output"),
                ("tokens", "token"),
                ("per request", "per request"),
                ("cost", "cost"),
            ];
            let missing: Vec<&str> = required.iter().filter(|(_, k)| !lower.contains(k)).map(|(n, _)| *n).collect();
            s.check("usage report present", !text.trim().is_empty() && !lower.contains("pending"), format!("{} bytes", text.len()));
            s.check("usage report sections", missing.is_empty(), format!("missing: {missing:?}"));
            s.check("usage report has no secrets", looks_like_secret(&text).is_none(), looks_like_secret(&text).unwrap_or("none found"));
        }
    }

    // Hardcoded answers in the prediction path (lead: flag at final sign-off). Paths resolve
    // from the dataset dir's parent (repo root): code/src/{engine,extract}, docs/gold_subset.json.
    let repo = dataset_dir.parent().unwrap_or(Path::new(".."));
    let hard = super::hardcode_scan::scan(&repo.join("code"), dataset_dir, Some(&repo.join("docs/gold_subset.json")))?;
    s.check(
        "no hardcoded ids/label figures in engine+extract",
        hard.is_empty(),
        if hard.is_empty() { "none found".to_string() } else { hard.iter().take(12).map(|f| f.to_string()).collect::<Vec<_>>().join(" | ") },
    );

    if let Some(rerun) = rerun {
        let a = std::fs::read(output)?;
        let b = std::fs::read(rerun).with_context(|| format!("read {}", rerun.display()))?;
        let first_diff = a.iter().zip(&b).position(|(x, y)| x != y);
        s.check(
            "warm rerun byte-identical",
            a == b,
            match (a == b, first_diff) {
                (true, _) => format!("{} bytes identical", a.len()),
                (false, Some(i)) => format!("first difference at byte {i}"),
                (false, None) => format!("lengths differ: {} vs {}", a.len(), b.len()),
            },
        );
    }
    s.contract = Some(rep);
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_patterns() {
        assert!(looks_like_secret("token hf_abcdefghijklmnopqrstuvwxyz123").is_some());
        assert!(looks_like_secret("the hf_router config").is_none());
        assert!(looks_like_secret("sk-ABCDEFGHIJKLMNOPQRSTUVWX").is_some());
        assert!(looks_like_secret("Pay IDR 5,491,000 in full").is_none());
    }

    #[test]
    fn signoff_on_sample_shaped_files() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../dataset");
        let tmp = std::env::temp_dir().join(format!("verifier_signoff_{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        // The blank template fails the contract; a missing usage report fails too.
        let s = run(&dir, &dir.join("output.csv"), &tmp.join("nope.md"), Some(&dir.join("output.csv"))).unwrap();
        assert!(!s.passed());
        let names: Vec<(&str, bool)> = s.checks.iter().map(|(n, ok, _)| (n.as_str(), *ok)).collect();
        assert!(names.contains(&("contract", false)));
        assert!(names.contains(&("usage report present", false)));
        assert!(names.contains(&("warm rerun byte-identical", true)));
        let usage = tmp.join("usage.md");
        std::fs::write(&usage, "Provider: x. Model: y. 3 calls. Input tokens 1, output tokens 2. Per request avg. Cost $0.").unwrap();
        let s = run(&dir, &dir.join("output.csv"), &usage, None).unwrap();
        assert!(s.checks.iter().any(|(n, ok, _)| n == "usage report sections" && *ok));
        std::fs::remove_dir_all(&tmp).ok();
    }
}
