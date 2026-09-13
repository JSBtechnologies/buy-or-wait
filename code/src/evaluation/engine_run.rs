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
        messages: Vec<model::Message>,
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
            messages: model::load_messages(d.join("messages.csv")).unwrap(),
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
        let default = base.clone();
        for (k, v) in patch.as_object().expect("rules patch must be a JSON object") {
            if base.get(k).is_some() {
                base[k] = v.clone();
                continue;
            }
            // RULES names are serde aliases: find the canonical field(s) the alias sets.
            let single: Rules = serde_json::from_value(serde_json::json!({ k.as_str(): v })).unwrap_or_else(|e| panic!("bad rules patch {k}: {e}"));
            let single = serde_json::to_value(single).unwrap();
            let set: Vec<String> = single.as_object().unwrap().iter().filter(|(f, x)| default.get(f.as_str()) != Some(*x)).map(|(f, _)| f.clone()).collect();
            assert!(!set.is_empty(), "rules patch {k}={v} is unknown or equals the default (no field changed)");
            for f in set {
                base[f.as_str()] = single[f.as_str()].clone();
            }
        }
        serde_json::from_value(base).unwrap()
    }

    fn session(inp: &Inputs, user: &str) -> anyhow::Result<Session> {
        session_with(inp, user, Rules::default())
    }

    /// The batch pipeline's evidence for one request (main.rs decide_one): retrieval by user and
    /// request date, then the deterministic skeleton parser. Used unless VERIFIER_EVIDENCE=store.
    fn pipeline_evidence(inp: &Inputs, user: &str, rd: NaiveDate) -> Vec<EvidenceRecord> {
        let home = inp.profiles.iter().find(|p| p.user_id == user).map(|p| p.home_currency.clone()).unwrap_or_default();
        let ev = crate::extract::retrieval::for_user(user, rd, &inp.messages, &[]);
        crate::extract::messages::deterministic_evidence(&ev.messages, &home)
    }

    fn session_with(inp: &Inputs, user: &str, rules: Rules) -> anyhow::Result<Session> {
        session_for(inp, user, None, rules)
    }

    fn session_for(inp: &Inputs, user: &str, rd: Option<NaiveDate>, rules: Rules) -> anyhow::Result<Session> {
        let mut s = Session::from_model(user, &inp.profiles, &inp.events, inp.rates.clone(), rules)?;
        let use_store = std::env::var("VERIFIER_EVIDENCE").map(|v| v == "store").unwrap_or(false);
        let ev = match rd {
            Some(rd) if !use_store && std::env::var("VERIFIER_NO_EVIDENCE").is_err() => pipeline_evidence(inp, user, rd),
            _ => evidence(user),
        };
        if !ev.is_empty() {
            s.apply_evidence(ev);
        }
        Ok(s)
    }

    fn decide(inp: &Inputs, r: &Req) -> anyhow::Result<Decision> {
        decide_with(inp, r, rules_with(&std::env::var("VERIFIER_RULES").unwrap_or_default()))
    }

    fn decide_with(inp: &Inputs, r: &Req, rules: Rules) -> anyhow::Result<Decision> {
        let session = session_for(inp, &r.user, Some(r.date), rules)?;
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

    /// Full invariant pass over every eval request (VERIFY_OUTPUT=<output.csv> also compares each
    /// shipped row with the engine's row for that request, byte for byte per field).
    #[test]
    #[ignore]
    fn eval_invariants() {
        let inp = inputs();
        let inv = Invariants::load(&dir(), &dir().join("requests.csv")).unwrap();
        let shipped: HashMap<String, crate::evaluation::OutputRow> = match std::env::var("VERIFY_OUTPUT") {
            Ok(p) => crate::evaluation::contract::read_output(Path::new(&p)).unwrap().1.into_iter().map(|r| (r.request_id.clone(), r)).collect(),
            Err(_) => HashMap::new(),
        };
        let (mut ok, mut violated, mut errors, mut diverged, mut warns) = (0, 0, 0, 0, 0);
        let mut rows = Vec::new();
        for r in eval_requests() {
            let d = match decide(&inp, &r) {
                Ok(d) => d,
                Err(e) => {
                    errors += 1;
                    println!("ENGINE_ERROR {} {e}", r.id);
                    continue;
                }
            };
            let (b, bl, wc, wl) = (d.baseline_series(), d.baseline_low_series(), d.with_changes_series(), d.with_changes_low_series());
            let fc = ForecastSeries { start: r.date, minimum: d.minimum_f64(), baseline: &b, baseline_low: Some(&bl), with_changes: wc.as_deref(), with_changes_low: wl.as_deref() };
            if std::env::var("VERIFIER_ONLY").map(|o| o.split(',').any(|x| x == r.id)).unwrap_or(false) {
                let no_ev = std::env::var("VERIFIER_NO_EVIDENCE").is_ok();
                for rec in if no_ev { Vec::new() } else { pipeline_evidence(&inp, &r.user, r.date) } {
                    println!("EVREC {} {} {:?}", r.id, rec.record_id, rec.fact);
                }
                for f in &d.baseline.flows {
                    if let FlowSource::Evidence { record_id } = &f.source {
                        println!("EVFLOW {} {} {} {} {}", r.id, record_id, f.date, f.category, f.amount.to_f64());
                    }
                }
                for f in d.baseline.flows.iter().filter(|f| f.amount.0 > 0) {
                    if !matches!(f.source, FlowSource::Evidence { .. }) {
                        println!("CREDIT {} {:?} {} {} {}", r.id, f.source, f.date, f.category, f.amount.to_f64());
                    }
                }
                let head = crate::evaluation::replay::headroom_from(&b, &bl, d.minimum_f64());
                let trough = bl.iter().cloned().fold(f64::INFINITY, f64::min);
                println!("DETAIL {} min_low_headroom={:.2} headroom_today={:.2} requested={}", r.id, trough - d.minimum_f64(), head[0], r.amount);
            }
            match inv.assert_row(&d.row, &fc) {
                Ok(w) => {
                    ok += 1;
                    for f in w {
                        warns += 1;
                        println!("WARN {f}");
                    }
                }
                Err(v) => {
                    violated += 1;
                    print!("VIOLATION {v}");
                }
            }
            let row: crate::evaluation::OutputRow = (&d.row).into();
            if let Some(s) = shipped.get(&r.id) {
                if *s != row {
                    diverged += 1;
                    for (i, (x, y)) in s.fields().iter().zip(row.fields()).enumerate() {
                        if *x != y {
                            println!("DIVERGED {} {}: shipped={x:?} engine={y:?}", r.id, crate::evaluation::contract::HEADER[i]);
                        }
                    }
                }
            }
            rows.push(row);
        }
        let file = inv.assert_file(&rows);
        println!(
            "EVAL INVARIANTS: {} rows, {ok} pass, {violated} violations, {errors} engine errors, {warns} warnings, file-level {}, shipped rows compared {} diverged {diverged}",
            rows.len(),
            if file.is_ok() { "PASS" } else { "FAIL" },
            shipped.len()
        );
    }

    /// PRIVATE verifier diagnosis (held-out): full forecast composition for VERIFIER_DIAG ids.
    /// Output stays in the verifier terminal; never post ids/values from held-out rows.
    #[test]
    #[ignore]
    fn private_forecast_dump() {
        let Ok(ids) = std::env::var("VERIFIER_DIAG") else { return };
        let inp = inputs();
        for r in samples().into_iter().chain(eval_requests()).filter(|r| ids.split(',').any(|x| x == r.id)) {
            let d = decide(&inp, &r).unwrap();
            let f = &d.facts;
            println!("=== {} start {} M {} reserved {} trough {} @{} horizon_end {} safe {} E {:?}", r.id, f.starting_balance.to_f64(), f.minimum_balance.to_f64(), f.reserved_pending_total.to_f64(), f.trough_balance.to_f64(), f.trough_date, f.horizon_end, f.safe_amount.to_f64(), f.earliest_full_date);
            for st in &d.streams.streams {
                let amts: Vec<f64> = st.occurrences.iter().map(|o| o.amount.to_f64()).collect();
                println!("  STREAM {:?} {} {:?} {:?} n={} proj={} amounts={:?}", st.kind, st.category, st.description, st.cadence, amts.len(), st.projected_amount.to_f64(), amts);
            }
            for fl in &d.baseline.flows {
                println!("  FLOW {} {:>12.2} {:<16} {:?}", fl.date, fl.amount.to_f64(), fl.category, fl.source);
            }
        }
    }

    /// Label-free attribution on the evaluation set: VERIFIER_RULES_STEPS is `;`-separated JSON
    /// patches (empty = Rules::default()); for each consecutive pair prints every evaluation row
    /// whose output changes, with the fields and status/method transitions. Evaluation rows only.
    #[test]
    #[ignore]
    fn rule_row_attribution() {
        let Ok(steps) = std::env::var("VERIFIER_RULES_STEPS") else { return };
        let steps: Vec<&str> = steps.split(';').collect();
        let inp = inputs();
        let eval = eval_requests();
        let rows: Vec<Vec<Option<crate::evaluation::OutputRow>>> = steps
            .iter()
            .map(|p| {
                let rules = rules_with(p);
                eval.iter().map(|r| decide_with(&inp, r, rules.clone()).ok().map(|d| (&d.row).into())).collect()
            })
            .collect();
        for w in 1..steps.len() {
            println!("STEP {:?} -> {:?}", steps[w - 1], steps[w]);
            for (i, r) in eval.iter().enumerate() {
                let (Some(x), Some(y)) = (&rows[w - 1][i], &rows[w][i]) else {
                    println!("ROW {} engine_error {} -> {}", r.id, rows[w - 1][i].is_none(), rows[w][i].is_none());
                    continue;
                };
                if x == y {
                    continue;
                }
                let mut fields = Vec::new();
                for (f, a, b) in [
                    ("amount", &x.amount_safe_to_pay, &y.amount_safe_to_pay),
                    ("status", &x.affordability_status, &y.affordability_status),
                    ("method", &x.recommended_payment_method, &y.recommended_payment_method),
                    ("plan", &x.payment_plan, &y.payment_plan),
                    ("earliest", &x.earliest_date_for_full_payment, &y.earliest_date_for_full_payment),
                    ("changes", &x.spending_changes_needed, &y.spending_changes_needed),
                    ("explanation", &x.decision_explanation, &y.decision_explanation),
                ] {
                    if a != b {
                        fields.push(f);
                    }
                }
                println!(
                    "ROW {} fields={fields:?} status {}->{} method {}->{} amount {}->{} earliest {:?}->{:?}",
                    r.id, x.affordability_status, y.affordability_status, x.recommended_payment_method, y.recommended_payment_method,
                    x.amount_safe_to_pay, y.amount_safe_to_pay, x.earliest_date_for_full_payment, y.earliest_date_for_full_payment
                );
            }
        }
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
            (crate::evaluation::scorer::score(&ds, &rows), rows)
        };
        let (pa, pb) = (std::env::var("VERIFIER_RULES_A").unwrap_or_default(), std::env::var("VERIFIER_RULES_B").unwrap_or_default());
        let ((a, rows_a), (b, rows_b)) = (run(&pa), run(&pb));
        println!("A={pa:?} B={pb:?}");
        let changed = rows_a.iter().zip(&rows_b).filter(|(x, y)| x != y).count();
        println!("ROWS CHANGED {changed}/{}{}", rows_a.len(), if changed == 0 { "  <- toggle had no effect on any sample row: check it is wired" } else { "" });
        // Blast radius on the evaluation set (no labels): rows whose output changes, by field.
        let (ra, rb) = (rules_with(&pa), rules_with(&pb));
        let mut eval_changed: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
        let eval = eval_requests();
        for r in &eval {
            let (Ok(x), Ok(y)) = (decide_with(&inp, r, ra.clone()), decide_with(&inp, r, rb.clone())) else {
                *eval_changed.entry("engine_error").or_default() += 1;
                continue;
            };
            let (x, y): (crate::evaluation::OutputRow, crate::evaluation::OutputRow) = ((&x.row).into(), (&y.row).into());
            for (f, a, b) in [
                ("any", format!("{x:?}"), format!("{y:?}")),
                ("amount", x.amount_safe_to_pay.clone(), y.amount_safe_to_pay.clone()),
                ("status", x.affordability_status.clone(), y.affordability_status.clone()),
                ("method", x.recommended_payment_method.clone(), y.recommended_payment_method.clone()),
                ("plan", x.payment_plan.clone(), y.payment_plan.clone()),
                ("earliest", x.earliest_date_for_full_payment.clone(), y.earliest_date_for_full_payment.clone()),
                ("changes", x.spending_changes_needed.clone(), y.spending_changes_needed.clone()),
            ] {
                if a != b {
                    *eval_changed.entry(f).or_default() += 1;
                }
            }
        }
        println!("EVAL CHANGED of {}: {eval_changed:?}", eval.len());
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
        std::fs::create_dir_all(&root).ok();
        let mut findings = crate::evaluation::evidence_audit::audit_dir(&dir(), &root, crate::engine::money::SCALE).unwrap();
        // Also audit every fact the batch pipeline applies, for all eval and sample requests.
        let inp = inputs();
        let ds_eval = Dataset::load(&dir(), &dir().join("requests.csv")).unwrap();
        let ds_samp = Dataset::load(&dir(), &dir().join("sample_requests.csv")).unwrap();
        let msgs = crate::evaluation::evidence_audit::load_messages(&dir()).unwrap();
        let mut applied = 0;
        for r in eval_requests().into_iter().chain(samples()) {
            let recs = pipeline_evidence(&inp, &r.user, r.date);
            applied += recs.len();
            let json = serde_json::to_value(&recs).unwrap();
            let n: u32 = r.id.rsplit('_').next().and_then(|x| x.parse().ok()).unwrap_or(0);
            let ds = if n >= 26 { &ds_eval } else { &ds_samp };
            findings.extend(crate::evaluation::evidence_audit::audit_records(ds, &msgs, &r.user, &json, crate::engine::money::SCALE));
        }
        println!("pipeline evidence records audited: {applied}");
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
            // Announced/one-off income must never be projected as a stream (spec: bonuses,
            // commissions, refunds, prizes count only once settled, and never recur).
            for f in &d.baseline.flows {
                if let FlowSource::Stream { stream_id } = &f.source {
                    let lower = stream_id.to_lowercase();
                    if f.amount.0 > 0
                        && ["bonus", "commission", "prize", "lottery", "refund", "reimburse", "arrears", "windfall", "payout"]
                            .iter()
                            .any(|k| lower.contains(k))
                    {
                        hits += 1;
                        println!("ONE-OFF-INCOME-PROJECTED {} {stream_id} @{}", r.id, f.date);
                    }
                }
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
