//! Sample harness (engine self-check, not the verifier's scorer): runs the engine on
//! `dataset/sample_requests.csv` and prints field-by-field agreement.
//! `cargo test --lib engine::samples -- --ignored --nocapture`

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::Path;
    use std::sync::Arc;

    use super::patched_rules;
    use crate::engine::{session::Session, types::*};
    use crate::model;

    /// Evidence handoff (board decision.evidence_handoff): `$EVIDENCE_DIR/<user_id>.json`
    /// (default `store/evidence`, relative to `code/`). `None` when the file is absent.
    fn load_evidence(user_id: &str) -> Option<Vec<crate::engine::ledger::EvidenceRecord>> {
        let dir = std::env::var("EVIDENCE_DIR").unwrap_or_else(|_| "store/evidence".into());
        let text = std::fs::read_to_string(Path::new(&dir).join(format!("{user_id}.json"))).ok()?;
        Some(serde_json::from_str(&text).unwrap_or_else(|e| panic!("{dir}/{user_id}.json: {e}")))
    }

    #[test]
    #[ignore]
    fn sample_report() {
        let ds = Path::new("../dataset");
        let profiles = model::load_financial_profiles(ds.join("financial_profiles.csv")).unwrap();
        let events = model::load_financial_events(ds.join("financial_events.csv")).unwrap();
        let rates = Arc::new(RateTable::from_model(&model::load_exchange_rates(ds.join("exchange_rates.csv")).unwrap()));
        let options = model::load_request_payment_options(ds.join("request_payment_options.csv")).unwrap();
        let samples = model::load_sample_requests(ds.join("sample_requests.csv")).unwrap();
        let messages = model::load_messages(ds.join("messages.csv")).unwrap();
        let only: Option<String> = std::env::var("ONLY").ok();
        let mut hits: HashMap<&str, usize> = HashMap::new();
        let (mut uncapped, mut within1, mut abs_err) = (0usize, 0usize, 0f64);
        let n = samples.len().min(18);
        for s in samples.iter().take(18) {
            if only.as_deref().is_some_and(|o| o != s.request_id) { continue; }
            let mut session = Session::from_model(&s.user_id, &profiles, &events, rates.clone(), patched_rules()).unwrap();
            let msgs: Vec<&model::Message> = messages.iter().filter(|m| m.user_id == s.user_id && m.sent_at.date_naive() <= s.request_date).collect();
            let ev = match load_evidence(&s.user_id) {
                Some(ev) => ev,
                None => crate::extract::messages::deterministic_evidence(&msgs, &session.profile().home_currency.clone()),
            };
            if only.is_some() { for e in &ev { println!("  evidence {} {:?}", e.record_id, e.fact); } }
            session.apply_evidence(ev);
            let opts: Vec<PaymentOption> = options.iter().filter(|o| o.request_id == s.request_id).map(|o| PaymentOption::from_model(o).unwrap()).collect();
            let spec = RequestSpec { amount: crate::engine::money::Money::from_f64(s.requested_amount), deadline: s.desired_completion_date, request_type: s.request_type.clone(), allows_partial_payment: s.allows_partial_payment };
            let d = match session.decide(&s.request_id, s.request_date, &spec, &opts) { Ok(d) => d, Err(e) => { println!("{} ERROR {e}", s.request_id); continue; } };
            let r = &d.row;
            let exp_earliest = s.earliest_date_for_full_payment.map(|x| x.to_string()).unwrap_or_default();
            let checks = [
                ("safe", r.amount_safe_to_pay == crate::engine::money::Money::from_f64(s.amount_safe_to_pay).fmt_plain()),
                ("status", r.affordability_status == s.affordability_status),
                ("method", r.recommended_payment_method == s.recommended_payment_method),
                ("plan", r.payment_plan == s.payment_plan),
                ("earliest", r.earliest_date_for_full_payment == exp_earliest),
                ("changes", r.spending_changes_needed == s.spending_changes_needed),
                ("explanation", r.decision_explanation == s.decision_explanation),
            ];
            for (k, ok) in checks { if ok { *hits.entry(k).or_default() += 1; } }
            let label = crate::engine::money::Money::from_f64(s.amount_safe_to_pay);
            if label != spec.amount && label.0 > 0 {
                let outflow = d.facts.starting_balance - d.facts.minimum_balance - label;
                let err = (d.facts.raw_safe_amount - label).0.abs() as f64 / outflow.0.max(1) as f64;
                uncapped += 1; abs_err += err; if err <= 0.01 { within1 += 1; }
            }
            let bad: Vec<&str> = checks.iter().filter(|c| !c.1).map(|c| c.0).collect();
            println!("{} {} | safe {} vs {} | {} {} vs {} {} | earliest {} vs {} | plan {} vs {} | chg {} vs {} | trough {} @{}",
                s.request_id, if bad.is_empty() { "OK".to_string() } else { format!("MISS{bad:?}") },
                r.amount_safe_to_pay, s.amount_safe_to_pay, r.affordability_status, r.recommended_payment_method,
                s.affordability_status, s.recommended_payment_method, r.earliest_date_for_full_payment, exp_earliest,
                r.payment_plan, s.payment_plan, r.spending_changes_needed, s.spending_changes_needed,
                d.facts.trough_balance, d.facts.trough_date);
            if only.is_some() {
                for st in &d.streams.streams { println!("  stream {} {:?} amt {} last {}", st.id, st.cadence, st.projected_amount, st.last_date()); }
                for f in &d.baseline.flows { println!("  flow {} {} {} {:?}", f.date, f.amount, f.category, f.source); }
                for c in &d.facts.candidates { println!("  cand {} {:?}", c.label, c.outcome); }
                println!("  expl: {}", r.decision_explanation);
            }
        }
        println!("scores over {n}: {hits:?}");
        println!("uncapped {uncapped}: outflow err within 1% {within1}, mean abs outflow err {:.2}%", abs_err / uncapped.max(1) as f64 * 100.0);
    }

    /// Runs every evaluation request (no evidence) and reports errors and distributions.
    /// `cargo test --lib engine::samples::tests::full_dataset_smoke -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn full_dataset_smoke() {
        let ds = Path::new("../dataset");
        let profiles = model::load_financial_profiles(ds.join("financial_profiles.csv")).unwrap();
        let events = model::load_financial_events(ds.join("financial_events.csv")).unwrap();
        let rates = Arc::new(RateTable::from_model(&model::load_exchange_rates(ds.join("exchange_rates.csv")).unwrap()));
        let options = model::load_request_payment_options(ds.join("request_payment_options.csv")).unwrap();
        let requests = model::load_requests(ds.join("requests.csv")).unwrap();
        let mut dist: std::collections::BTreeMap<String, usize> = Default::default();
        let mut issues = 0;
        let started = std::time::Instant::now();
        for r in &requests {
            let session = Session::from_model(&r.user_id, &profiles, &events, rates.clone(), patched_rules()).unwrap();
            let opts: Vec<PaymentOption> = options.iter().filter(|o| o.request_id == r.request_id).map(|o| PaymentOption::from_model(o).unwrap()).collect();
            match session.decide(&r.request_id, r.request_date, &RequestSpec::from_model(r), &opts) {
                Ok(d) => {
                    *dist.entry(format!("{}/{}", d.row.affordability_status, d.row.recommended_payment_method)).or_default() += 1;
                    let drivers: crate::engine::money::Money = d.facts.trough_drivers.iter().map(|t| t.total).sum();
                    assert_eq!(d.facts.starting_balance + drivers, d.facts.trough_balance, "{} trough drivers do not reconcile", r.request_id);
                    if !d.facts.missing_amounts.is_empty() {
                        issues += 1;
                        let rows: Vec<String> = d.facts.missing_amounts.iter().map(|id| {
                            let e = session.ledger().get(id).unwrap();
                            format!("{} {} {:?} {} {}", id, e.event.description, e.event.status, e.cash_date, if e.cash_date >= r.request_date { "future" } else { "history" })
                        }).collect();
                        println!("{} missing_amounts {:?}", r.request_id, rows);
                    }
                }
                Err(e) => println!("{} ERROR {e:#}", r.request_id),
            }
        }
        println!("{} requests in {:?}; rows with ledger issues: {issues}", requests.len(), started.elapsed());
        for (k, v) in dist { println!("  {k}: {v}"); }
    }
}

