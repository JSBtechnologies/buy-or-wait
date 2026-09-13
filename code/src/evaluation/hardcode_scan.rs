//! Static scan for hardcoded answers in the prediction path (`src/engine/`, `src/extract/`).
//!
//! Test code is skipped: files named `*test*` or `samples.rs`, and everything from a file's first
//! `#[cfg(test)]` onward. In the remaining code it flags:
//! - record-id string literals (`"request_64"`, `"event_6033"`, `"image_10"`, ...): per-record rules
//! - numeric literals equal to a sample label amount or a gold-subset figure (as units, cents, or
//!   engine Money at 1e4 scale), for figures ≥ 1000 so small constants do not trip it
//! - references to label or gold files (`sample_requests.csv`, `gold_subset`)

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::Result;

use super::contract::{parse_changes, parse_plan, Finding, Severity};
use super::data::{parse_cents, Cents, Dataset};

const ID_PREFIXES: [&str; 6] = ["request_", "event_", "user_", "message_", "image_", "payment_option_"];

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            rust_files(&p, out);
        } else if p.extension().map(|x| x == "rs").unwrap_or(false) {
            out.push(p);
        }
    }
}

fn production_part(path: &Path, text: &str) -> Option<String> {
    let name = path.file_name()?.to_string_lossy().to_lowercase();
    if name.contains("test") || name == "samples.rs" {
        return None;
    }
    let cut = text.find("#[cfg(test)]").unwrap_or(text.len());
    Some(text[..cut].to_string())
}

/// Figures that must never appear as literals: sample label amounts and gold-subset numbers.
pub fn forbidden_figures(dataset_dir: &Path, gold: Option<&Path>) -> Result<BTreeSet<Cents>> {
    let ds = Dataset::load(dataset_dir, &dataset_dir.join("sample_requests.csv"))?;
    let mut set = BTreeSet::new();
    for l in &ds.labels {
        if let Ok(c) = parse_cents(&l.amount_safe_to_pay) {
            set.insert(c);
        }
        for p in parse_plan(&l.payment_plan).unwrap_or_default() {
            set.insert(p.amount);
        }
        for ch in parse_changes(&l.spending_changes_needed).unwrap_or_default() {
            if let super::contract::Change::ReduceTo(_, c) = ch {
                set.insert(c);
            }
        }
    }
    if let Some(g) = gold.filter(|g| g.exists()) {
        let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(g)?)?;
        fn walk(v: &serde_json::Value, set: &mut BTreeSet<Cents>) {
            match v {
                serde_json::Value::Number(n) => {
                    if let Some(f) = n.as_f64() {
                        set.insert((f * 100.0).round() as Cents);
                    }
                }
                serde_json::Value::Array(a) => a.iter().for_each(|x| walk(x, set)),
                serde_json::Value::Object(m) => m.values().for_each(|x| walk(x, set)),
                _ => {}
            }
        }
        walk(&v, &mut set);
    }
    set.retain(|c| *c >= 1000_00);
    Ok(set)
}

/// Every figure (expected and alt) in the analyst's image audit reference, read at runtime from
/// RULES.md. Validation only: no audit figure is compiled into this crate.
pub fn image_gold(rules_md: Option<&Path>) -> Option<BTreeSet<Cents>> {
    let text = std::fs::read_to_string(rules_md?).ok()?;
    let set: BTreeSet<Cents> = super::false_accepts::gold_from_rules(&text).into_values().flat_map(|g| g.amounts).collect();
    (!set.is_empty()).then_some(set)
}

/// Numeric literals in code as cents under each plausible scale (units, cents, 1e4 Money).
fn literal_values(code: &str) -> Vec<(usize, Vec<Cents>, String)> {
    let mut out = Vec::new();
    let b = code.as_bytes();
    let mut i = 0;
    while i < b.len() {
        let starts = b[i].is_ascii_digit() && (i == 0 || !(b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'_' || b[i - 1] == b'.'));
        if !starts {
            i += 1;
            continue;
        }
        let s = i;
        while i < b.len() && (b[i].is_ascii_digit() || b[i] == b'_' || (b[i] == b'.' && b.get(i + 1).map(|c| c.is_ascii_digit()).unwrap_or(false))) {
            i += 1;
        }
        let tok: String = code[s..i].chars().filter(|c| *c != '_').collect();
        let line = code[..s].matches('\n').count() + 1;
        let mut vals = Vec::new();
        if let Ok(c) = parse_cents(&tok) {
            vals.push(c);
            if !tok.contains('.') {
                vals.push(c / 100); // literal already in cents
                if c % 10_000 == 0 {
                    vals.push(c / 10_000); // literal in Money(1e4) units → cents = raw/100
                }
            }
        }
        if !vals.is_empty() {
            out.push((line, vals, tok));
        }
    }
    out
}

