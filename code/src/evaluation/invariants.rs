//! Hard invariant assertions the engine runs before a row is written (PLAN.md §2.10).
//!
//! A violation is returned as an error, never clamped or fixed up: all arithmetic is
//! deterministic, so a violation is a bug we want to see.
//!
//! ```ignore
//! let inv = Invariants::load(Path::new("../dataset"), Path::new("../dataset/requests.csv"))?;
//! let fc = ForecastSeries { start, minimum, baseline: &b, with_changes: Some(&w) };
//! let warnings = inv.assert_row(&row, &fc)?;   // Err(InvariantViolation) aborts the run
//! inv.assert_file(&rows)?;                     // header, one row per request, then every row
//! ```

use std::fmt;
use std::path::Path;

use anyhow::Result;

use super::contract::{self, Finding, OutputRow, Report, Severity, HEADER};
use super::data::Dataset;
use super::replay::{self, ForecastSeries};

#[derive(Debug)]
pub struct InvariantViolation {
    pub findings: Vec<Finding>,
}

impl fmt::Display for InvariantViolation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "{} invariant violation(s):", self.findings.len())?;
        for x in &self.findings {
            writeln!(f, "  {x}")?;
        }
        Ok(())
    }
}

impl std::error::Error for InvariantViolation {}

impl From<&crate::model::OutputRow> for OutputRow {
    fn from(r: &crate::model::OutputRow) -> OutputRow {
        OutputRow {
            request_id: r.request_id.clone(),
            amount_safe_to_pay: r.amount_safe_to_pay.clone(),
            affordability_status: r.affordability_status.clone(),
            recommended_payment_method: r.recommended_payment_method.clone(),
            payment_plan: r.payment_plan.clone(),
            earliest_date_for_full_payment: r.earliest_date_for_full_payment.clone(),
            spending_changes_needed: r.spending_changes_needed.clone(),
            decision_explanation: r.decision_explanation.clone(),
        }
    }
}

pub struct Invariants {
    pub ds: Dataset,
}

fn split(findings: Vec<Finding>) -> Result<Vec<Finding>, InvariantViolation> {
    let (errors, warnings): (Vec<_>, Vec<_>) = findings.into_iter().partition(|f| f.severity == Severity::Error);
    if errors.is_empty() {
        Ok(warnings)
    } else {
        Err(InvariantViolation { findings: errors })
    }
}

impl Invariants {
    /// Loads its own copy of the dataset (independent of `crate::model`).
    pub fn load(dataset_dir: &Path, requests_file: &Path) -> Result<Invariants> {
        Ok(Invariants { ds: Dataset::load(dataset_dir, requests_file)? })
    }

    /// Contract rules plus the forecast replay for one row. Ok carries the warnings.
    pub fn assert_row<R>(&self, row: R, forecast: &ForecastSeries) -> Result<Vec<Finding>, InvariantViolation>
    where
        R: Into<OutputRow>,
    {
        let row: OutputRow = row.into();
        let mut findings = contract::check_row(&self.ds, &row);
        if let Some(req) = self.ds.request(&row.request_id) {
            findings.extend(replay::replay(req, &row, forecast));
        }
        split(findings)
    }

    /// Contract rules only (no forecast available, e.g. validating a finished file).
    pub fn assert_row_contract<R: Into<OutputRow>>(&self, row: R) -> Result<Vec<Finding>, InvariantViolation> {
        split(contract::check_row(&self.ds, &row.into()))
    }

    /// Whole-file rules (exact header, one row per request) plus every row's contract rules.
    pub fn assert_file<R>(&self, rows: &[R]) -> Result<Report, InvariantViolation>
    where
        for<'a> &'a R: Into<OutputRow>,
    {
        let rows: Vec<OutputRow> = rows.iter().map(Into::into).collect();
        let header: Vec<String> = HEADER.iter().map(|s| s.to_string()).collect();
        let report = contract::validate_rows(&self.ds, &header, &rows);
        if report.passed() {
            Ok(report)
        } else {
            Err(InvariantViolation { findings: report.errors().cloned().collect() })
        }
    }
}

impl From<&OutputRow> for OutputRow {
    fn from(r: &OutputRow) -> OutputRow {
        r.clone()
    }
}