#[cfg(test)]
mod residuals {
    use std::path::Path;
    use std::sync::Arc;

    use crate::engine::money::Money;
    use super::patched_rules;
    use crate::engine::{session::Session, types::*};
    use crate::model;

    /// Safe-amount residual analysis on tuning rows: label - engine, the flows before the
    /// binding trough, and which single flow (if any) the residual matches.
    /// `cargo test --lib engine::samples::residuals -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn safe_residuals() {
        let ds = Path::new("../dataset");
        let profiles = model::load_financial_profiles(ds.join("financial_profiles.csv")).unwrap();
        let events = model::load_financial_events(ds.join("financial_events.csv")).unwrap();
        let rates = Arc::new(RateTable::from_model(&model::load_exchange_rates(ds.join("exchange_rates.csv")).unwrap()));
        let options = model::load_request_payment_options(ds.join("request_payment_options.csv")).unwrap();
        let messages = model::load_messages(ds.join("messages.csv")).unwrap();
        for s in model::load_sample_requests(ds.join("sample_requests.csv")).unwrap().iter().take(18) {
            let label = Money::from_f64(s.amount_safe_to_pay);
            let req = Money::from_f64(s.requested_amount);
            if label == req || label == Money::ZERO { continue; }
            let mut session = Session::from_model(&s.user_id, &profiles, &events, rates.clone(), patched_rules()).unwrap();
            let msgs: Vec<&model::Message> = messages.iter().filter(|m| m.user_id == s.user_id && m.sent_at.date_naive() <= s.request_date).collect();
            let home = session.profile().home_currency.clone();
            session.apply_evidence(crate::extract::messages::deterministic_evidence(&msgs, &home));
            let opts: Vec<PaymentOption> = options.iter().filter(|o| o.request_id == s.request_id).map(|o| PaymentOption::from_model(o).unwrap()).collect();
            let spec = RequestSpec { amount: req, deadline: s.desired_completion_date, request_type: s.request_type.clone(), allows_partial_payment: s.allows_partial_payment };
            let d = session.decide(&s.request_id, s.request_date, &spec, &opts).unwrap();
            let f = &d.baseline;
            let ours = f.raw_safe_amount();
            let diff = label - ours;
            let outflow_label = f.opening_balance - f.minimum_balance - label;
            let (trough, tdate) = f.trough();
            let pct = diff.0 as f64 / outflow_label.0.max(1) as f64 * 100.0;
            println!("{} {} label {} ours {} diff {} ({:+.2}% of outflow {}) trough {} horizon_end {}",
                s.request_id, home, label, ours.fmt_plan(), diff.fmt_plan(), pct, outflow_label, tdate, f.horizon_end());
            let _ = trough;
            let before: Vec<_> = f.flows.iter().filter(|x| x.date <= tdate).collect();
            for x in &before {
                let tag = if (x.amount.abs() - diff.abs()).0.abs() <= Money::from_f64(0.01).0.max((diff.abs().0) / 50) { "  <== ~diff" } else { "" };
                println!("    {} {:>14} {:<16} {:?}{}", x.date, x.amount.fmt_plan(), x.category, x.source, tag);
            }
        }
    }
}