/// Prediction modules (engine, extract, main.rs) must not contain record ids, label/gold/audit
/// figures, or references to label/gold/audit files. Test code anywhere under `code/` is reported
/// (warning) when it embeds an image audit figure, so owners can keep the package free of anything
/// that looks like precomputed answers.
pub fn scan(code_dir: &Path, dataset_dir: &Path, gold: Option<&Path>, rules_md: Option<&Path>) -> Result<Vec<Finding>> {
    let figures = forbidden_figures(dataset_dir, gold)?;
    let mut out = Vec::new();
    let images = match image_gold(rules_md) {
        Some(i) => i,
        None => {
            out.push(Finding { request_id: "RULES.md".into(), severity: Severity::Error, code: "H0_audit_reference_unavailable", detail: "image audit table not found: image figures cannot be scanned".into() });
            BTreeSet::new()
        }
    };
    let mut files = Vec::new();
    for sub in ["src/engine", "src/extract"] {
        rust_files(&code_dir.join(sub), &mut files);
    }
    files.push(code_dir.join("src/main.rs"));
    files.sort();
    for f in files {
        let Ok(text) = std::fs::read_to_string(&f) else { continue };
        let Some(code) = production_part(&f, &text) else { continue };
        let rel = f.strip_prefix(code_dir).unwrap_or(&f).display().to_string().replace('\\', "/");
        let mut push = |line: usize, code_: &'static str, detail: String| {
            out.push(Finding { request_id: format!("{rel}:{line}"), severity: Severity::Error, code: code_, detail })
        };
        for (lineno, line) in code.lines().enumerate() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("//") {
                continue;
            }
            for p in ID_PREFIXES {
                let mut rest = line;
                while let Some(pos) = rest.find(&format!("\"{p}")) {
                    let after = &rest[pos + 1 + p.len()..];
                    let digits = after.bytes().take_while(|c| c.is_ascii_digit()).count();
                    if digits > 0 && after.as_bytes().get(digits) == Some(&b'"') {
                        push(lineno + 1, "H1_record_id_literal", format!("\"{p}{}\"", &after[..digits]));
                    }
                    rest = &rest[pos + 1..];
                }
            }
            for needle in ["sample_requests.csv", "gold_subset", "RULES.md", "false_accepts", "image_gold", "image_audit"] {
                if line.contains(needle) {
                    push(lineno + 1, "H3_label_file_reference", needle.to_string());
                }
            }
        }
        let comment_free: String = code.lines().map(|l| if l.trim_start().starts_with("//") { "" } else { l }).collect::<Vec<_>>().join("\n");
        for (line, vals, tok) in literal_values(&comment_free) {
            if let Some(hit) = vals.iter().find(|v| figures.contains(v)) {
                push(line, "H2_label_or_gold_figure", format!("literal {tok} equals label/gold figure {}", super::data::fmt_cents_2dp(*hit)));
            } else if let Some(hit) = vals.iter().find(|v| images.contains(v)) {
                push(line, "H4_image_gold_figure", format!("literal {tok} equals an image audit figure {} (validation only)", super::data::fmt_cents_2dp(*hit)));
            }
        }
    }

    // Appearance: audit figures embedded in test code anywhere in the shipped package.
    let mut all = Vec::new();
    rust_files(&code_dir.join("src"), &mut all);
    rust_files(&code_dir.join("tests"), &mut all);
    all.sort();
    for f in all {
        let Ok(text) = std::fs::read_to_string(&f) else { continue };
        let name = f.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
        let whole_file_is_test = name.contains("test") || name == "samples.rs" || f.starts_with(code_dir.join("tests"));
        let offset = if whole_file_is_test {
            0
        } else {
            match text.find("#[cfg(test)]") {
                Some(i) => i,
                None => continue,
            }
        };
        let test_part = &text[offset..];
        let base_line = text[..offset].matches('\n').count();
        let rel = f.strip_prefix(code_dir).unwrap_or(&f).display().to_string().replace('\\', "/");
        let comment_free: String = test_part.lines().map(|l| if l.trim_start().starts_with("//") { "" } else { l }).collect::<Vec<_>>().join("\n");
        for (line, vals, tok) in literal_values(&comment_free) {
            if let Some(hit) = vals.iter().find(|v| images.contains(v)) {
                out.push(Finding {
                    request_id: format!("{rel}:{}", base_line + line),
                    severity: Severity::Warn,
                    code: "H6_audit_figure_in_test_code",
                    detail: format!("test literal {tok} equals an image audit figure {}; prefer synthetic values or reading RULES.md at test time", super::data::fmt_cents_2dp(*hit)),
                });
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Synthetic audit table for tests (no real audit figures in this crate).
    fn synthetic_rules(dir: &Path) -> std::path::PathBuf {
        let p = dir.join("RULES.md");
        std::fs::write(&p, "| 01 | event_1 salary, settled | **2,468,000** net pay | x | y |\n| 02 | event_2 utilities, **pending** | **654.32** amount due (alt 654.3 \"Total\") | x | y |\n").unwrap();
        p
    }

    #[test]
    fn reference_comes_from_rules_md_only() {
        let dir = std::env::temp_dir().join(format!("verifier_hc_ref_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let set = image_gold(Some(&synthetic_rules(&dir))).unwrap();
        assert_eq!(set.into_iter().collect::<Vec<_>>(), vec![654_30, 654_32, 2_468_000_00]);
        assert!(image_gold(Some(&dir.join("missing.md"))).is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn detects_ids_figures_and_label_files_but_not_tests_or_comments() {
        let root = std::env::temp_dir().join(format!("verifier_hardcode_{}", std::process::id()));
        let eng = root.join("src/engine");
        std::fs::create_dir_all(&eng).unwrap();
        let rules = synthetic_rules(&root);
        std::fs::write(
            eng.join("plans.rs"),
            "// request_03 label 873000 in a comment is fine\n\
             fn f(id: &str) -> i64 { if id == \"request_03\" { return 873_000; } 0 }\n\
             const SALARY_CENTS: i64 = 246800000;\n\
             fn g() { let _ = std::fs::read(\"../dataset/sample_requests.csv\"); }\n\
             const DAYS: i64 = 90;\n\
             #[cfg(test)] mod t { const X: i64 = 873000; const Y: f64 = 654.32; fn h() { let _ = \"event_12\"; } }\n",
        )
        .unwrap();
        std::fs::write(eng.join("scenario_tests.rs"), "const Y: &str = \"event_476\";").unwrap();
        std::fs::write(root.join("src/main.rs"), "fn main() { let _ = std::fs::read_to_string(\"../RULES.md\"); }\n").unwrap();
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../dataset");
        let f = scan(&root, &dir, None, Some(&rules)).unwrap();
        let errs: Vec<(&str, &str)> = f.iter().filter(|x| x.severity == Severity::Error).map(|x| (x.request_id.as_str(), x.code)).collect();
        assert!(errs.contains(&("src/engine/plans.rs:2", "H1_record_id_literal")), "{f:?}");
        assert!(errs.contains(&("src/engine/plans.rs:2", "H2_label_or_gold_figure")), "{f:?}");
        assert!(errs.contains(&("src/engine/plans.rs:3", "H4_image_gold_figure")), "{f:?}");
        assert!(errs.contains(&("src/engine/plans.rs:4", "H3_label_file_reference")), "{f:?}");
        assert!(errs.contains(&("src/main.rs:1", "H3_label_file_reference")), "{f:?}");
        assert_eq!(errs.len(), 5, "{f:?}");
        // The audit figure inside cfg(test) is only an appearance warning.
        assert!(f.iter().any(|x| x.severity == Severity::Warn && x.code == "H6_audit_figure_in_test_code" && x.request_id.starts_with("src/engine/plans.rs:")), "{f:?}");

        // Image audit figures in units, cents and Money(1e4) forms, incl. the alt rendering.
        std::fs::write(eng.join("plans.rs"), "const A: f64 = 654.32;\nconst B: i64 = 65430;\nconst C: i64 = 6_543_200;\nconst OK: f64 = 1.5;\n").unwrap();
        let f = scan(&root, &dir, None, Some(&rules)).unwrap();
        let lines: Vec<&str> = f.iter().filter(|x| x.code == "H4_image_gold_figure").map(|x| x.request_id.as_str()).collect();
        assert_eq!(lines, vec!["src/engine/plans.rs:1", "src/engine/plans.rs:2", "src/engine/plans.rs:3"], "{f:?}");
        // No audit reference: the scan cannot vouch for image figures, so it fails.
        assert!(scan(&root, &dir, None, None).unwrap().iter().any(|x| x.code == "H0_audit_reference_unavailable"));
        std::fs::remove_dir_all(&root).ok();
    }
}
