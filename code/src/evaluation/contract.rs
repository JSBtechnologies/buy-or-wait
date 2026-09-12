//! Output-contract validator (INVARIANTS.md sections A–E).
//!
//! `check_row` is shared by the file validator and the engine-facing invariant assertions, so the
//! rows the engine writes and the file we ship are held to exactly the same rules.

use std::collections::HashSet;
use std::fmt;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{Duration, NaiveDate};

use super::data::{fmt_cents_2dp, parse_cents, parse_date, Cents, Dataset, Profile, Request};

pub const HEADER: [&str; 8] = [
    "request_id",
    "amount_safe_to_pay",
    "affordability_status",
    "recommended_payment_method",
    "payment_plan",
    "earliest_date_for_full_payment",
    "spending_changes_needed",
    "decision_explanation",
];

pub const STATUSES: [&str; 4] =
    ["affordable_now", "affordable_with_plan", "affordable_later", "not_affordable"];
pub const METHODS: [&str; 5] =
    ["full_payment", "partial_payment", "installments", "wait", "not_recommended"];
pub const HORIZON_DAYS: i64 = 90;

/// Latest date a forecast may reach: the spec's 90 days or RULES.md S3.1's last day of
/// month(rd)+2 (up to 91 days), whichever is later.
pub fn horizon_end(rd: NaiveDate) -> NaiveDate {
    use chrono::Datelike;
    let months = rd.year() * 12 + rd.month0() as i32 + 3;
    let first_after = NaiveDate::from_ymd_opt(months.div_euclid(12), months.rem_euclid(12) as u32 + 1, 1)
        .expect("valid month");
    (first_after - Duration::days(1)).max(rd + Duration::days(HORIZON_DAYS))
}
pub const MAX_CHANGES: usize = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Error,
    Warn,
}

#[derive(Clone, Debug)]
pub struct Finding {
    pub request_id: String,
    pub severity: Severity,
    pub code: &'static str,
    pub detail: String,
}

impl fmt::Display for Finding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let sev = match self.severity {
            Severity::Error => "ERROR",
            Severity::Warn => "warn",
        };
        write!(f, "{sev} {} [{}] {}", self.request_id, self.code, self.detail)
    }
}

/// One output row exactly as it is (or will be) written to output.csv.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OutputRow {
    pub request_id: String,
    pub amount_safe_to_pay: String,
    pub affordability_status: String,
    pub recommended_payment_method: String,
    pub payment_plan: String,
    pub earliest_date_for_full_payment: String,
    pub spending_changes_needed: String,
    pub decision_explanation: String,
}

impl OutputRow {
    pub fn from_fields<S: AsRef<str>>(f: &[S]) -> OutputRow {
        let g = |i: usize| f.get(i).map(|s| s.as_ref().to_string()).unwrap_or_default();
        OutputRow {
            request_id: g(0),
            amount_safe_to_pay: g(1),
            affordability_status: g(2),
            recommended_payment_method: g(3),
            payment_plan: g(4),
            earliest_date_for_full_payment: g(5),
            spending_changes_needed: g(6),
            decision_explanation: g(7),
        }
    }

