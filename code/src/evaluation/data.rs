//! Independent dataset loading for verification.
//!
//! Deliberately does not reuse `crate::model`: a parsing bug shared by the engine and the
//! verifier would be invisible. Only the columns the checks need are read. Money is held as
//! integer cents (every amount in the dataset has at most 2 decimal places).

use std::collections::HashMap;
use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use chrono::NaiveDate;

pub type Cents = i64;

/// Strict decimal parse: optional `-`, digits, optional `.` with 1–2 digits.
pub fn parse_cents(s: &str) -> Result<Cents> {
    let t = s.trim();
    if t != s || t.is_empty() {
        bail!("not a plain decimal: {s:?}");
    }
    let (neg, body) = match t.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, t),
    };
    let (int, frac) = match body.split_once('.') {
        Some((i, f)) => (i, f),
        None => (body, ""),
    };
    let digits = |p: &str| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit());
    if !digits(int) || (body.contains('.') && !digits(frac)) || frac.len() > 2 {
        bail!("not a decimal with at most 2 dp: {s:?}");
    }
    let whole: i64 = int.parse().map_err(|_| anyhow!("amount out of range: {s:?}"))?;
    let mut cents = match frac.len() {
        0 => 0,
        1 => frac.parse::<i64>()? * 10,
        _ => frac.parse::<i64>()?,
    };
    cents += whole
        .checked_mul(100)
        .ok_or_else(|| anyhow!("amount out of range: {s:?}"))?;
    Ok(if neg { -cents } else { cents })
}

/// Strict `YYYY-MM-DD`.
pub fn parse_date(s: &str) -> Result<NaiveDate> {
    if s.len() != 10 {
        bail!("not YYYY-MM-DD: {s:?}");
    }
    NaiveDate::parse_from_str(s, "%Y-%m-%d").with_context(|| format!("not YYYY-MM-DD: {s:?}"))
}

/// `620.40`, `10840`: integer when whole, else exactly 2 dp (plan and reduce_to style in the samples).
pub fn fmt_cents_2dp(c: Cents) -> String {
    let sign = if c < 0 { "-" } else { "" };
    let a = c.abs();
    if a % 100 == 0 {
        format!("{sign}{}", a / 100)
    } else {
        format!("{sign}{}.{:02}", a / 100, a % 100)
    }
}

/// `603.3`, `8401800`: shortest form (amount_safe_to_pay style in the samples).
pub fn fmt_cents_short(c: Cents) -> String {
    let s = fmt_cents_2dp(c);
    if s.contains('.') && s.ends_with('0') {
        s[..s.len() - 1].to_string()
    } else {
        s
    }
}

pub fn cents_to_f64(c: Cents) -> f64 {
    c as f64 / 100.0
}

fn split_list(s: &str) -> Vec<String> {
    s.split('|').map(str::trim).filter(|x| !x.is_empty()).map(String::from).collect()
}

struct Table {
    index: HashMap<String, usize>,
    rows: Vec<csv::StringRecord>,
    name: String,
}

impl Table {
    fn read(path: &Path) -> Result<Table> {
        let mut rdr = csv::ReaderBuilder::new()
            .has_headers(true)
            .from_path(path)
            .with_context(|| format!("open {}", path.display()))?;
        let index = rdr
            .headers()?
            .iter()
            .enumerate()
            .map(|(i, h)| (h.trim_start_matches('\u{feff}').to_string(), i))
            .collect();
        let rows = rdr.records().collect::<Result<Vec<_>, _>>()?;
        Ok(Table { index, rows, name: path.display().to_string() })
    }

    fn get<'r>(&self, row: &'r csv::StringRecord, col: &str) -> Result<&'r str> {
        let i = *self
            .index
            .get(col)
            .ok_or_else(|| anyhow!("{}: missing column {col}", self.name))?;
        Ok(row.get(i).unwrap_or(""))
    }
}

#[derive(Clone, Debug)]
pub struct Request {
    pub request_id: String,
    pub user_id: String,
    pub request_date: NaiveDate,
    pub requested: Cents,
    pub desired_completion_date: NaiveDate,
    pub allows_partial_payment: bool,
}

