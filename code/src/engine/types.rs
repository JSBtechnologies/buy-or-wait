//! Engine-owned domain types. The only coupling to the shared CSV model (`crate::model`) is
//! the `from_model` adapters at the bottom, so engine logic never touches raw strings/floats.
//!
//! Note the absence of `user_id` on every type here: identity lives only in
//! [`super::session::Session`], which is constructed around exactly one user (PLAN.md §2.5).

use std::collections::BTreeMap;
use std::str::FromStr;

use anyhow::{anyhow, bail, Result};
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

use super::money::{Cents, DecimalRate};
use crate::model;

macro_rules! string_enum {
    ($name:ident { $($variant:ident = $s:literal),+ $(,)? }) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        pub enum $name { $($variant),+ }
        impl $name {
            pub fn as_str(self) -> &'static str {
                match self { $($name::$variant => $s),+ }
            }
        }
        impl FromStr for $name {
            type Err = anyhow::Error;
            fn from_str(s: &str) -> Result<Self> {
                match s.trim() {
                    $($s => Ok($name::$variant),)+
                    other => Err(anyhow!(concat!("unknown ", stringify!($name), ": {:?}"), other)),
                }
            }
        }
        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str(self.as_str()) }
        }
    };
}

string_enum!(EventType {
    Expense = "expense",
    Subscription = "subscription",
    Income = "income",
    DebtPayment = "debt_payment",
    InvestmentPurchase = "investment_purchase",
    InvestmentSale = "investment_sale",
    InvestmentValuation = "investment_valuation",
    Refund = "refund",
});

string_enum!(Direction { Debit = "debit", Credit = "credit", NonCash = "non_cash" });

string_enum!(Status {
    Settled = "settled",
    Pending = "pending",
    Scheduled = "scheduled",
    Cancelled = "cancelled",
    Failed = "failed",
    Unrealized = "unrealized",
});

string_enum!(Flexibility {
    Fixed = "fixed",
    Reducible = "reducible",
    Stoppable = "stoppable",
    ReducibleOrStoppable = "reducible_or_stoppable",
});

string_enum!(PaymentMethod {
    FullPayment = "full_payment",
    PartialPayment = "partial_payment",
    Installments = "installments",
    Wait = "wait",
    NotRecommended = "not_recommended",
});

string_enum!(AffordabilityStatus {
    AffordableNow = "affordable_now",
    AffordableWithPlan = "affordable_with_plan",
    AffordableLater = "affordable_later",
    NotAffordable = "not_affordable",
});

impl Flexibility {
    pub fn can_reduce(self) -> bool {
        matches!(self, Flexibility::Reducible | Flexibility::ReducibleOrStoppable)
    }
    pub fn can_stop(self) -> bool {
        matches!(self, Flexibility::Stoppable | Flexibility::ReducibleOrStoppable)
    }
}

/// One financial event row, typed. Amounts are in the event's own currency.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub id: String,
    pub event_type: EventType,
    pub description: String,
    pub category: String,
    pub direction: Direction,
    /// `None` when the row is blank: the figure must come from an image, never zero.
    pub amount: Option<Cents>,
    pub currency: String,
    pub event_date: NaiveDate,
    pub settlement_date: Option<NaiveDate>,
    pub status: Status,
    pub linked_event_id: Option<String>,
    pub flexibility: Flexibility,
    pub minimum_allowed_amount: Option<Cents>,
}