/// Harness-only: `RULES_PATCH='{json}'` applied over `Rules::default()` (serde default).
#[cfg(test)]
fn patched_rules() -> crate::engine::Rules {
    serde_json::from_str(&std::env::var("RULES_PATCH").unwrap_or_else(|_| "{}".into())).expect("RULES_PATCH json")
}

#[cfg(test)]
mod explanations {
    use std::path::Path;
    use std::sync::Arc;

    use super::patched_rules;
    use crate::engine::{session::Session, types::*};
    use crate::model;

    /// Dump every evaluation explanation with its row fields to `$EXPLAIN_OUT` (TSV).
    #[test]
    #[ignore]
    fn dump_explanations() {
        let ds = Path::new("../dataset");
        let profiles = model::load_financial_profiles(ds.join("financial_profiles.csv")).unwrap();
        let events = model::load_financial_events(ds.join("financial_events.csv")).unwrap();
        let rates = Arc::new(RateTable::from_model(&model::load_exchange_rates(ds.join("exchange_rates.csv")).unwrap()));
        let options = model::load_request_payment_options(ds.join("request_payment_options.csv")).unwrap();
        let messages = model::load_messages(ds.join("messages.csv")).unwrap();
        let mut out = String::new();
        for r in model::load_requests(ds.join("requests.csv")).unwrap() {
            let mut session = Session::from_model(&r.user_id, &profiles, &events, rates.clone(), patched_rules()).unwrap();
            let msgs: Vec<&model::Message> = messages.iter().filter(|m| m.user_id == r.user_id && m.sent_at.date_naive() <= r.request_date).collect();
            let home = session.profile().home_currency.clone();
            session.apply_evidence(crate::extract::messages::deterministic_evidence(&msgs, &home));
            let opts: Vec<PaymentOption> = options.iter().filter(|o| o.request_id == r.request_id).map(|o| PaymentOption::from_model(o).unwrap()).collect();
            let d = session.decide(&r.request_id, r.request_date, &RequestSpec::from_model(&r), &opts).unwrap();
            let f = &d.facts;
            out.push_str(&format!(
                "{}\t{}\t{}\t{}\t{}\tdue={}\tmethods={:?}\tpartial={}\t{}\n",
                r.request_id, d.row.affordability_status, d.row.recommended_payment_method, d.row.amount_safe_to_pay,
                d.row.earliest_date_for_full_payment, f.desired_completion_date, f.accepted_methods, f.allows_partial_payment,
                d.row.decision_explanation
            ));
        }
        std::fs::write(std::env::var("EXPLAIN_OUT").unwrap_or_else(|_| "explanations.tsv".into()), out).unwrap();
    }
}