#[derive(Clone, Debug)]
pub struct Profile {
    pub user_id: String,
    pub home_currency: String,
    pub balance: Cents,
    pub minimum: Cents,
    pub protect: Vec<String>,
    pub willing_to_reduce: Vec<String>,
    pub willing_to_stop: Vec<String>,
    pub methods: Vec<String>,
    pub max_installment_months: Option<u32>,
}

impl Profile {
    pub fn accepts(&self, method: &str) -> bool {
        self.methods.iter().any(|m| m == method)
    }
}

#[derive(Clone, Debug)]
pub struct Event {
    pub event_id: String,
    pub user_id: String,
    pub event_type: String,
    pub description: String,
    pub category: String,
    pub direction: String,
    pub amount: Option<Cents>,
    pub currency: String,
    pub event_date: NaiveDate,
    pub status: String,
    pub flexibility: String,
    pub minimum_allowed_amount: Option<Cents>,
}

#[derive(Clone, Debug)]
pub struct PayOption {
    pub payment_option_id: String,
    pub request_id: String,
    pub method: String,
    pub payment_amount: Cents,
    pub number_of_payments: u32,
    pub first_payment_date: NaiveDate,
    pub frequency_days: Option<u32>,
    pub total_payable: Cents,
}

impl PayOption {
    /// The exact schedule an installment plan must reproduce.
    pub fn schedule(&self) -> Vec<(NaiveDate, Cents)> {
        let step = self.frequency_days.unwrap_or(0) as i64;
        (0..self.number_of_payments as i64)
            .map(|k| (self.first_payment_date + chrono::Duration::days(k * step), self.payment_amount))
            .collect()
    }
}

/// Labels carried by `sample_requests.csv` (absent for `requests.csv`).
pub type LabelRow = super::contract::OutputRow;

pub struct Dataset {
    pub requests: Vec<Request>,
    pub request_index: HashMap<String, usize>,
    pub profiles: HashMap<String, Profile>,
    pub events: HashMap<String, Event>,
    pub events_by_user: HashMap<String, Vec<String>>,
    pub options: HashMap<String, Vec<PayOption>>,
    /// Filled only when the requests file has the output columns (sample_requests.csv).
    pub labels: Vec<LabelRow>,
}