impl Event {
    /// The date the cash actually moves.
    pub fn cash_date(&self) -> NaiveDate {
        self.settlement_date.unwrap_or(self.event_date)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Profile {
    pub home_currency: String,
    pub current_available_balance: Cents,
    pub minimum_balance_to_keep: Cents,
    pub financial_priorities: Vec<String>,
    pub protected_categories: Vec<String>,
    pub reducible_categories: Vec<String>,
    pub stoppable_categories: Vec<String>,
    pub accepted_methods: Vec<PaymentMethod>,
    /// `None` when the user will not consider installments.
    pub max_installment_months: Option<u32>,
}

impl Profile {
    pub fn accepts(&self, m: PaymentMethod) -> bool {
        self.accepted_methods.contains(&m)
    }
    pub fn is_protected(&self, category: &str) -> bool {
        self.protected_categories.iter().any(|c| c == category)
    }
}

/// The request as intake produces it (PLAN.md §2.6): the same struct whether it came from
/// CSV columns (batch) or a model parse of free text (interactive).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RequestSpec {
    pub amount: Cents,
    pub deadline: NaiveDate,
    pub request_type: String,
    pub allows_partial_payment: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PaymentOption {
    pub id: String,
    pub method: PaymentMethod,
    pub payment_amount: Cents,
    pub number_of_payments: u32,
    pub first_payment_date: NaiveDate,
    pub payment_frequency_days: Option<u32>,
    pub financing_fee: Cents,
    pub total_payable_amount: Cents,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Payment {
    pub date: NaiveDate,
    pub amount: Cents,
}

impl PaymentOption {
    /// The option's exact schedule: `number_of_payments` of `payment_amount`, spaced
    /// `payment_frequency_days` apart from `first_payment_date`.
    pub fn schedule(&self) -> Vec<Payment> {
        let step = self.payment_frequency_days.unwrap_or(0) as i64;
        (0..self.number_of_payments as i64)
            .map(|k| Payment {
                date: self.first_payment_date + chrono::Duration::days(k * step),
                amount: self.payment_amount,
            })
            .collect()
    }

    /// Sort key for the "lowest payment_option_id" tie-breaker: numeric suffix, then text.
    pub fn id_rank(&self) -> (u64, String) {
        id_rank(&self.id)
    }
}

pub fn id_rank(id: &str) -> (u64, String) {
    let digits: String = id.chars().rev().take_while(|c| c.is_ascii_digit()).collect();
    let n = digits.chars().rev().collect::<String>().parse().unwrap_or(u64::MAX);
    (n, id.to_string())
}

/// Dated fixed rates, looked up by (date, from, to). Behind a trait so a live provider is a
/// drop-in extension; the dataset table is the only implementation used for the submission.
pub trait RateProvider {
    fn rate(&self, date: NaiveDate, from: &str, to: &str) -> Option<DecimalRate>;
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RateTable {
    rates: BTreeMap<(NaiveDate, String, String), DecimalRate>,
}

impl RateTable {
    pub fn insert(&mut self, date: NaiveDate, from: &str, to: &str, rate: DecimalRate) {
        self.rates.insert((date, from.to_string(), to.to_string()), rate);
    }
}

impl RateProvider for RateTable {
    fn rate(&self, date: NaiveDate, from: &str, to: &str) -> Option<DecimalRate> {
        self.rates.get(&(date, from.to_string(), to.to_string())).copied()
    }
}

// ---------------------------------------------------------------------------------------
// Adapters from the shared CSV model.
// ---------------------------------------------------------------------------------------

fn split_list(s: &str) -> Vec<String> {
    s.split('|').map(str::trim).filter(|x| !x.is_empty()).map(String::from).collect()
}

fn opt_str(s: &Option<String>) -> Option<String> {
    s.as_ref().map(|x| x.trim().to_string()).filter(|x| !x.is_empty())
}

impl Event {
    pub fn from_model(e: &model::FinancialEvent) -> Result<Event> {
        Ok(Event {
            id: e.event_id.clone(),
            event_type: e.event_type.parse()?,
            description: e.description.clone(),
            category: e.category.clone(),
            direction: e.direction.parse()?,
            amount: e.amount.map(Cents::from_f64),
            currency: e.currency.clone(),
            event_date: e.event_date,
            settlement_date: e.settlement_date,
            status: e.status.parse()?,
            linked_event_id: opt_str(&e.linked_event_id),
            flexibility: e.flexibility.parse()?,
            minimum_allowed_amount: e.minimum_allowed_amount.map(Cents::from_f64),
        })
    }
}

impl Profile {
    pub fn from_model(p: &model::FinancialProfile) -> Result<Profile> {
        let accepted_methods = split_list(&p.payment_methods_user_will_consider)
            .iter()
            .map(|m| m.parse())
            .collect::<Result<Vec<PaymentMethod>>>()?;
        Ok(Profile {
            home_currency: p.home_currency.clone(),
            current_available_balance: Cents::from_f64(p.current_available_balance),
            minimum_balance_to_keep: Cents::from_f64(p.minimum_balance_to_keep),
            financial_priorities: split_list(&p.financial_priorities),
            protected_categories: split_list(&p.expense_categories_to_protect),
            reducible_categories: split_list(&p.expense_categories_user_is_willing_to_reduce),
            stoppable_categories: split_list(&p.expense_categories_user_is_willing_to_stop),
            accepted_methods,
            max_installment_months: p.max_installment_months,
        })
    }
}

impl RequestSpec {
    /// Batch intake: the four fields straight from the CSV columns (0 tokens).
    pub fn from_model(r: &model::Request) -> RequestSpec {
        RequestSpec {
            amount: Cents::from_f64(r.requested_amount),
            deadline: r.desired_completion_date,
            request_type: r.request_type.clone(),
            allows_partial_payment: r.allows_partial_payment,
        }
    }
}

impl PaymentOption {
    pub fn from_model(o: &model::RequestPaymentOption) -> Result<PaymentOption> {
        let method: PaymentMethod = o.payment_method.parse()?;
        if !matches!(method, PaymentMethod::FullPayment | PaymentMethod::Installments) {
            bail!("{}: unexpected payment option method {method}", o.payment_option_id);
        }
        Ok(PaymentOption {
            id: o.payment_option_id.clone(),
            method,
            payment_amount: Cents::from_f64(o.payment_amount),
            number_of_payments: o.number_of_payments,
            first_payment_date: o.first_payment_date,
            payment_frequency_days: o.payment_frequency_days,
            financing_fee: Cents::from_f64(o.financing_fee),
            total_payable_amount: Cents::from_f64(o.total_payable_amount),
        })
    }
}

impl RateTable {
    pub fn from_model(rows: &[model::ExchangeRate]) -> RateTable {
        let mut t = RateTable::default();
        for r in rows {
            t.insert(r.rate_date, &r.from_currency, &r.to_currency, DecimalRate::from_f64(r.rate));
        }
        t
    }
}
