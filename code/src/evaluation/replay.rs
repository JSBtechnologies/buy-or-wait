//! Independent replay of a recommended plan against the engine's day-by-day forecast
//! (INVARIANTS.md section F).
//!
//! The engine hands over its projected balances; this module re-applies the plan's payments on
//! its own and re-derives the safe amount and the earliest full-payment date with a plain O(n)
//! scan, so an off-by-one in the engine's suffix minimum or payment dating shows up.
//!
//! Day model (RULES.md S2.3): within a day debits apply before credits. `low[i]` is the balance
//! after day i's debits and before its credits; `eod[i]` is the end-of-day balance. A plan
//! payment on day d is made after d's credits, so it lowers `eod[d]` and every later day.

use chrono::{Duration, NaiveDate};

use super::contract::{parse_changes, parse_plan, Finding, OutputRow, Severity};
use super::data::{cents_to_f64, parse_cents, parse_date, Request};

/// Half a cent: anything beyond this is a real difference, not float noise.
pub const TOL: f64 = 0.005;

/// The engine's forecast for one request. All series start on `start` and share one length.
#[derive(Clone, Debug)]
pub struct ForecastSeries<'a> {
    /// Must equal `request_date`.
    pub start: NaiveDate,
    /// `minimum_balance_to_keep` in home currency.
    pub minimum: f64,
    /// End-of-day balances with no payment for this request and no spending changes.
    pub baseline: &'a [f64],
    /// Intraday lows (after debits, before credits) for `baseline`. `None` = same as end of day.
    pub baseline_low: Option<&'a [f64]>,
    /// End-of-day balances with the recommended spending changes. Required when the row has changes.
    pub with_changes: Option<&'a [f64]>,
    /// Intraday lows for `with_changes`.
    pub with_changes_low: Option<&'a [f64]>,
}

fn finding(req: &Request, severity: Severity, code: &'static str, detail: String) -> Finding {
    Finding { request_id: req.request_id.clone(), severity, code, detail }
}

/// `out[d] = min(eod[d], min_{t>d} low[t]) − minimum`: the most a single payment on day d can be.
pub fn headroom_from(eod: &[f64], low: &[f64], minimum: f64) -> Vec<f64> {
    let mut out = vec![f64::INFINITY; eod.len()];
    let mut later_low = f64::INFINITY;
    for d in (0..eod.len()).rev() {
        out[d] = eod[d].min(later_low) - minimum;
        later_low = later_low.min(low[d]);
    }
    out
}

fn valid(s: &[f64], len: usize) -> bool {
    s.len() == len && s.iter().all(|v| v.is_finite())
}

