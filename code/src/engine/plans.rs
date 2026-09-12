//! Plan search and ranking (PLAN.md §2.8).
//!
//! Every candidate (full, each installment option, two-payment partial, wait, and
//! spending-change variants) is replayed against the forecast. Ineligible or unsafe
//! candidates are dropped with a recorded reason; survivors are sorted by a lexicographic
//! ranking key and the first one is the recommendation.

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

use super::forecast::{Forecast, ForecastInputs, SafetyReport, SpendingChange};
use super::money::Money;
use super::recurrence::{Occurrence, StreamKind, Streams};
use super::rules::{ChangePreference, Rules};
use super::types::{id_rank, Direction, Payment, PaymentMethod, PaymentOption, Profile, RequestSpec};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Candidate {
    pub method: PaymentMethod,
    pub payments: Vec<Payment>,
    pub option_id: Option<String>,
    pub changes: Vec<SpendingChange>,
    pub total_paid: Money,
}

impl Candidate {
    pub fn label(&self) -> String {
        let mut s = self.method.as_str().to_string();
        if let Some(id) = &self.option_id {
            s.push(':');
            s.push_str(id);
        }
        if !self.changes.is_empty() {
            let c: Vec<String> = self.changes.iter().map(|c| c.render()).collect();
            s.push_str(&format!("+[{}]", c.join("|")));
        }
        s
    }

    pub fn start(&self) -> Option<NaiveDate> {
        self.payments.first().map(|p| p.date)
    }

    pub fn completion(&self) -> Option<NaiveDate> {
        self.payments.last().map(|p| p.date)
    }
}