    pub fn fields(&self) -> [&str; 8] {
        [
            &self.request_id,
            &self.amount_safe_to_pay,
            &self.affordability_status,
            &self.recommended_payment_method,
            &self.payment_plan,
            &self.earliest_date_for_full_payment,
            &self.spending_changes_needed,
            &self.decision_explanation,
        ]
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Payment {
    pub date: NaiveDate,
    pub amount: Cents,
}

/// `none` → empty vec. Errors describe the first malformed entry.
pub fn parse_plan(s: &str) -> Result<Vec<Payment>, String> {
    if s == "none" {
        return Ok(Vec::new());
    }
    s.split('|')
        .map(|entry| {
            let (d, a) = entry
                .split_once(':')
                .ok_or_else(|| format!("plan entry {entry:?} is not DATE:AMOUNT"))?;
            let date = parse_date(d).map_err(|e| e.to_string())?;
            let amount = parse_cents(a).map_err(|e| e.to_string())?;
            Ok(Payment { date, amount })
        })
        .collect()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Change {
    Stop(String),
    ReduceTo(String, Cents),
}

impl Change {
    pub fn event_id(&self) -> &str {
        match self {
            Change::Stop(e) | Change::ReduceTo(e, _) => e,
        }
    }
}

pub fn parse_changes(s: &str) -> Result<Vec<Change>, String> {
    if s == "none" {
        return Ok(Vec::new());
    }
    s.split('|')
        .map(|item| {
            let parts: Vec<&str> = item.split(':').collect();
            match parts.as_slice() {
                ["stop", ev] if !ev.is_empty() => Ok(Change::Stop(ev.to_string())),
                ["reduce_to", ev, amt] if !ev.is_empty() => Ok(Change::ReduceTo(
                    ev.to_string(),
                    parse_cents(amt).map_err(|e| e.to_string())?,
                )),
                _ => Err(format!("change {item:?} is not stop:<event_id> or reduce_to:<event_id>:<amount>")),
            }
        })
        .collect()
}

struct Sink<'a> {
    id: &'a str,
    out: Vec<Finding>,
}

impl Sink<'_> {
    fn err(&mut self, code: &'static str, detail: impl Into<String>) {
        self.out.push(Finding {
            request_id: self.id.to_string(),
            severity: Severity::Error,
            code,
            detail: detail.into(),
        });
    }
    fn warn(&mut self, code: &'static str, detail: impl Into<String>) {
        self.out.push(Finding {
            request_id: self.id.to_string(),
            severity: Severity::Warn,
            code,
            detail: detail.into(),
        });
    }
}

/// All per-row contract rules. Never mutates or "fixes" anything.
pub fn check_row(ds: &Dataset, row: &OutputRow) -> Vec<Finding> {
    let mut s = Sink { id: &row.request_id, out: Vec::new() };
    let Some(req) = ds.request(&row.request_id) else {
        s.err("A2_unknown_request", format!("{} is not in the requests file", row.request_id));
        return s.out;
    };
    let Some(profile) = ds.profiles.get(&req.user_id) else {
        s.err("X_missing_profile", format!("no profile for {}", req.user_id));
        return s.out;
    };
    for (i, f) in row.fields().iter().enumerate() {
        if f.contains('\n') || f.contains('\r') {
            let sev_code = if i == 7 { "B6_newline" } else { "A3_newline" };
            if i == 7 {
                s.warn(sev_code, format!("{} contains a line break", HEADER[i]));
            } else {
                s.err(sev_code, format!("{} contains a line break", HEADER[i]));
            }
        }
    }

    // B1 amount
    let amount = match parse_cents(&row.amount_safe_to_pay) {
        Ok(a) => {
            if a < 0 || a > req.requested {
                s.err(
                    "B1_amount_range",
                    format!("amount_safe_to_pay {} outside [0, {}]", row.amount_safe_to_pay, fmt_cents_2dp(req.requested)),
                );
            }
            Some(a)
        }
        Err(e) => {
            s.err("B1_amount_format", e.to_string());
            None
        }
    };

    // B2/B3 enums
    let status = row.affordability_status.as_str();
    let method = row.recommended_payment_method.as_str();
    let status_ok = STATUSES.contains(&status);
    let method_ok = METHODS.contains(&method);
    if !status_ok {
        s.err("B2_status_enum", format!("affordability_status {status:?}"));
    }
    if !method_ok {
        s.err("B3_method_enum", format!("recommended_payment_method {method:?}"));
    }

    // B4 earliest date
    let earliest = if row.earliest_date_for_full_payment.is_empty() {
        None
    } else {
        match parse_date(&row.earliest_date_for_full_payment) {
            Ok(d) => {
                let horizon = horizon_end(req.request_date);
                if d < req.request_date || d > horizon {
                    s.err("B4_earliest_window", format!("earliest {d} outside [{}, {horizon}]", req.request_date));
                }
                Some(d)
            }
            Err(e) => {
                s.err("B4_earliest_format", e.to_string());
                None
            }
        }
    };

    // B5 safe-today ⇔ earliest == request_date
    if let Some(a) = amount {
        let full_today = a == req.requested;
        let earliest_today = earliest == Some(req.request_date);
        if full_today != earliest_today {
            s.err(
                "B5_amount_vs_earliest",
                format!(
                    "amount_safe_to_pay {} {} requested but earliest is {:?} (request_date {})",
                    row.amount_safe_to_pay,
                    if full_today { "==" } else { "<" },
                    row.earliest_date_for_full_payment,
                    req.request_date
                ),
            );
        }
    }

    // B6 explanation
    if row.decision_explanation.trim().is_empty() {
        s.err("B6_explanation_empty", "decision_explanation is empty");
    } else if row.decision_explanation.len() > 600 {
        s.warn("B6_explanation_long", format!("{} chars", row.decision_explanation.len()));
    }

    // D1 plan syntax
    let plan = match parse_plan(&row.payment_plan) {
        Ok(p) => {
            for w in p.windows(2) {
                if w[1].date <= w[0].date {
                    s.err("D1_plan_order", format!("plan dates not strictly increasing: {} then {}", w[0].date, w[1].date));
                }
            }
            for pay in &p {
                if pay.amount <= 0 {
                    s.err("D1_plan_amount", format!("non-positive payment on {}", pay.date));
                }
                if pay.date < req.request_date {
                    s.err("D1_plan_before_request", format!("payment on {} before request_date", pay.date));
                }
            }
            Some(p)
        }
        Err(e) => {
            s.err("D1_plan_format", e);
            None
        }
    };

    // E spending changes
    let changes = match parse_changes(&row.spending_changes_needed) {
        Ok(c) => {
            check_changes(&mut s, ds, req, profile, &c);
            Some(c)
        }
        Err(e) => {
            s.err("E1_changes_format", e);
            None
        }
    };
    let has_changes = changes.as_ref().map(|c| !c.is_empty()).unwrap_or(false);
    if has_changes && status_ok && status != "affordable_with_plan" {
        s.err("E7_changes_status", format!("spending changes with status {status}"));
    }

    // C status ↔ method
    if status_ok && method_ok {
        let ok = match status {
            "affordable_now" => method == "full_payment",
            "affordable_with_plan" => matches!(method, "partial_payment" | "installments" | "full_payment"),
            "affordable_later" => method == "wait",
            _ => method == "not_recommended",
        };
        if !ok {
            s.err("C_status_method", format!("status {status} with method {method}"));
        }
    }

    let (Some(plan), Some(amount)) = (plan, amount) else { return s.out };
    let total: Cents = plan.iter().map(|p| p.amount).sum();
    let single_today = plan.len() == 1 && plan[0].date == req.request_date && plan[0].amount == req.requested;

    match method {
        "full_payment" => {
            accepts(&mut s, profile, "full_payment");
            if !single_today {
                s.err("D2_full_plan", format!("full_payment plan must be {}:{}", req.request_date, fmt_cents_2dp(req.requested)));
            }
            if status == "affordable_now" {
                if amount != req.requested {
                    s.err("D3_now_amount", "affordable_now but amount_safe_to_pay < requested_amount");
                }
                if earliest != Some(req.request_date) {
                    s.err("D3_now_earliest", "affordable_now but earliest != request_date");
                }
                if has_changes {
                    s.err("D3_now_changes", "affordable_now must not need spending changes");
                }
            } else if status == "affordable_with_plan" && !has_changes {
                s.err("D4_full_with_plan_no_changes", "full_payment with affordable_with_plan needs spending changes");
            }
        }
        "partial_payment" => {
            accepts(&mut s, profile, "partial_payment");
            if !req.allows_partial_payment {
                s.err("D5_partial_not_allowed", "request does not allow partial payment");
            }
            if !(0 < amount && amount < req.requested) {
                s.err("D5_partial_amount", "partial_payment needs 0 < amount_safe_to_pay < requested_amount");
            }
            match earliest {
                None => s.err("D5_partial_no_earliest", "partial_payment needs earliest_date_for_full_payment"),
                Some(e) if e > req.desired_completion_date => s.err(
                    "D5_partial_deadline",
                    format!("second payment {e} after desired_completion_date {}", req.desired_completion_date),
                ),
                _ => {}
            }
            let expected = earliest.map(|e| {
                vec![
                    Payment { date: req.request_date, amount },
                    Payment { date: e, amount: req.requested - amount },
                ]
            });
            if plan.len() != 2 || Some(&plan) != expected.as_ref() {
                s.err(
                    "D5_partial_plan",
                    format!(
                        "partial plan must be {}:{}|{}:{}",
                        req.request_date,
                        fmt_cents_2dp(amount),
                        row.earliest_date_for_full_payment,
                        fmt_cents_2dp(req.requested - amount)
                    ),
                );
            }
            if total != req.requested {
                s.err("D5_partial_sum", format!("plan sums to {} not {}", fmt_cents_2dp(total), fmt_cents_2dp(req.requested)));
            }
        }
        "installments" => {
            accepts(&mut s, profile, "installments");
            let opts = ds.options.get(&req.request_id).map(Vec::as_slice).unwrap_or(&[]);
            let got: Vec<(NaiveDate, Cents)> = plan.iter().map(|p| (p.date, p.amount)).collect();
            match opts.iter().find(|o| o.method == "installments" && o.schedule() == got) {
                None => s.err("D6_installment_no_option", "installment plan matches no supplied installments option"),
                Some(o) => {
                    match profile.max_installment_months {
                        None => s.err("D6_installment_max_blank", "user has blank max_installment_months"),
                        Some(m) if o.number_of_payments > m => s.err(
                            "D6_installment_max",
                            format!("{} has {} payments > max_installment_months {m}", o.payment_option_id, o.number_of_payments),
                        ),
                        _ => {}
                    }
                    if let Some(last) = plan.last() {
                        if last.date > req.desired_completion_date {
                            s.warn("D6_installment_deadline", format!("last installment {} after desired date", last.date));
                        }
                    }
                }
            }
        }
        "wait" => {
            accepts(&mut s, profile, "full_payment");
            match earliest {
                Some(e) if e > req.request_date => {
                    if plan.len() != 1 || plan[0].date != e || plan[0].amount != req.requested {
                        s.err("D7_wait_plan", format!("wait plan must be {e}:{}", fmt_cents_2dp(req.requested)));
                    }
                    if e > req.desired_completion_date {
                        s.warn("D7_wait_deadline", format!("wait date {e} after desired date {}", req.desired_completion_date));
                    }
                }
                _ => s.err("D7_wait_earliest", "wait needs earliest_date_for_full_payment after request_date"),
            }
            if has_changes {
                s.err("D7_wait_changes", "wait must not carry spending changes");
            }
        }
        "not_recommended" => {
            if !plan.is_empty() {
                s.err("D8_notrec_plan", "not_recommended must have payment_plan none");
            }
            if has_changes {
                s.err("D8_notrec_changes", "not_recommended must have spending_changes_needed none");
            }
            if earliest.is_some() {
                s.warn("D8_notrec_earliest", "not_recommended with an earliest date (all samples leave it blank)");
            }
        }
        _ => {}
    }
    if method != "not_recommended" && plan.is_empty() {
        s.err("D1_plan_none", format!("{method} with payment_plan none"));
    }
    explanation_grounding(&mut s, row, &plan);
    s.out
}

fn accepts(s: &mut Sink, profile: &Profile, method: &str) {
    if !profile.accepts(method) {
        s.err("D_method_not_accepted", format!("user will not consider {method} ({})", profile.methods.join("|")));
    }
}

fn check_changes(s: &mut Sink, ds: &Dataset, req: &Request, profile: &Profile, changes: &[Change]) {
    if changes.len() > MAX_CHANGES {
        s.err("E1_changes_count", format!("{} changes > {MAX_CHANGES}", changes.len()));
    }
    let mut seen = HashSet::new();
    for c in changes {
        let id = c.event_id();
        if !seen.insert(id.to_string()) {
            s.err("E6_changes_same_event", format!("{id} referenced more than once"));
        }
        let Some(ev) = ds.events.get(id) else {
            s.err("E2_changes_unknown_event", format!("{id} does not exist"));
            continue;
        };
        if ev.user_id != req.user_id {
            s.err("E2_changes_other_user", format!("{id} belongs to {}", ev.user_id));
            continue;
        }
        if ev.direction != "debit" {
            s.err("E2_changes_not_debit", format!("{id} is {}", ev.direction));
        }
        if ev.event_date > req.request_date {
            s.warn("E2_changes_future_event", format!("{id} dated {} after request_date", ev.event_date));
        }
        if profile.protect.contains(&ev.category) {
            s.err("E3_changes_protected", format!("{id} category {} is protected", ev.category));
        }
        match c {
            Change::Stop(_) => {
                if !matches!(ev.flexibility.as_str(), "stoppable" | "reducible_or_stoppable") {
                    s.err("E4_stop_flexibility", format!("{id} flexibility {}", ev.flexibility));
                }
                if !profile.willing_to_stop.contains(&ev.category) {
                    s.err("E4_stop_category", format!("{id} category {} not in willing_to_stop", ev.category));
                }
            }
            Change::ReduceTo(_, new) => {
                if !matches!(ev.flexibility.as_str(), "reducible" | "reducible_or_stoppable") {
                    s.err("E5_reduce_flexibility", format!("{id} flexibility {}", ev.flexibility));
                }
                if !profile.willing_to_reduce.contains(&ev.category) {
                    s.err("E5_reduce_category", format!("{id} category {} not in willing_to_reduce", ev.category));
                }
                match ev.minimum_allowed_amount {
                    None => s.err("E5_reduce_no_minimum", format!("{id} has no minimum_allowed_amount")),
                    Some(m) if *new < m => s.err(
                        "E5_reduce_below_minimum",
                        format!("{id} reduce_to {} < minimum_allowed_amount {}", fmt_cents_2dp(*new), fmt_cents_2dp(m)),
                    ),
                    _ => {}
                }
                if let Some(a) = ev.amount {
                    if *new >= a {
                        s.err("E5_reduce_not_lower", format!("{id} reduce_to {} >= current {}", fmt_cents_2dp(*new), fmt_cents_2dp(a)));
                    }
                }
                if ev.currency != profile.home_currency {
                    s.warn("E5_reduce_foreign", format!("{id} is in {} (home {})", ev.currency, profile.home_currency));
                }
            }
        }
        // W: should reference the latest occurrence of the stream before the request.
        if let Some(ids) = ds.events_by_user.get(&req.user_id) {
            let latest = ids
                .iter()
                .filter_map(|x| ds.events.get(x))
                .filter(|o| o.description == ev.description && o.category == ev.category && o.event_date <= req.request_date)
                .max_by_key(|o| o.event_date);
            if let Some(l) = latest {
                if l.event_date > ev.event_date {
                    s.warn("E_changes_not_latest", format!("{id} ({}) is older than {} ({})", ev.event_date, l.event_id, l.event_date));
                }
            }
        }
    }
}

/// W: the explanation should mention at least one of the plan's amounts (as written, with grouping)
/// — a cheap guard against an explanation rendered from a different plan.
fn explanation_grounding(s: &mut Sink, row: &OutputRow, plan: &[Payment]) {
    if plan.is_empty() {
        return;
    }
    let text: String = row.decision_explanation.chars().filter(|c| *c != ',').collect();
    let mentions = plan.iter().any(|p| {
        let v = fmt_cents_2dp(p.amount);
        text.contains(&v) || text.contains(v.trim_end_matches('0').trim_end_matches('.'))
    });
    if !mentions {
        s.warn("B6_explanation_ungrounded", "explanation mentions none of the plan amounts");
    }
}

#[derive(Debug, Default)]
pub struct Report {
    pub rows: usize,
    pub findings: Vec<Finding>,
}

impl Report {
    pub fn errors(&self) -> impl Iterator<Item = &Finding> {
        self.findings.iter().filter(|f| f.severity == Severity::Error)
    }
    pub fn warnings(&self) -> impl Iterator<Item = &Finding> {
        self.findings.iter().filter(|f| f.severity == Severity::Warn)
    }
    pub fn passed(&self) -> bool {
        self.errors().next().is_none()
    }
    pub fn render(&self) -> String {
        let mut out = String::new();
        let mut sorted: Vec<&Finding> = self.findings.iter().collect();
        sorted.sort_by(|a, b| (a.severity, &a.request_id, a.code).cmp(&(b.severity, &b.request_id, b.code)));
        for f in sorted {
            out.push_str(&f.to_string());
            out.push('\n');
        }
        out.push_str(&format!(
            "contract: {} rows, {} errors, {} warnings -> {}\n",
            self.rows,
            self.errors().count(),
            self.warnings().count(),
            if self.passed() { "PASS" } else { "FAIL" }
        ));
        out
    }
}

/// Validate a whole output.csv against the requests file the dataset was loaded with.
pub fn validate_rows(ds: &Dataset, header: &[String], rows: &[OutputRow]) -> Report {
    let mut rep = Report { rows: rows.len(), findings: Vec::new() };
    let file_err = |code: &'static str, detail: String| Finding {
        request_id: "-".into(),
        severity: Severity::Error,
        code,
        detail,
    };
    if header.len() != HEADER.len() || header.iter().zip(HEADER).any(|(h, e)| h != e) {
        rep.findings.push(file_err("A1_header", format!("header {header:?}")));
    }
    let mut seen = HashSet::new();
    for r in rows {
        if !seen.insert(r.request_id.clone()) {
            rep.findings.push(file_err("A2_duplicate", format!("{} appears more than once", r.request_id)));
        }
    }
    for q in &ds.requests {
        if !seen.contains(&q.request_id) {
            rep.findings.push(file_err("A2_missing", format!("{} has no output row", q.request_id)));
        }
    }
    let order_matches = rows.len() == ds.requests.len()
        && rows.iter().zip(&ds.requests).all(|(r, q)| r.request_id == q.request_id);
    if !order_matches && rep.findings.is_empty() {
        rep.findings.push(Finding {
            request_id: "-".into(),
            severity: Severity::Warn,
            code: "A2_order",
            detail: "row order differs from the requests file".into(),
        });
    }
    for r in rows {
        rep.findings.extend(check_row(ds, r));
    }
    rep
}

pub fn read_output(path: &Path) -> Result<(Vec<String>, Vec<OutputRow>)> {
    let mut rdr = csv::ReaderBuilder::new()
        .has_headers(true)
        .flexible(false)
        .from_path(path)
        .with_context(|| format!("open {}", path.display()))?;
    let header = rdr
        .headers()?
        .iter()
        .map(|h| h.trim_start_matches('\u{feff}').to_string())
        .collect();
    let mut rows = Vec::new();
    for rec in rdr.records() {
        let rec = rec.with_context(|| format!("malformed CSV in {}", path.display()))?;
        rows.push(OutputRow::from_fields(&rec.iter().collect::<Vec<_>>()));
    }
    Ok((header, rows))
}

pub fn validate_file(ds: &Dataset, path: &Path) -> Result<Report> {
    let (header, rows) = read_output(path)?;
    Ok(validate_rows(ds, &header, &rows))
}