impl Dataset {
    /// `dataset_dir` holds profiles/events/options; `requests_file` is `requests.csv` or `sample_requests.csv`.
    pub fn load(dataset_dir: &Path, requests_file: &Path) -> Result<Dataset> {
        let rq = Table::read(requests_file)?;
        let mut requests = Vec::new();
        let mut labels = Vec::new();
        let has_labels = rq.index.contains_key("affordability_status");
        for r in &rq.rows {
            let id = rq.get(r, "request_id")?.to_string();
            let ctx = || format!("{}: {id}", rq.name);
            requests.push(Request {
                request_id: id.clone(),
                user_id: rq.get(r, "user_id")?.to_string(),
                request_date: parse_date(rq.get(r, "request_date")?).with_context(ctx)?,
                requested: parse_cents(rq.get(r, "requested_amount")?).with_context(ctx)?,
                desired_completion_date: parse_date(rq.get(r, "desired_completion_date")?)
                    .with_context(ctx)?,
                allows_partial_payment: match rq.get(r, "allows_partial_payment")? {
                    "true" => true,
                    "false" => false,
                    other => bail!("{}: allows_partial_payment={other:?}", ctx()),
                },
            });
            if has_labels {
                let mut fields = Vec::new();
                for col in super::contract::HEADER {
                    fields.push(rq.get(r, col)?.to_string());
                }
                labels.push(super::contract::OutputRow::from_fields(&fields));
            }
        }
        let request_index = requests
            .iter()
            .enumerate()
            .map(|(i, r)| (r.request_id.clone(), i))
            .collect();

        let pf = Table::read(&dataset_dir.join("financial_profiles.csv"))?;
        let mut profiles = HashMap::new();
        for r in &pf.rows {
            let uid = pf.get(r, "user_id")?.to_string();
            let max = pf.get(r, "max_installment_months")?;
            profiles.insert(
                uid.clone(),
                Profile {
                    user_id: uid,
                    home_currency: pf.get(r, "home_currency")?.to_string(),
                    balance: parse_cents(pf.get(r, "current_available_balance")?)?,
                    minimum: parse_cents(pf.get(r, "minimum_balance_to_keep")?)?,
                    protect: split_list(pf.get(r, "expense_categories_to_protect")?),
                    willing_to_reduce: split_list(pf.get(r, "expense_categories_user_is_willing_to_reduce")?),
                    willing_to_stop: split_list(pf.get(r, "expense_categories_user_is_willing_to_stop")?),
                    methods: split_list(pf.get(r, "payment_methods_user_will_consider")?),
                    max_installment_months: if max.is_empty() { None } else { Some(max.parse()?) },
                },
            );
        }

        let ev = Table::read(&dataset_dir.join("financial_events.csv"))?;
        let mut events = HashMap::new();
        let mut events_by_user: HashMap<String, Vec<String>> = HashMap::new();
        for r in &ev.rows {
            let id = ev.get(r, "event_id")?.to_string();
            let amount = ev.get(r, "amount")?;
            let min_allowed = ev.get(r, "minimum_allowed_amount")?;
            let e = Event {
                event_id: id.clone(),
                user_id: ev.get(r, "user_id")?.to_string(),
                event_type: ev.get(r, "event_type")?.to_string(),
                description: ev.get(r, "description")?.to_string(),
                category: ev.get(r, "category")?.to_string(),
                direction: ev.get(r, "direction")?.to_string(),
                amount: if amount.is_empty() { None } else { Some(parse_cents(amount)?) },
                currency: ev.get(r, "currency")?.to_string(),
                event_date: parse_date(ev.get(r, "event_date")?)?,
                status: ev.get(r, "status")?.to_string(),
                flexibility: ev.get(r, "flexibility")?.to_string(),
                minimum_allowed_amount: if min_allowed.is_empty() {
                    None
                } else {
                    Some(parse_cents(min_allowed)?)
                },
            };
            events_by_user.entry(e.user_id.clone()).or_default().push(id.clone());
            events.insert(id, e);
        }

        let op = Table::read(&dataset_dir.join("request_payment_options.csv"))?;
        let mut options: HashMap<String, Vec<PayOption>> = HashMap::new();
        for r in &op.rows {
            let freq = op.get(r, "payment_frequency_days")?;
            let o = PayOption {
                payment_option_id: op.get(r, "payment_option_id")?.to_string(),
                request_id: op.get(r, "request_id")?.to_string(),
                method: op.get(r, "payment_method")?.to_string(),
                payment_amount: parse_cents(op.get(r, "payment_amount")?)?,
                number_of_payments: op.get(r, "number_of_payments")?.parse()?,
                first_payment_date: parse_date(op.get(r, "first_payment_date")?)?,
                frequency_days: if freq.is_empty() { None } else { Some(freq.parse()?) },
                total_payable: parse_cents(op.get(r, "total_payable_amount")?)?,
            };
            options.entry(o.request_id.clone()).or_default().push(o);
        }

        Ok(Dataset { requests, request_index, profiles, events, events_by_user, options, labels })
    }

    pub fn request(&self, request_id: &str) -> Option<&Request> {
        self.request_index.get(request_id).map(|&i| &self.requests[i])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cents_parse_and_format() {
        assert_eq!(parse_cents("620.4").unwrap(), 62040);
        assert_eq!(parse_cents("620.40").unwrap(), 62040);
        assert_eq!(parse_cents("17229139.2").unwrap(), 1722913920);
        assert_eq!(parse_cents("0").unwrap(), 0);
        assert_eq!(parse_cents("-1.5").unwrap(), -150);
        for bad in ["", " 1", "1.", ".5", "1.234", "1,000", "1e3", "NaN", "--1"] {
            assert!(parse_cents(bad).is_err(), "{bad}");
        }
        assert_eq!(fmt_cents_2dp(62040), "620.40");
        assert_eq!(fmt_cents_2dp(1084000), "10840");
        assert_eq!(fmt_cents_short(60330), "603.3");
        assert_eq!(fmt_cents_short(2350), "23.5");
    }

    #[test]
    fn strict_dates() {
        assert!(parse_date("2024-03-03").is_ok());
        assert!(parse_date("2024-3-3").is_err());
        assert!(parse_date("2024-02-30").is_err());
    }
}