/// Lexicographic ranking key; derive(Ord) compares fields in declaration order.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct RankKey {
    /// 1. Complete the full request by desired_completion_date (false sorts first).
    pub misses_deadline: bool,
    /// 2. Require no spending changes; among change plans, per `Rules::change_preference`
    ///    either fewer changes (RULES S1.3) or the smallest spending cut.
    pub change_order: (usize, Money),
    /// 3. Minimize the total amount paid.
    pub total_paid: Money,
    /// 4. Start payment earlier.
    pub start: NaiveDate,
    /// 5. Use fewer payments.
    pub payment_count: usize,
    /// 6. Lowest payment_option_id; non-option plans sort first on a full tie (RULES S1.3).
    pub option_rank: Option<(u64, String)>,
    /// 7. Remaining change tie-breaks (the other of count/cut, then event ids).
    pub change_tail: (usize, Money, Vec<String>),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum DropReason {
    MethodNotAccepted,
    InstallmentsNotConsidered,
    ExceedsMaxInstallmentMonths { months: u32, max: u32 },
    PartialNotAllowedByRequest,
    PartialAmountOutOfRange { safe: Money },
    NoSafeFullDateInHorizon,
    FullDateAfterDeadline { date: NaiveDate },
    NotLater,
    CompletesAfterDeadline { completion: NaiveDate },
    Unsafe { date: NaiveDate, balance: Money },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Evaluated {
    pub candidate: Candidate,
    pub outcome: Outcome,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Outcome {
    Survived { key: RankKey, safety: SafetyReport },
    Dropped(DropReason),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SearchResult {
    /// Every candidate considered, survivors first in rank order, then drops in generation order.
    pub evaluated: Vec<Evaluated>,
    /// Whether spending-change variants were generated.
    pub searched_changes: bool,
}

impl SearchResult {
    pub fn winner(&self) -> Option<(&Candidate, &RankKey, &SafetyReport)> {
        self.evaluated.first().and_then(|e| match &e.outcome {
            Outcome::Survived { key, safety } => Some((&e.candidate, key, safety)),
            Outcome::Dropped(_) => None,
        })
    }
}

/// An eligible spending-change action with its cut to monthly spending.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChangeAction {
    pub change: SpendingChange,
    pub cut: Money,
    pub description: String,
    pub category: String,
}

pub struct PlanContext<'a> {
    pub profile: &'a Profile,
    pub spec: &'a RequestSpec,
    pub request_date: NaiveDate,
    pub options: &'a [PaymentOption],
    pub baseline: &'a Forecast,
    pub safe_amount: Money,
    pub earliest_full_date: Option<NaiveDate>,
    pub streams: &'a Streams,
    pub forecast_inputs: &'a ForecastInputs<'a>,
    pub rules: &'a Rules,
}

pub fn search(ctx: &PlanContext) -> SearchResult {
    let mut evaluated = Vec::new();
    let base = base_candidates(ctx, &mut evaluated);
    for c in &base {
        evaluated.push(evaluate(ctx, ctx.baseline, c.clone()));
    }

    let base_ok = evaluated.iter().any(|e| matches!(&e.outcome, Outcome::Survived { key, .. } if !key.misses_deadline));
    let mut searched_changes = false;
    if !base_ok {
        searched_changes = true;
        // Only plans paid from today's position benefit from changes: partial and wait are
        // defined from the no-change safe amount and earliest date.
        let today: Vec<&Candidate> = base
            .iter()
            .filter(|c| matches!(c.method, PaymentMethod::FullPayment | PaymentMethod::Installments))
            .collect();
        if !today.is_empty() {
            let actions = eligible_actions(ctx);
            for set in change_sets(&actions, ctx.rules.max_spending_changes) {
                let changes: Vec<SpendingChange> = set.iter().map(|a| a.change.clone()).collect();
                let forecast = Forecast::build(ctx.forecast_inputs, &changes);
                if forecast.balance == ctx.baseline.balance {
                    continue; // no effect inside the horizon
                }
                for c in &today {
                    let mut v = (*c).clone();
                    v.changes = changes.clone();
                    let cut: Money = set.iter().map(|a| a.cut).sum();
                    let mut e = evaluate(ctx, &forecast, v);
                    if let Outcome::Survived { key, .. } = &mut e.outcome {
                        let count = key.change_order.0;
                        let ids = std::mem::take(&mut key.change_tail.2);
                        (key.change_order, key.change_tail) = match ctx.rules.change_preference {
                            ChangePreference::FewestChanges => ((count, cut), (0, Money::ZERO, ids)),
                            ChangePreference::SmallestCut => ((1, cut), (count, Money::ZERO, ids)),
                        };
                    }
                    // Unsafe change variants are not worth recording individually.
                    if matches!(e.outcome, Outcome::Survived { .. }) {
                        evaluated.push(e);
                    }
                }
            }
        }
    }

    let (mut survivors, drops): (Vec<Evaluated>, Vec<Evaluated>) =
        evaluated.into_iter().partition(|e| matches!(e.outcome, Outcome::Survived { .. }));
    survivors.sort_by(|a, b| rank_of(a).cmp(rank_of(b)));
    survivors.extend(drops);
    SearchResult { evaluated: survivors, searched_changes }
}

fn rank_of(e: &Evaluated) -> &RankKey {
    match &e.outcome {
        Outcome::Survived { key, .. } => key,
        Outcome::Dropped(_) => unreachable!("only survivors are ranked"),
    }
}

/// Eligible no-change candidates; ineligible ones are recorded as drops.
fn base_candidates(ctx: &PlanContext, evaluated: &mut Vec<Evaluated>) -> Vec<Candidate> {
    let p = ctx.profile;
    let spec = ctx.spec;
    let mut out = Vec::new();
    let mut drop = |c: Candidate, r: DropReason| evaluated.push(Evaluated { candidate: c, outcome: Outcome::Dropped(r) });

    // 1. Full payment today.
    let full_option = ctx.options.iter().find(|o| o.method == PaymentMethod::FullPayment);
    let full = Candidate {
        method: PaymentMethod::FullPayment,
        payments: vec![Payment {
            date: full_option.map_or(ctx.request_date, |o| o.first_payment_date),
            amount: spec.amount,
        }],
        option_id: full_option.map(|o| o.id.clone()),
        changes: vec![],
        total_paid: spec.amount,
    };
    if p.accepts(PaymentMethod::FullPayment) { out.push(full) } else { drop(full, DropReason::MethodNotAccepted) }

    // 2. Each installment option, on its exact schedule.
    let mut installments: Vec<&PaymentOption> =
        ctx.options.iter().filter(|o| o.method == PaymentMethod::Installments).collect();
    installments.sort_by_key(|o| o.id_rank());
    for o in installments {
        let c = Candidate {
            method: PaymentMethod::Installments,
            payments: o.schedule(),
            option_id: Some(o.id.clone()),
            changes: vec![],
            total_paid: o.total_payable_amount,
        };
        if !p.accepts(PaymentMethod::Installments) {
            drop(c, DropReason::MethodNotAccepted);
            continue;
        }
        let months = ctx.rules.installment_months(o);
        match p.max_installment_months {
            None => drop(c, DropReason::InstallmentsNotConsidered),
            Some(max) if months > max => drop(c, DropReason::ExceedsMaxInstallmentMonths { months, max }),
            Some(_) => out.push(c),
        }
    }

    // 3. Two-payment partial split.
    let safe = ctx.safe_amount;
    if let Some(earliest) = ctx.earliest_full_date {
        let c = Candidate {
            method: PaymentMethod::PartialPayment,
            payments: vec![
                Payment { date: ctx.request_date, amount: safe },
                Payment { date: earliest, amount: spec.amount - safe },
            ],
            option_id: None,
            changes: vec![],
            total_paid: spec.amount,
        };
        if !spec.allows_partial_payment {
            drop(c, DropReason::PartialNotAllowedByRequest);
        } else if !p.accepts(PaymentMethod::PartialPayment) {
            drop(c, DropReason::MethodNotAccepted);
        } else if !(safe > Money::ZERO && safe < spec.amount) {
            drop(c, DropReason::PartialAmountOutOfRange { safe });
        } else if earliest > spec.deadline {
            drop(c, DropReason::FullDateAfterDeadline { date: earliest });
        } else {
            out.push(c);
        }
    }

    // 4. Wait until the earliest safe full-payment date.
    match ctx.earliest_full_date {
        Some(earliest) => {
            let c = Candidate {
                method: PaymentMethod::Wait,
                payments: vec![Payment { date: earliest, amount: spec.amount }],
                option_id: None,
                changes: vec![],
                total_paid: spec.amount,
            };
            if !p.accepts(PaymentMethod::FullPayment) {
                drop(c, DropReason::MethodNotAccepted);
            } else if earliest <= ctx.request_date {
                drop(c, DropReason::NotLater);
            } else {
                out.push(c);
            }
        }
        None => drop(
            Candidate { method: PaymentMethod::Wait, payments: vec![], option_id: None, changes: vec![], total_paid: spec.amount },
            DropReason::NoSafeFullDateInHorizon,
        ),
    }
    out
}

fn evaluate(ctx: &PlanContext, forecast: &Forecast, c: Candidate) -> Evaluated {
    let completion = c.completion().unwrap_or(ctx.request_date);
    let misses_deadline = completion > ctx.spec.deadline;
    if misses_deadline && ctx.rules.drop_late_plans {
        return Evaluated { candidate: c, outcome: Outcome::Dropped(DropReason::CompletesAfterDeadline { completion }) };
    }
    let safety = forecast.check(&c.payments, ctx.rules);
    if let Some((date, balance)) = safety.first_breach {
        return Evaluated { candidate: c, outcome: Outcome::Dropped(DropReason::Unsafe { date, balance }) };
    }
    // RULES S1.1(a): full payment today needs that forecast's safe amount >= req, which also
    // covers the request day's own pre-credit low.
    if c.method == PaymentMethod::FullPayment && c.start() == Some(forecast.start) {
        let safe = forecast.raw_safe_amount();
        if safe < c.total_paid {
            let (balance, date) = forecast.trough();
            let balance = balance - c.total_paid;
            return Evaluated { candidate: c, outcome: Outcome::Dropped(DropReason::Unsafe { date, balance }) };
        }
    }
    let key = RankKey {
        misses_deadline,
        change_order: (c.changes.len(), Money::ZERO),
        total_paid: c.total_paid,
        start: c.start().unwrap_or(ctx.request_date),
        payment_count: c.payments.len(),
        option_rank: c.option_id.as_deref().map(id_rank),
        change_tail: (0, Money::ZERO, c.changes.iter().map(|x| x.event_id().to_string()).collect()),
    };
    Evaluated { candidate: c, outcome: Outcome::Survived { key, safety } }
}

/// Stop/reduce actions allowed on flexible, non-protected events in permitted categories.
pub fn eligible_actions(ctx: &PlanContext) -> Vec<ChangeAction> {
    let p = ctx.profile;
    let mut out = Vec::new();
    for s in &ctx.streams.streams {
        if s.direction != Direction::Debit || p.is_protected(&s.category) {
            continue;
        }
        for o in s.flexible_targets() {
            let current = match s.kind {
                StreamKind::Recurring => s.projected_amount,
                StreamKind::VariableSpend => o.amount,
            };
            push_actions(&mut out, p, &s.category, o, current, ctx.rules);
        }
    }
    out
}

fn push_actions(out: &mut Vec<ChangeAction>, p: &Profile, category: &str, o: &Occurrence, current: Money, rules: &Rules) {
    let in_list = |list: &[String]| list.iter().any(|c| c == category);
    if o.flexibility.can_stop() && in_list(&p.stoppable_categories) {
        out.push(ChangeAction {
            change: SpendingChange::Stop { event_id: o.event_id.clone() },
            cut: current,
            description: o.description.clone(),
            category: category.to_string(),
        });
    }
    if o.flexibility.can_reduce() && in_list(&p.reducible_categories) {
        if let Some(floor) = o.minimum_allowed_amount {
            let to = rules.reduce_to_amount(floor);
            if to < current {
                out.push(ChangeAction {
                    change: SpendingChange::ReduceTo { event_id: o.event_id.clone(), amount: to },
                    cut: current - to,
                    description: o.description.clone(),
                    category: category.to_string(),
                });
            }
        }
    }
}

/// All sets of 1..=max actions touching distinct events (stop and reduce on the same event
/// are mutually exclusive).
fn change_sets(actions: &[ChangeAction], max: usize) -> Vec<Vec<&ChangeAction>> {
    fn rec<'a>(actions: &'a [ChangeAction], from: usize, max: usize, cur: &mut Vec<&'a ChangeAction>, out: &mut Vec<Vec<&'a ChangeAction>>) {
        if !cur.is_empty() {
            out.push(cur.clone());
        }
        if cur.len() == max {
            return;
        }
        for i in from..actions.len() {
            if cur.iter().any(|a| a.change.event_id() == actions[i].change.event_id()) {
                continue;
            }
            cur.push(&actions[i]);
            rec(actions, i + 1, max, cur, out);
            cur.pop();
        }
    }
    let mut out = Vec::new();
    rec(actions, 0, max, &mut Vec::new(), &mut out);
    out
}
