//! Deterministic explanation templates (RULES.md S1.6), filled only from `DecisionFacts`
//! (0 model tokens).

use chrono::{Datelike, NaiveDate};

use super::facts::DecisionFacts;
use super::money::Money;
use super::types::PaymentMethod;

pub fn render(f: &DecisionFacts) -> String {
    let cur = &f.currency;
    let money = |c: Money| format!("{cur} {}", c.fmt_grouped());
    let min = money(f.minimum_balance);
    let req = money(f.requested_amount);
    match f.method {
        PaymentMethod::FullPayment if f.changes.is_empty() => {
            format!("Pay {req} today. This leaves at least {min} available over the next 90 days.")
        }
        PaymentMethod::FullPayment => {
            format!("{}, then pay {req} today. This leaves at least {min} available.", changes_clause(f))
        }
        PaymentMethod::Installments => {
            let first = f.plan.first().expect("installment plan");
            let lead = if f.changes.is_empty() { "Use".to_string() } else { format!("{}, then use", changes_clause(f)) };
            format!(
                "{lead} {} installments of {}, starting {}. This leaves at least {min} available.",
                f.plan.len(),
                money(first.amount),
                long_date(first.date)
            )
        }
        PaymentMethod::PartialPayment => {
            let (a, b) = (f.plan[0], f.plan[1]);
            format!(
                "Pay {} today and the remaining {} on {}. This completes the full request and keeps the {min} minimum protected.",
                money(a.amount),
                money(b.amount),
                long_date(b.date)
            )
        }
        PaymentMethod::Wait => {
            let e = f.plan[0].date;
            if e == f.desired_completion_date {
                format!("Pay {req} in full on {}. Paying earlier would take the balance below the {min} minimum.", long_date(e))
            } else {
                format!("Wait until {}, then pay {req} in full. Paying sooner would put the {min} minimum at risk.", long_date(e))
            }
        }
        PaymentMethod::NotRecommended => {
            // Variant B iff methods == {partial_payment}, partial allowed, safe > 0, no E [FIT].
            let only_partial = f.accepted_methods == [PaymentMethod::PartialPayment];
            if only_partial && f.allows_partial_payment && f.safe_amount > Money::ZERO && f.earliest_full_date.is_none() {
                format!(
                    "Do not proceed with the {req} request. Although {} is available today, the full amount cannot be completed safely within 90 days.",
                    money(f.safe_amount)
                )
            } else {
                format!(
                    "Do not make this payment by {}. None of the available options keeps the {min} minimum protected.",
                    long_date(f.desired_completion_date)
                )
            }
        }
    }
}

/// `Stop the online backup subscription and reduce the streaming subscription to USD 23.50`
fn changes_clause(f: &DecisionFacts) -> String {
    let parts: Vec<String> = f
        .changes
        .iter()
        .map(|c| {
            let what = lower_first(&c.description);
            match c.new_amount {
                None => format!("stop the {what}"),
                Some(a) => format!("reduce the {what} to {} {}", f.currency, a.fmt_grouped()),
            }
        })
        .collect();
    let joined = match parts.len() {
        0 => String::new(),
        1 => parts[0].clone(),
        n => format!("{} and {}", parts[..n - 1].join(", "), parts[n - 1]),
    };
    upper_first(&joined)
}

fn upper_first(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(first) => first.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

fn lower_first(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(first) => first.to_lowercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

/// `15 November 2019`
pub fn long_date(d: NaiveDate) -> String {
    const MONTHS: [&str; 12] = [
        "January", "February", "March", "April", "May", "June", "July", "August", "September", "October",
        "November", "December",
    ];
    format!("{} {} {}", d.day(), MONTHS[d.month0() as usize], d.year())
}
