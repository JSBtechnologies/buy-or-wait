//! Verifier harness over the engine on main (ignored tests; they write under `target/verifier/`).
//!
//! `cargo test --lib evaluation::engine_run -- --ignored --nocapture`
//! - `samples`: engine on sample_requests.csv → invariants (intraday replay) → contract + scores.
//!   Held-out detail only with VERIFIER_REVEAL=1.
//! - `ledger_gate`: independent cash treatment vs the engine ledger for every user.
//! - `double_counts`: a scheduled/reserved row and a projected recurring stream in the same
//!   category and month, across every request.

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use chrono::NaiveDate;

    use crate::engine::forecast::FlowSource;
    use crate::engine::ledger::{CashTreatment, EvidenceRecord};
    use crate::engine::money::Money;
    use crate::engine::session::{Decision, Session};
    use crate::engine::types::{PaymentOption, RateTable, RequestSpec};
    use crate::engine::Rules;
    use crate::evaluation::data::Dataset;
    use crate::evaluation::ledger_gate::{compare, Class};
    use crate::evaluation::{ForecastSeries, Invariants, Severity};
    use crate::model;

    struct Inputs {
        profiles: Vec<model::FinancialProfile>,
        events: Vec<model::FinancialEvent>,
        rates: Arc<RateTable>,
        options: Vec<model::RequestPaymentOption>,
    }

    struct Req {
        id: String,
        user: String,
        date: NaiveDate,
        amount: f64,
        due: NaiveDate,
        kind: String,
        partial: bool,
    }

    fn dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../dataset")
    }

    fn inputs() -> Inputs {
        let d = dir();
        Inputs {
            profiles: model::load_financial_profiles(d.join("financial_profiles.csv")).unwrap(),
            events: model::load_financial_events(d.join("financial_events.csv")).unwrap(),
            rates: Arc::new(RateTable::from_model(&model::load_exchange_rates(d.join("exchange_rates.csv")).unwrap())),
            options: model::load_request_payment_options(d.join("request_payment_options.csv")).unwrap(),
        }
    }

    fn samples() -> Vec<Req> {
        model::load_sample_requests(dir().join("sample_requests.csv"))
            .unwrap()
            .into_iter()
            .map(|r| Req {
                id: r.request_id,
                user: r.user_id,
                date: r.request_date,
                amount: r.requested_amount,
                due: r.desired_completion_date,
                kind: r.request_type,
                partial: r.allows_partial_payment,
            })
            .collect()
    }

    fn eval_requests() -> Vec<Req> {
        model::load_requests(dir().join("requests.csv"))
            .unwrap()
            .into_iter()
            .map(|r| Req {
                id: r.request_id,
                user: r.user_id,
                date: r.request_date,
                amount: r.requested_amount,
                due: r.desired_completion_date,
                kind: r.request_type,
                partial: r.allows_partial_payment,
            })
            .collect()
    }

    /// Extraction's handoff (board decision.evidence_handoff): `code/store/evidence/<user_id>.json`.
    /// Applied when present so scores reflect messages and images. VERIFIER_EVIDENCE_DIR overrides the
    /// directory (the store is gitignored, so point it at the tree that ran extraction);
    /// VERIFIER_NO_EVIDENCE=1 skips it.
    fn evidence(user: &str) -> Vec<EvidenceRecord> {
        if std::env::var("VERIFIER_NO_EVIDENCE").is_ok() {
            return Vec::new();
        }
        let root = std::env::var("VERIFIER_EVIDENCE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| Path::new(env!("CARGO_MANIFEST_DIR")).join("store/evidence"));
        let p = root.join(format!("{user}.json"));
        match std::fs::read_to_string(&p) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| panic!("bad evidence file {}: {e}", p.display())),
            Err(_) => Vec::new(),
        }
    }

    /// `Rules::default()` with a JSON object patch applied on top (e.g. `{"drop_late_plans":false}`).
    fn rules_with(patch: &str) -> Rules {
        let mut base = serde_json::to_value(Rules::default()).unwrap();
        let patch: serde_json::Value = serde_json::from_str(if patch.trim().is_empty() { "{}" } else { patch })
            .unwrap_or_else(|e| panic!("bad rules patch {patch:?}: {e}"));
        for (k, v) in patch.as_object().expect("rules patch must be a JSON object") {
            assert!(base.get(k).is_some(), "unknown Rules field {k}");
            base[k] = v.clone();
        }
        serde_json::from_value(base).unwrap()
    }

    fn session(inp: &Inputs, user: &str) -> anyhow::Result<Session> {
        session_with(inp, user, Rules::default())
    }

    fn session_with(inp: &Inputs, user: &str, rules: Rules) -> anyhow::Result<Session> {
        let mut s = Session::from_model(user, &inp.profiles, &inp.events, inp.rates.clone(), rules)?;
        let ev = evidence(user);
        if !ev.is_empty() {
            s.apply_evidence(ev);
        }
        Ok(s)
    }

    fn decide(inp: &Inputs, r: &Req) -> anyhow::Result<Decision> {
        decide_with(inp, r, Rules::default())
    }

    fn decide_with(inp: &Inputs, r: &Req, rules: Rules) -> anyhow::Result<Decision> {
        let session = session_with(inp, &r.user, rules)?;
        let opts = inp
            .options
            .iter()
            .filter(|o| o.request_id == r.id)
            .map(PaymentOption::from_model)
            .collect::<anyhow::Result<Vec<_>>>()?;
        let spec = RequestSpec {
            amount: Money::from_f64(r.amount),
            deadline: r.due,
            request_type: r.kind.clone(),
            allows_partial_payment: r.partial,
        };
        session.decide(&r.id, r.date, &spec, &opts)
    }

    fn out_dir() -> PathBuf {
        let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/verifier");
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    #[ignore]
    fn samples_scoreboard() {
        let inp = inputs();
        let inv = Invariants::load(&dir(), &dir().join("sample_requests.csv")).unwrap();
        let reveal = std::env::var("VERIFIER_REVEAL").is_ok();
        let out = out_dir().join("sample_out.csv");
        let mut w = csv::Writer::from_path(&out).unwrap();
        let (mut violations, mut engine_errors) = (0, 0);
        for r in samples() {
            let held = crate::evaluation::scorer::is_heldout(&r.id);
            let d = match decide(&inp, &r) {
                Ok(d) => d,
                Err(e) => {
                    engine_errors += 1;
                    println!("ENGINE_ERROR {} {}", if held && !reveal { "held-out" } else { &r.id }, e);
                    w.serialize(model::OutputRow { request_id: r.id.clone(), ..Default::default() }).unwrap();
                    continue;
                }
            };
            let (b, bl, wc, wl) = (d.baseline_series(), d.baseline_low_series(), d.with_changes_series(), d.with_changes_low_series());
            let fc = ForecastSeries {
                start: r.date,
                minimum: d.minimum_f64(),
                baseline: &b,
                baseline_low: Some(&bl),
                with_changes: wc.as_deref(),
                with_changes_low: wl.as_deref(),
            };
            let shown = !held || reveal;
            match inv.assert_row(&d.row, &fc) {
                Ok(warns) => {
                    for f in warns.iter().filter(|f| f.severity == Severity::Warn && shown) {
                        println!("INV {f}");
                    }
                }
                Err(v) => {
                    violations += 1;
                    if shown {
                        print!("INVARIANT_VIOLATION {v}");
                    }
                }
            }
            w.serialize(&d.row).unwrap();
        }
        w.flush().unwrap();
        println!("invariant violations: {violations}, engine errors: {engine_errors}");
        let (_, text) = crate::evaluation::score_file(&dir(), &out, reveal).unwrap();
        println!("{text}");
    }

    /// Overfit guard: VERIFIER_RULES_A / VERIFIER_RULES_B are JSON patches over Rules::default().
    /// Prints aggregate per-field counts for both splits and the B−A delta. No per-request detail.
    #[test]
    #[ignore]
    fn rule_delta() {
        let inp = inputs();
        let ds = Dataset::load(&dir(), &dir().join("sample_requests.csv")).unwrap();
        let run = |patch: &str| {
            let rules = rules_with(patch);
            let rows: Vec<crate::evaluation::OutputRow> = samples()
                .iter()
                .map(|r| match decide_with(&inp, r, rules.clone()) {
                    Ok(d) => (&d.row).into(),
                    Err(_) => crate::evaluation::OutputRow { request_id: r.id.clone(), ..Default::default() },
                })
                .collect();
            crate::evaluation::scorer::score(&ds, &rows)
        };
        let (pa, pb) = (std::env::var("VERIFIER_RULES_A").unwrap_or_default(), std::env::var("VERIFIER_RULES_B").unwrap_or_default());
        let (a, b) = (run(&pa), run(&pb));
        println!("A={pa:?} B={pb:?}");
        for (name, sa, sb) in [("tuning", &a.tuning, &b.tuning), ("held-out", &a.heldout, &b.heldout)] {
            for f in crate::evaluation::scorer::FIELDS {
                let (x, y) = (sa.matched.get(f).copied().unwrap_or(0), sb.matched.get(f).copied().unwrap_or(0));
                println!("DELTA {name:<8} {f:<38} {x:>2} -> {y:>2} ({:+})", y as i64 - x as i64);
            }
            let stats = |v: &[f64]| {
                let mut e: Vec<f64> = v.iter().copied().filter(|x| x.is_finite()).collect();
                e.sort_by(|a, b| a.partial_cmp(b).unwrap());
                let mean = if e.is_empty() { f64::NAN } else { e.iter().sum::<f64>() / e.len() as f64 };
                let median = e.get(e.len() / 2).copied().unwrap_or(f64::NAN);
                let within5 = e.iter().filter(|x| **x <= 0.05).count();
                (mean * 100.0, median * 100.0, within5)
            };
            let ((ma, da, wa), (mb, db, wb)) = (stats(&sa.amount_rel_err), stats(&sb.amount_rel_err));
            println!("DELTA {name:<8} amount rel err mean {ma:.2}% -> {mb:.2}%, median {da:.2}% -> {db:.2}%, within5% {wa} -> {wb}");
        }
    }

    #[test]
    #[ignore]
    fn evidence_audit() {
        let root = std::env::var("VERIFIER_EVIDENCE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| Path::new(env!("CARGO_MANIFEST_DIR")).join("store/evidence"));
        if !root.exists() {
            println!("no evidence dir at {}", root.display());
            return;
        }
        let findings = crate::evaluation::evidence_audit::audit_dir(&dir(), &root, crate::engine::money::SCALE).unwrap();
        for f in &findings {
            println!("EVIDENCE {f}");
        }
        let errors = findings.iter().filter(|f| f.severity == Severity::Error).count();
        println!("evidence audit: {errors} errors, {} warnings", findings.len() - errors);
    }

    #[test]
    #[ignore]
    fn ledger_gate() {
        let inp = inputs();
        let ds = Dataset::load(&dir(), &dir().join("requests.csv")).unwrap();
        let mut total = 0;
        for p in &inp.profiles {
            let s = session(&inp, &p.user_id).unwrap();
            let mut got = HashMap::new();
            let mut touched = Vec::new();
            for e in &s.ledger().entries {
                let class = match e.treatment {
                    CashTreatment::Settled => Class::Settled,
                    CashTreatment::Reserved => Class::Reserved,
                    CashTreatment::Scheduled => Class::Scheduled,
                    CashTreatment::Excluded(_) => Class::Excluded,
                };
                got.insert(e.event.id.clone(), (class, e.chain_root.is_none()));
                if !e.applied_evidence.is_empty() {
                    touched.push(e.event.id.clone());
                }
            }
            for d in compare(&ds, &p.user_id, &got, &touched) {
                total += 1;
                println!("GATE {} {} expected={} got={}", p.user_id, d.event_id, d.expected, d.got);
            }
        }
        println!("ledger gate diffs: {total} over {} users", inp.profiles.len());
    }

    #[test]
    #[ignore]
    fn double_counts() {
        let inp = inputs();
        let mut reqs = eval_requests();
        reqs.extend(samples().into_iter().filter(|r| !crate::evaluation::scorer::is_heldout(&r.id)));
        let (mut hits, mut errors) = (0, 0);
        for r in &reqs {
            let d = match decide(&inp, r) {
                Ok(d) => d,
                Err(e) => {
                    errors += 1;
                    println!("ENGINE_ERROR {} {e}", r.id);
                    continue;
                }
            };
            let mut by: HashMap<(String, String, bool), Vec<String>> = HashMap::new();
            for f in &d.baseline.flows {
                let tag = match &f.source {
                    FlowSource::Stream { stream_id } => format!("stream:{stream_id}@{}", f.date),
                    FlowSource::Scheduled { event_id } => format!("sched:{event_id}@{}", f.date),
                    FlowSource::Reserved { event_id } => format!("resv:{event_id}@{}", f.date),
                    FlowSource::Evidence { record_id } => format!("evid:{record_id}@{}", f.date),
                };
                by.entry((f.category.clone(), f.date.format("%Y-%m").to_string(), f.amount.0 > 0))
                    .or_default()
                    .push(tag);
            }
            let mut keys: Vec<_> = by.into_iter().collect();
            keys.sort();
            for ((cat, ym, credit), tags) in keys {
                let row = tags.iter().any(|t| t.starts_with("sched:"));
                let stream = tags.iter().any(|t| t.starts_with("stream:rec:"));
                let resv_same_stream = tags.iter().any(|t| t.starts_with("resv:")) && stream;
                if (row && stream) || (resv_same_stream && credit) {
                    hits += 1;
                    println!("DOUBLE? {} {cat} {ym} credit={credit} {tags:?}", r.id);
                }
            }
        }
        println!("double-count candidates: {hits}, engine errors: {errors}");
    }
}
