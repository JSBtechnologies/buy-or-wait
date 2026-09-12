//! Verification: invariants, output-contract validation, sample scoring, forecast replay
//! (owner: verifier). Rules are listed in `INVARIANTS.md`.

pub mod contract;
pub mod data;
pub mod invariants;
pub mod replay;
pub mod scorer;

use std::path::{Path, PathBuf};

use anyhow::{bail, Result};

pub use contract::{Finding, OutputRow, Report, Severity};
pub use invariants::{InvariantViolation, Invariants};
pub use replay::ForecastSeries;

const USAGE: &str = "usage:
  validate --output FILE [--dataset DIR] [--requests FILE]   contract check (default requests: DIR/requests.csv)
  score    --output FILE [--dataset DIR] [--reveal-heldout]  contract check + field scores vs DIR/sample_requests.csv
  selftest [--dataset DIR]                                   run the validator over the sample labels themselves";

/// Entry point for a `verify` subcommand. Returns the process exit code (0 pass, 1 fail, 2 usage).
pub fn cli(args: &[String]) -> Result<i32> {
    let mut cmd = None;
    let mut output = None;
    let mut dataset = PathBuf::from("../dataset");
    let mut requests = None;
    let mut reveal = false;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--output" => output = it.next().map(PathBuf::from),
            "--dataset" => dataset = it.next().map(PathBuf::from).unwrap_or(dataset),
            "--requests" => requests = it.next().map(PathBuf::from),
            "--reveal-heldout" => reveal = true,
            c if cmd.is_none() && !c.starts_with("--") => cmd = Some(c.to_string()),
            other => bail!("unexpected argument {other:?}\n{USAGE}"),
        }
    }
    match cmd.as_deref() {
        Some("validate") => {
            let Some(output) = output else { bail!("validate needs --output\n{USAGE}") };
            let requests = requests.unwrap_or_else(|| dataset.join("requests.csv"));
            let ds = data::Dataset::load(&dataset, &requests)?;
            let rep = contract::validate_file(&ds, &output)?;
            print!("{}", rep.render());
            Ok(if rep.passed() { 0 } else { 1 })
        }
        Some("score") => {
            let Some(output) = output else { bail!("score needs --output\n{USAGE}") };
            let (code, text) = score_file(&dataset, &output, reveal)?;
            print!("{text}");
            Ok(code)
        }
        Some("selftest") => {
            let rep = selftest(&dataset)?;
            print!("{}", rep.render());
            Ok(if rep.passed() { 0 } else { 1 })
        }
        _ => {
            eprintln!("{USAGE}");
            Ok(2)
        }
    }
}

/// Validate + score an engine output produced for `sample_requests.csv`.
pub fn score_file(dataset: &Path, output: &Path, reveal_heldout: bool) -> Result<(i32, String)> {
    let ds = data::Dataset::load(dataset, &dataset.join("sample_requests.csv"))?;
    let (header, rows) = contract::read_output(output)?;
    let rep = contract::validate_rows(&ds, &header, &rows);
    let mut text = String::new();
    // Contract findings for held-out rows would leak their shape; show counts only for those.
    let (held, tune): (Vec<&Finding>, Vec<&Finding>) =
        rep.findings.iter().partition(|f| scorer::is_heldout(&f.request_id));
    for f in &tune {
        text.push_str(&format!("{f}\n"));
    }
    if reveal_heldout {
        for f in &held {
            text.push_str(&format!("{f}\n"));
        }
    } else if !held.is_empty() {
        let errs = held.iter().filter(|f| f.severity == Severity::Error).count();
        text.push_str(&format!("held-out rows: {errs} contract errors, {} warnings\n", held.len() - errs));
    }
    text.push_str(&format!(
        "contract: {} rows, {} errors, {} warnings -> {}\n",
        rep.rows,
        rep.errors().count(),
        rep.warnings().count(),
        if rep.passed() { "PASS" } else { "FAIL" }
    ));
    text.push_str(&scorer::score(&ds, &rows).render(reveal_heldout));
    Ok((if rep.passed() { 0 } else { 1 }, text))
}

/// The labels must satisfy every hard rule; if they do not, the rule is wrong, not the label.
pub fn selftest(dataset: &Path) -> Result<Report> {
    let ds = data::Dataset::load(dataset, &dataset.join("sample_requests.csv"))?;
    let header: Vec<String> = contract::HEADER.iter().map(|s| s.to_string()).collect();
    let labels = ds.labels.clone();
    Ok(contract::validate_rows(&ds, &header, &labels))
}

#[cfg(test)]
mod tests;
