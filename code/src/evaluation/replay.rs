//! Independent replay of a recommended plan against the engine's day-by-day forecast
//! (INVARIANTS.md section F).
//!
//! The engine hands over its projected closing balances; this module re-applies the plan's
//! payments on its own and re-derives the safe amount and the earliest full-payment date with a
//! plain O(n) scan, so an off-by-one in the engine's suffix minimum or payment dating shows up.

use chrono::{Duration, NaiveDate};

use super::contract::{parse_changes, parse_plan, Finding, OutputRow, Severity};
use super::data::{cents_to_f64, parse_cents, parse_date, Request};

/// Half a cent: anything beyond this is a real difference, not float noise.
pub const TOL: f64 = 0.005;

/// The engine's forecast for one request.
#[derive(Clone, Debug)]
pub struct ForecastSeries<'a> {
    /// Must equal `request_date`; `baseline[i]` is the balance at the end of `start + i` days.
    pub start: NaiveDate,
    /// `minimum_balance_to_keep` in home currency.
    pub minimum: f64,
    /// Projected balances with no payment for this request and no spending changes.
    pub baseline: &'a [f64],
    /// Same projection with the recommended spending changes applied. Required when the row has changes.
    pub with_changes: Option<&'a [f64]>,
}

fn finding(req: &Request, severity: Severity, code: &'static str, detail: String) -> Finding {
    Finding { request_id: req.request_id.clone(), severity, code, detail }
}

/// Suffix minima of `b - minimum`: `out[i] = min over j >= i of (b[j] - minimum)`.
pub fn suffix_headroom(b: &[f64], minimum: f64) -> Vec<f64> {
    let mut out = vec![f64::INFINITY; b.len()];
    let mut run = f64::INFINITY;
    for i in (0..b.len()).rev() {
        run = run.min(b[i] - minimum);
        out[i] = run;
    }
    out
}

pub fn replay(req: &Request, row: &OutputRow, fc: &ForecastSeries) -> Vec<Finding> {
    let mut out = Vec::new();
    let mut err = |code, detail| out.push(finding(req, Severity::Error, code, detail));

    if fc.start != req.request_date {
        err("F0_series_start", format!("series starts {} but request_date is {}", fc.start, req.request_date));
        return out;
    }
    if fc.baseline.is_empty() || fc.baseline.iter().any(|v| !v.is_finite()) {
        err("F0_series_invalid", "baseline is empty or has a non-finite value".into());
        return out;
    }
    if let Some(w) = fc.with_changes {
        if w.len() != fc.baseline.len() || w.iter().any(|v| !v.is_finite()) {
            err("F0_series_invalid", "with_changes length differs from baseline or has a non-finite value".into());
            return out;
        }
    }
    let requested = cents_to_f64(req.requested);
    let has_changes = parse_changes(&row.spending_changes_needed).map(|c| !c.is_empty()).unwrap_or(false);
    let series = match (has_changes, fc.with_changes) {
        (true, Some(w)) => w,
        (true, None) => {
            err("F1_changes_series_missing", "row has spending changes but no with_changes series".into());
            return out;
        }
        (false, _) => fc.baseline,
    };

    // F1: every day of the horizon stays at or above the minimum after cumulative plan payments.
    if let Ok(plan) = parse_plan(&row.payment_plan) {
        let mut paid = 0.0;
        let mut next = 0;
        let mut worst: Option<(NaiveDate, f64)> = None;
        for (i, bal) in series.iter().enumerate() {
            let day = fc.start + Duration::days(i as i64);
            while next < plan.len() && plan[next].date <= day {
                paid += cents_to_f64(plan[next].amount);
                next += 1;
            }
            let left = bal - paid - fc.minimum;
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

    let headroom = suffix_headroom(fc.baseline, fc.minimum);
    let mut warn = |code, detail| out.push(finding(req, Severity::Warn, code, detail));

    // F2: amount_safe_to_pay vs the closed form on the no-change series.
    if let Ok(amount) = parse_cents(&row.amount_safe_to_pay) {
        let safe = headroom[0].max(0.0).min(requested);
        let got = cents_to_f64(amount);
        if got > safe + TOL {
            warn("F2_amount_over_safe", format!("amount_safe_to_pay {got:.2} > replayed safe {safe:.2}"));
        } else if got < safe - TOL {
            warn("F2_amount_under_safe", format!("amount_safe_to_pay {got:.2} < replayed safe {safe:.2}"));
        }
    }

    // F3: earliest date is the first day whose suffix headroom covers the full amount.
    let replayed = headroom.iter().position(|h| *h >= requested - TOL).map(|i| fc.start + Duration::days(i as i64));
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

    #[test]
    fn suffix_headroom_is_suffix_min() {
        assert_eq!(suffix_headroom(&[5.0, 3.0, 9.0, 4.0], 1.0), vec![2.0, 2.0, 3.0, 3.0]);
    }

    #[test]
    fn partial_plan_replays_clean_and_breach_is_caught() {
        // Headroom 40 until day 3 (salary), then 200.
        let b = [140.0, 140.0, 140.0, 300.0, 300.0];
        let fc = ForecastSeries { start: req().request_date, minimum: 100.0, baseline: &b, with_changes: None };
        let ok = row("40", "2024-01-01:40|2024-01-04:60", "2024-01-04");
        assert!(replay(&req(), &ok, &fc).is_empty(), "{:?}", replay(&req(), &ok, &fc));

        let bad = row("40", "2024-01-01:41|2024-01-04:59", "2024-01-04");
        let f = replay(&req(), &bad, &fc);
        assert!(f.iter().any(|x| x.code == "F1_plan_breaches_minimum"));

        let late = row("40", "2024-01-01:40|2024-01-05:60", "2024-01-05");
        assert!(replay(&req(), &late, &fc).iter().any(|x| x.code == "F3_earliest_mismatch"));

        let wrong_start = ForecastSeries { start: NaiveDate::from_ymd_opt(2024, 1, 2).unwrap(), ..fc.clone() };
        assert!(replay(&req(), &ok, &wrong_start).iter().any(|x| x.code == "F0_series_start"));
    }

    #[test]
    fn changes_need_their_own_series() {
        let b = [150.0, 150.0];
        let fc = ForecastSeries { start: req().request_date, minimum: 100.0, baseline: &b, with_changes: None };
        let mut r = row("50", "2024-01-01:100", "");
        r.spending_changes_needed = "stop:event_1".into();
        assert!(replay(&req(), &r, &fc).iter().any(|x| x.code == "F1_changes_series_missing"));
        let w = [200.0, 200.0];
        let fc2 = ForecastSeries { with_changes: Some(&w), ..fc };
        assert!(!replay(&req(), &r, &fc2).iter().any(|x| x.severity == Severity::Error));
    }
}