pub fn replay(req: &Request, row: &OutputRow, fc: &ForecastSeries) -> Vec<Finding> {
    let mut out = Vec::new();
    let mut err = |code, detail| out.push(finding(req, Severity::Error, code, detail));

    if fc.start != req.request_date {
        err("F0_series_start", format!("series starts {} but request_date is {}", fc.start, req.request_date));
        return out;
    }
    let n = fc.baseline.len();
    let all_valid = n > 0
        && valid(fc.baseline, n)
        && [fc.baseline_low, fc.with_changes, fc.with_changes_low].iter().all(|s| s.map(|s| valid(s, n)).unwrap_or(true));
    if !all_valid {
        err("F0_series_invalid", "a series is empty, has a non-finite value, or differs in length".into());
        return out;
    }
    let base_low = fc.baseline_low.unwrap_or(fc.baseline);
    let has_changes = parse_changes(&row.spending_changes_needed).map(|c| !c.is_empty()).unwrap_or(false);
    let (eod, low) = match (has_changes, fc.with_changes) {
        (true, Some(w)) => (w, fc.with_changes_low.unwrap_or(w)),
        (true, None) => {
            err("F1_changes_series_missing", "row has spending changes but no with_changes series".into());
            return out;
        }
        (false, _) => (fc.baseline, base_low),
    };

    // F1: with the plan applied, no day's low or close falls below the minimum. Days before the
    // first payment are the baseline's own business (a not_recommended row may sit on a dip).
    if let Some(plan) = parse_plan(&row.payment_plan).ok().filter(|p| !p.is_empty()) {
        let (mut through, mut next) = (0.0, 0);
        let mut worst: Option<(NaiveDate, f64)> = None;
        for i in 0..n {
            let day = fc.start + Duration::days(i as i64);
            let before = through;
            while next < plan.len() && plan[next].date <= day {
                through += cents_to_f64(plan[next].amount);
                next += 1;
            }
            let mut left = f64::INFINITY;
            if before > 0.0 {
                left = left.min(low[i] - before - fc.minimum);
            }
            if through > 0.0 {
                left = left.min(eod[i] - through - fc.minimum);
            }
            if left < -TOL && worst.map(|(_, w)| left < w).unwrap_or(true) {
                worst = Some((day, left));
            }
        }
        if let Some((day, left)) = worst {
            err(
                "F1_plan_breaches_minimum",
                format!("after plan payments the balance is {:.2} below the minimum on {day}", -left),
            );
        }
    }

    let requested = cents_to_f64(req.requested);
    let mut warn = |code, detail| out.push(finding(req, Severity::Warn, code, detail));

    // F2: amount_safe_to_pay vs clamp(min low − M, 0, req) on the no-change series.
    if let Ok(amount) = parse_cents(&row.amount_safe_to_pay) {
        let trough = base_low.iter().cloned().fold(f64::INFINITY, f64::min);
        let safe = (trough - fc.minimum).max(0.0).min(requested);
        let got = cents_to_f64(amount);
        // Engine floors to the cent, so allow up to one cent below.
        if got > safe + TOL {
            warn("F2_amount_over_safe", format!("amount_safe_to_pay {got:.2} > replayed safe {safe:.2}"));
        } else if got < safe - 0.01 - TOL {
            warn("F2_amount_under_safe", format!("amount_safe_to_pay {got:.2} < replayed safe {safe:.2}"));
        }
    }

    // F3: earliest = first day whose single-payment headroom covers the full amount.
    let head = headroom_from(fc.baseline, base_low, fc.minimum);
    let replayed = head.iter().position(|h| *h >= requested - TOL).map(|i| fc.start + Duration::days(i as i64));
    let claimed = if row.earliest_date_for_full_payment.is_empty() {
        None
    } else {
        parse_date(&row.earliest_date_for_full_payment).ok()
    };
    if claimed != replayed {
        warn(
            "F3_earliest_mismatch",
            format!(
                "earliest {} but replay gives {}",
                claimed.map(|d| d.to_string()).unwrap_or_else(|| "empty".into()),
                replayed.map(|d| d.to_string()).unwrap_or_else(|| "empty".into())
            ),
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req() -> Request {
        Request {
            request_id: "r".into(),
            user_id: "u".into(),
            request_date: NaiveDate::from_ymd_opt(2024, 1, 1).unwrap(),
            requested: 100_00,
            desired_completion_date: NaiveDate::from_ymd_opt(2024, 1, 10).unwrap(),
            allows_partial_payment: true,
        }
    }

    fn row(amount: &str, plan: &str, earliest: &str) -> OutputRow {
        OutputRow {
            request_id: "r".into(),
            amount_safe_to_pay: amount.into(),
            payment_plan: plan.into(),
            earliest_date_for_full_payment: earliest.into(),
            spending_changes_needed: "none".into(),
            ..Default::default()
        }
    }

    fn series<'a>(eod: &'a [f64], low: Option<&'a [f64]>) -> ForecastSeries<'a> {
        ForecastSeries {
            start: req().request_date,
            minimum: 100.0,
            baseline: eod,
            baseline_low: low,
            with_changes: None,
            with_changes_low: None,
        }
    }

    #[test]
    fn headroom_uses_close_on_payment_day_and_lows_after() {
        // day 3: a 30 debit before a 190 salary credit.
        let eod = [140.0, 140.0, 140.0, 300.0, 300.0];
        let low = [140.0, 140.0, 140.0, 110.0, 300.0];
        assert_eq!(headroom_from(&eod, &low, 100.0), vec![10.0, 10.0, 10.0, 200.0, 200.0]);
        assert_eq!(headroom_from(&eod, &eod, 100.0), vec![40.0, 40.0, 40.0, 200.0, 200.0]);
    }

    #[test]
    fn partial_plan_on_salary_day_replays_clean_and_breach_is_caught() {
        let eod = [140.0, 140.0, 140.0, 300.0, 300.0];
        let low = [140.0, 140.0, 140.0, 110.0, 300.0];
        let fc = series(&eod, Some(&low));
        // safe = min low − M = 10; E = day 3 (payment after the salary).
        let ok = row("10", "2024-01-01:10|2024-01-04:90", "2024-01-04");
        assert!(replay(&req(), &ok, &fc).is_empty(), "{:?}", replay(&req(), &ok, &fc));

        // 11 today dips the day-3 low (110 − 11 < 100).
        let bad = row("10", "2024-01-01:11|2024-01-04:89", "2024-01-04");
        assert!(replay(&req(), &bad, &fc).iter().any(|x| x.code == "F1_plan_breaches_minimum"));

        let late = row("10", "2024-01-01:10|2024-01-05:90", "2024-01-05");
        assert!(replay(&req(), &late, &fc).iter().any(|x| x.code == "F3_earliest_mismatch"));
        let over = row("40", "none", "2024-01-04");
        assert!(replay(&req(), &over, &fc).iter().any(|x| x.code == "F2_amount_over_safe"));

        let wrong_start = ForecastSeries { start: NaiveDate::from_ymd_opt(2024, 1, 2).unwrap(), ..fc.clone() };
        assert!(replay(&req(), &ok, &wrong_start).iter().any(|x| x.code == "F0_series_start"));
        let short = [1.0];
        let mismatched = ForecastSeries { baseline_low: Some(&short), ..fc.clone() };
        assert!(replay(&req(), &ok, &mismatched).iter().any(|x| x.code == "F0_series_invalid"));
    }

    #[test]
    fn empty_plan_on_a_dipping_baseline_is_not_a_breach() {
        let dip = [90.0, 300.0];
        let fc = series(&dip, None);
        assert!(!replay(&req(), &row("0", "none", ""), &fc).iter().any(|x| x.severity == Severity::Error));
        // paying into the dip is a breach; paying after it is not
        assert!(replay(&req(), &row("0", "2024-01-01:1", ""), &fc).iter().any(|x| x.code == "F1_plan_breaches_minimum"));
        assert!(!replay(&req(), &row("0", "2024-01-02:1", ""), &fc).iter().any(|x| x.severity == Severity::Error));
    }

    #[test]
    fn changes_need_their_own_series() {
        let b = [150.0, 150.0];
        let mut r = row("50", "2024-01-01:100", "");
        r.spending_changes_needed = "stop:event_1".into();
        assert!(replay(&req(), &r, &series(&b, None)).iter().any(|x| x.code == "F1_changes_series_missing"));
        let w = [200.0, 200.0];
        let fc2 = ForecastSeries { with_changes: Some(&w), ..series(&b, None) };
        assert!(!replay(&req(), &r, &fc2).iter().any(|x| x.severity == Severity::Error));
    }
}
