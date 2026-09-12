//! Deterministic explanation templates, filled only from `DecisionFacts` (0 model tokens).
//!
//! Wording follows the sample outputs. PROVISIONAL until the analyst's template slice lands
//! in RULES.md (in particular which variant each status uses).

use chrono::{Datelike, NaiveDate};

use super::facts::DecisionFacts;
use super::money::Cents;
use super::types::PaymentMethod;

pub fn render(f: &DecisionFacts) -> String {
    let cur = &f.currency;
    let money = |c: Cents| format!("{cur} {}", c.fmt_grouped());
    let min = money(f.minimum_balance);
    match f.method {
        PaymentMethod::FullPayment if f.changes.is_empty() => format!(
            "Pay {} today. This leaves at least {min} available over the next 90 days.",
            money(f.requested_amount)
        ),
        PaymentMethod::FullPayment => format!(
            "{}, then pay {} today. This leaves at least {min} available.",
            changes_clause(f),
            money(f.requested_amount)
        ),
        PaymentMethod::Installments => {
            let first = f.plan.first().expect("installment plan");
            let lead = if f.changes.is_empty() { String::new() } else { format!("{}, then use", changes_clause(f)) };
            let verb = if lead.is_empty() { "Use".to_string() } else { lead };
            format!(
                "{verb} {} installments of {}, starting {}. This leaves at least {min} available.",
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
            let p = f.plan[0];
            format!(
                "Pay {} in full on {}. Paying earlier would take the balance below the {min} minimum.",
                money(p.amount),
                long_date(p.date)
            )
        }
        PaymentMethod::NotRecommended => {
            if f.safe_amount > Cents::ZERO && f.earliest_full_date.is_none() {
                format!(
                    "Do not proceed with the {} request. Although {} is available today, the full amount cannot be completed safely within 90 days.",
                    money(f.requested_amount),
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

/// "Stop the online backup subscription and reduce the streaming subscription to USD 23.50"
fn changes_clause(f: &DecisionFacts) -> String {
    let parts: Vec<String> = f
        .changes
        .iter()
        .map(|c| {
            let what = c.description.to_lowercase();
            match c.new_amount {
                None => format!("stop the {what}"),
                Some(a) => format!("reduce the {what} to {} {}", f.currency, a.fmt_plan()),
            }
        })
        .collect();
    let joined = match parts.len() {
        0 => String::new(),
        1 => parts[0].clone(),
        2 => format!("{} and {}", parts[0], parts[1]),
        _ => format!("{}, and {}", parts[..parts.len() - 1].join(", "), parts[parts.len() - 1]),
    };
    capitalize(&joined)
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(first) => first.to_uppercase().collect::<String>() + c.as_str(),
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
