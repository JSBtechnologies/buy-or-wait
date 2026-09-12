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

/// Hand-read image figures (RULES.md S5 table, analyst 9b123f6; verifier readings on bus
/// extract#130). Audit gold only: they must never appear in the prediction path. Pinned here and
/// unioned with whatever the current RULES.md table says, so edits to the table are covered.
pub const IMAGE_GOLD_CENTS: [Cents; 17] = [
    4_365_000_00, // image_01 net pay
    100_000_00,   // image_02 balance due
    41_272_00,    // image_03 cash paid
    2_854_00,     // image_04 item bill
    822_05,       // image_05 amount due after due date
    1_995_00,     // image_06 invoice total
    8_528_00,     // image_07 grand total
    8_528_10,     // image_07 alt total
    15_339_00,    // image_08 total received
    723_00,       // image_09 total received
    79_679_26,    // image_10 balance due
    3_650_00,     // image_11 amount payable
    33_50,        // image_12 total (USD)
    2_298_00,     // image_13 total paid
    4_543_00,     // image_14 total
    9_968_00,     // image_15 grand total
    393_22,       // image_16 total
];

/// Bold `**figure**` of each `| NN | event_… |` row in RULES.md's image table.
pub fn image_gold_from_rules(rules_md: &str) -> Vec<Cents> {
    let mut out = Vec::new();
    for line in rules_md.lines() {
        let cells: Vec<&str> = line.split('|').map(str::trim).collect();
        let is_image_row = cells.len() > 3
            && cells[1].len() == 2
            && cells[1].bytes().all(|b| b.is_ascii_digit())
            && cells[2].starts_with("event_");
        if !is_image_row {
            continue;
        }
        let Some(start) = cells[3].find("**") else { continue };
        let rest = &cells[3][start + 2..];
        let Some(end) = rest.find("**") else { continue };
        let figure: String = rest[..end].chars().take_while(|c| c.is_ascii_digit() || *c == ',' || *c == '.').filter(|c| *c != ',').collect();
        if let Ok(c) = parse_cents(&figure) {
            out.push(c);
        }
    }
    out
}

/// Image gold set: pinned figures plus the current RULES.md table (no size threshold).
pub fn image_gold(repo_root: Option<&Path>) -> BTreeSet<Cents> {
    let mut set: BTreeSet<Cents> = IMAGE_GOLD_CENTS.into_iter().collect();
    if let Some(text) = repo_root.and_then(|r| std::fs::read_to_string(r.join("RULES.md")).ok()) {
        set.extend(image_gold_from_rules(&text));
    }
    set
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

pub fn scan(code_dir: &Path, dataset_dir: &Path, gold: Option<&Path>) -> Result<Vec<Finding>> {
    let figures = forbidden_figures(dataset_dir, gold)?;
    let images = image_gold(code_dir.parent());
    let mut files = Vec::new();
    for sub in ["src/engine", "src/extract"] {
        rust_files(&code_dir.join(sub), &mut files);
    }
    files.sort();
    let mut out = Vec::new();
    for f in files {
        let text = std::fs::read_to_string(&f)?;
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
            for needle in ["sample_requests.csv", "gold_subset"] {
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
                push(line, "H4_image_gold_figure", format!("literal {tok} equals hand-read image figure {} (audit gold only)", super::data::fmt_cents_2dp(*hit)));
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rules_image_table() {
        let md = "| image | event | expected |
|---|---|---|
| 01 | event_253 salary | **4,365,000** net pay | x |
| 05 | event_1786 utilities | **822.05** amount due | y |
| 12 | event_7307 transport | **33.50 USD** total | z |
";
        assert_eq!(image_gold_from_rules(md), vec![4_365_000_00, 822_05, 33_50]);
        // The analyst's table on its branch must parse to the pinned 16 expected figures.
        let out = std::process::Command::new("git").args(["show", "9b123f6:RULES.md"]).current_dir(env!("CARGO_MANIFEST_DIR")).output();
        if let Some(o) = out.ok().filter(|o| o.status.success()) {
            let parsed: BTreeSet<Cents> = image_gold_from_rules(&String::from_utf8_lossy(&o.stdout)).into_iter().collect();
            assert_eq!(parsed.len(), 16, "{parsed:?}");
            assert!(parsed.iter().all(|c| IMAGE_GOLD_CENTS.contains(c)), "{parsed:?}");
        }
    }

    #[test]
    fn detects_ids_figures_and_label_files_but_not_tests_or_comments() {
        let root = std::env::temp_dir().join(format!("verifier_hardcode_{}", std::process::id()));
        let eng = root.join("src/engine");
        std::fs::create_dir_all(&eng).unwrap();
        std::fs::write(
            eng.join("plans.rs"),
            "// request_03 label 873000 in a comment is fine\n\
             fn f(id: &str) -> i64 { if id == \"request_03\" { return 873_000; } 0 }\n\
             const SALARY_CENTS: i64 = 436500000;\n\
             fn g() { let _ = std::fs::read(\"../dataset/sample_requests.csv\"); }\n\
             const DAYS: i64 = 90;\n\
             #[cfg(test)] mod t { const X: i64 = 873000; fn h() { let _ = \"event_12\"; } }\n",
        )
        .unwrap();
        std::fs::write(eng.join("scenario_tests.rs"), "const Y: &str = \"event_476\";").unwrap();
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../dataset");
        let f = scan(&root, &dir, None).unwrap();
        let codes: Vec<(&str, &str)> = f.iter().map(|x| (x.request_id.as_str(), x.code)).collect();
        assert!(codes.contains(&("src/engine/plans.rs:2", "H1_record_id_literal")), "{f:?}");
        assert!(codes.contains(&("src/engine/plans.rs:2", "H2_label_or_gold_figure")), "{f:?}");
        assert!(codes.contains(&("src/engine/plans.rs:4", "H3_label_file_reference")), "{f:?}");
        // line 3 is image_01's net pay in cents (4,365,000): image gold. Comment (1), small constant
        // (5), cfg(test) code (6) and test files are ignored.
        assert!(codes.contains(&("src/engine/plans.rs:3", "H4_image_gold_figure")), "{f:?}");
        assert_eq!(f.len(), 4, "{f:?}");

        // Image gold, including figures under 1000, in units, cents and Money(1e4) forms.
        std::fs::write(
            eng.join("plans.rs"),
            "const BILL: f64 = 3650.0;
const DUE: f64 = 822.05;
const FARE_CENTS: i64 = 3350;
const INVOICE_MONEY: i64 = 796_792_600;
const OK: f64 = 1.5;
",
        )
        .unwrap();
        let f = scan(&root, &dir, None).unwrap();
        let lines: Vec<&str> = f.iter().filter(|x| x.code == "H4_image_gold_figure").map(|x| x.request_id.as_str()).collect();
        assert_eq!(lines, vec!["src/engine/plans.rs:1", "src/engine/plans.rs:2", "src/engine/plans.rs:3", "src/engine/plans.rs:4"], "{f:?}");
        assert!(!codes.iter().any(|(l, _)| l.contains("scenario_tests") || l.ends_with(":1") || l.ends_with(":5") || l.ends_with(":6")));
        std::fs::remove_dir_all(&root).ok();
    }
}