#[cfg(test)]
mod image_preview {
    use std::path::Path;
    use std::sync::Arc;

    use super::patched_rules;
    use crate::engine::ledger::{EvidenceRecord, EvidenceSource, Fact};
    use crate::engine::money::Money;
    use crate::engine::{session::Session, types::*};
    use crate::model;

    /// First bold number (e.g. `**1,234.50**`) in the RULES.md table row that names `event_id`.
    fn audited_figure(md: &str, event_id: &str) -> Option<f64> {
        let row = md.lines().find(|l| l.starts_with('|') && l.split(|c: char| !c.is_alphanumeric() && c != '_').any(|w| w == event_id))?;
        row.split("**").skip(1).step_by(2).find_map(|t| t.replace(',', "").trim().parse::<f64>().ok())
    }

    /// board verify.images_64_73: with the verifier's image readings as EventAmount facts,
    /// request_64 -> safe 0, E blank; request_73 -> not_affordable, safe 68498.58, E 2023-02-15.
    #[test]
    #[ignore]
    fn preview_64_73() {
        let ds = Path::new("../dataset");
        let profiles = model::load_financial_profiles(ds.join("financial_profiles.csv")).unwrap();
        let events = model::load_financial_events(ds.join("financial_events.csv")).unwrap();
        let rates = Arc::new(RateTable::from_model(&model::load_exchange_rates(ds.join("exchange_rates.csv")).unwrap()));
        let options = model::load_request_payment_options(ds.join("request_payment_options.csv")).unwrap();
        let messages = model::load_messages(ds.join("messages.csv")).unwrap();
        let requests = model::load_requests(ds.join("requests.csv")).unwrap();
        // Figures come from the analyst's RULES.md image audit table at test time; nothing
        // precomputed lives in code.
        let rules_md = std::fs::read_to_string(std::env::var("RULES_MD").unwrap_or_else(|_| "../RULES.md".into())).unwrap_or_default();
        for (rid, eid) in [("request_64", "event_6033"), ("request_73", "event_6859")] {
            let Some(amount) = audited_figure(&rules_md, eid) else {
                println!("{rid}: no RULES.md audit figure for {eid}; skipped");
                continue;
            };
            let r = requests.iter().find(|r| r.request_id == rid).unwrap();
            let mut session = Session::from_model(&r.user_id, &profiles, &events, rates.clone(), patched_rules()).unwrap();
            let msgs: Vec<&model::Message> = messages.iter().filter(|m| m.user_id == r.user_id && m.sent_at.date_naive() <= r.request_date).collect();
            let home = session.profile().home_currency.clone();
            let currency = session.ledger().get(eid).unwrap().event.currency.clone();
            let mut ev = crate::extract::messages::deterministic_evidence(&msgs, &home);
            ev.push(EvidenceRecord {
                record_id: format!("image_for_{eid}"),
                source: EvidenceSource::Image,
                observed_at: r.request_date.and_hms_opt(0, 0, 0).unwrap(),
                fact: Fact::EventAmount { event_id: eid.into(), amount: Money::from_f64(amount), currency },
            });
            if rid == "request_73" && !ev.iter().any(|e| matches!(e.fact, Fact::ExpenseAmountChange { .. })) {
                // message_55 (rent +12% on renewal, next payment): in main's store/evidence,
                // not parsed by the zero-token skeletons.
                ev.push(EvidenceRecord {
                    record_id: "message_55#0".into(),
                    source: EvidenceSource::Message { source_type: "service_provider".into() },
                    observed_at: chrono::NaiveDate::from_ymd_opt(2023, 1, 19).unwrap().and_hms_opt(9, 30, 0).unwrap(),
                    fact: Fact::ExpenseAmountChange { category: "rent".into(), amount: None, percent: Some(12.0), currency: None, effective: None },
                });
            }
            session.apply_evidence(ev);
            let opts: Vec<PaymentOption> = options.iter().filter(|o| o.request_id == rid).map(|o| PaymentOption::from_model(o).unwrap()).collect();
            let d = session.decide(rid, r.request_date, &RequestSpec::from_model(r), &opts).unwrap();
            println!("  raw_safe={} trough={:?}", d.facts.raw_safe_amount, (d.facts.trough_balance, d.facts.trough_date));
            for f in d.baseline.flows.iter().filter(|f| (f.date - r.request_date).num_days() <= 12) { println!("  flow {} {} {} {:?}", f.date, f.amount, f.category, f.source); }
            println!("{} {} {} safe={} E={:?} missing={:?} rejected={:?} | {}", rid, d.row.affordability_status, d.row.recommended_payment_method,
                d.row.amount_safe_to_pay, d.row.earliest_date_for_full_payment, d.facts.missing_amounts, d.facts.rejected_evidence, d.row.decision_explanation);
        }
    }
}
