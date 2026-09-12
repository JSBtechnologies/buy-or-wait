//! Sample harness (engine self-check, not the verifier's scorer): runs the engine on
//! `dataset/sample_requests.csv` and prints field-by-field agreement.
//! `cargo test --lib engine::samples -- --ignored --nocapture`

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::Path;
    use std::sync::Arc;

    use crate::engine::{session::Session, types::*, Rules};
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
        let n = samples.len().min(18);
        for s in samples.iter().take(18) {
            if only.as_deref().is_some_and(|o| o != s.request_id) { continue; }
            let mut session = Session::from_model(&s.user_id, &profiles, &events, rates.clone(), Rules::default()).unwrap();
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
            ];
            for (k, ok) in checks { if ok { *hits.entry(k).or_default() += 1; } }
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
            let session = Session::from_model(&r.user_id, &profiles, &events, rates.clone(), Rules::default()).unwrap();
            let opts: Vec<PaymentOption> = options.iter().filter(|o| o.request_id == r.request_id).map(|o| PaymentOption::from_model(o).unwrap()).collect();
            match session.decide(&r.request_id, r.request_date, &RequestSpec::from_model(r), &opts) {
                Ok(d) => {
                    *dist.entry(format!("{}/{}", d.row.affordability_status, d.row.recommended_payment_method)).or_default() += 1;
                    if !d.facts.ledger_issues.is_empty() {
                        issues += 1;
                        println!("{} issues {:?}", r.request_id, d.facts.ledger_issues);
                    }
                }
                Err(e) => println!("{} ERROR {e:#}", r.request_id),
            }
        }
        println!("{} requests in {:?}; rows with ledger issues: {issues}", requests.len(), started.elapsed());
        for (k, v) in dist { println!("  {k}: {v}"); }
    }
}
