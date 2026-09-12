//! Evidence consistency (signoff): what the shipped pipeline applies must equal an independent
//! regeneration.
//!
//! 1. Drift: the live batch-path evidence (`mirror::evidence_for`, same code as main.rs) is
//!    compared record-for-record with every persisted snapshot that exists:
//!    `store/processed/evidence/<request_id>.json` (what the shipped run applied) and
//!    `store/evidence/<user_id>.json` (gen_evidence handoff dump). Any difference fails.
//! 2. Coverage: a verifier-owned classifier assigns each message (sent on or before its user's
//!    request date) to a template family by EN/ID signature phrases, with the fact kinds RULES.md
//!    and the board decisions require for that family. The pipeline's emitted kinds per message
//!    must be allowed for its family: a missing required kind or a counted kind on a no-fact
//!    family fails. Messages matching no family are reported (warning) so new templates surface.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::Result;
use serde_json::Value;

use super::contract::{Finding, Severity};
use super::mirror::{evidence_for, Inputs};
use crate::engine::ledger::EvidenceRecord;
use crate::model;

pub struct Family {
    pub name: &'static str,
    /// Any phrase (lowercase substring) identifies the family.
    pub phrases: &'static [&'static str],
    /// At least one of these kinds must be emitted (empty = no fact required).
    pub required_any: &'static [&'static str],
    /// Kinds that may be emitted.
    pub allowed: &'static [&'static str],
}

const INCOME_CHANGE: &[&str] = &["IncomeAmountChange", "NextIncomeAmount"];

/// Order matters: the first matching family wins (no-fact and unconfirmed families first so a
/// scam or pending-payout note never matches an income family).
pub const FAMILIES: &[Family] = &[
    Family { name: "scam_prize", phrases: &["selected for a cash prize", "release charge", "terpilih untuk menerima hadiah", "biaya pencairan"], required_any: &[], allowed: &[] },
    Family { name: "own_account_transfer", phrases: &["transfer between your two accounts", "transfer antara dua rekening"], required_any: &[], allowed: &[] },
    Family { name: "open_dispute_duplicate", phrases: &["extra card charge", "tagihan kartu tambahan"], required_any: &[], allowed: &[] },
    Family { name: "failed_debit_retry", phrases: &["previous debit attempt failed", "percobaan debit sebelumnya gagal"], required_any: &[], allowed: &[] },
    Family { name: "unrealized_investment", phrases: &["displayed market value", "displayed value of the investment", "nilai investasi", "nilai pasar"], required_any: &[], allowed: &[] },
    Family { name: "settled_one_offs", phrases: &["prize proceeds have reached", "investment sale have settled", "hasil penjualan investasi", "reimbursement for your earlier work expense", "penggantian atas biaya kerja", "dana hadiah"], required_any: &[], allowed: &[] },
    Family { name: "arrears_one_off", phrases: &["one-time arrears adjustment", "penyesuaian tunggakan", "gaji rutin untuk penggajian berikutnya sudah dikonfirmasi", "regular pay and the one-time adjustment"], required_any: &[], allowed: &[] },
    Family { name: "two_card_minimums", phrases: &["minimum payments due on two separate card", "dua rekening kartu"], required_any: &[], allowed: &[] },
    Family { name: "fx_salary_or_bill_row", phrases: &["receiving bank will convert", "bank penerima akan mengonversi", "charged in a foreign currency", "mata uang asing", "receipt contains the final", "receipt has the final", "payment was received on", "confirmed that the"], required_any: &[], allowed: &["Unconfirmed"] },
    Family { name: "unconfirmed_money", phrases: &["quarterly bonus", "bonus kuartalan", "refund has been initiated", "refund is still processing", "pengembalian dana", "prize claim has been verified", "klaim hadiah", "payout is still pending", "masih tertunda", "commission shown for open deals", "komisi dari transaksi"], required_any: &["Unconfirmed"], allowed: &["Unconfirmed"] },
    Family { name: "invoice_approved_A6", phrases: &["client approved an invoice payment", "klien menyetujui pembayaran faktur"], required_any: &["OneTimeFlow"], allowed: &["OneTimeFlow"] },
    Family { name: "household_income_ended_A4", phrases: &["household employment record has ended", "pendapatan kerja rumah tangga telah berakhir"], required_any: &["IncomeEnded"], allowed: &["IncomeEnded"] },
    Family { name: "income_ended", phrases: &["seasonal contract has ended", "employment has ended", "kontrak musiman", "hubungan kerja anda telah berakhir", "contract has ended"], required_any: &["IncomeEnded"], allowed: &["IncomeEnded"] },
    Family { name: "income_date_moved", phrases: &["now expected on", "replaces the payroll date", "kini diperkirakan masuk pada"], required_any: &["IncomeDateMoved"], allowed: &["IncomeDateMoved"] },
    Family { name: "temporary_pay", phrases: &["temporary monthly pay", "gaji bulanan sementara"], required_any: INCOME_CHANGE, allowed: INCOME_CHANGE },
    Family { name: "income_starts", phrases: &["first salary", "resumes on", "gaji pertama", "dilanjutkan"], required_any: &["IncomeStarts"], allowed: &["IncomeStarts", "NewRecurringExpense"] },
    Family { name: "income_amount_change", phrases: &["next salary is reduced to", "salary has increased to", "naik menjadi", "turun menjadi", "salary is now"], required_any: INCOME_CHANGE, allowed: INCOME_CHANGE },
    Family { name: "rent_increase", phrases: &["increases monthly rent by", "menaikkan biaya sewa"], required_any: &["ExpenseAmountChange"], allowed: &["ExpenseAmountChange"] },
];

pub fn classify(text: &str) -> Option<&'static Family> {
    let t = text.to_lowercase();
    FAMILIES.iter().find(|f| f.phrases.iter().any(|p| t.contains(p)))
}

fn kind(rec: &EvidenceRecord) -> String {
    serde_json::to_value(&rec.fact).ok().and_then(|v| v.as_object().and_then(|m| m.keys().next().cloned())).unwrap_or_default()
}

fn canonical(records: &[EvidenceRecord]) -> BTreeSet<String> {
    records
        .iter()
        // Through Value so object keys are sorted, matching snapshots read back from disk.
        .map(|r| format!("{} {}", r.record_id, serde_json::to_value(&r.fact).map(|v| v.to_string()).unwrap_or_default()))
        .collect()
}

fn canonical_json(v: &Value) -> BTreeSet<String> {
    v.as_array()
        .map(|a| {
            a.iter()
                .map(|r| {
                    format!(
                        "{} {}",
                        r.get("record_id").and_then(Value::as_str).unwrap_or(""),
                        r.get("fact").map(|f| f.to_string()).unwrap_or_default()
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

fn err(id: &str, code: &'static str, detail: String) -> Finding {
    Finding { request_id: id.to_string(), severity: Severity::Error, code, detail }
}

/// All requests (eval + sample) as (request_id, user_id, request_date).
fn all_requests(dataset_dir: &Path) -> Result<Vec<(String, String, chrono::NaiveDate)>> {
    let mut v: Vec<_> = model::load_requests(dataset_dir.join("requests.csv"))?.into_iter().map(|r| (r.request_id, r.user_id, r.request_date)).collect();
    v.extend(model::load_sample_requests(dataset_dir.join("sample_requests.csv"))?.into_iter().map(|r| (r.request_id, r.user_id, r.request_date)));
    Ok(v)
}

/// Compare emitted kinds per message with the family table. Pure, so it can be mutation-tested.
pub fn coverage_findings(
    messages: &[model::Message],
    request_date_of_user: &BTreeMap<String, chrono::NaiveDate>,
    emitted: &BTreeMap<String, BTreeSet<String>>,
) -> Vec<Finding> {
    let mut out = Vec::new();
    for m in messages {
        let Some(rd) = request_date_of_user.get(&m.user_id) else { continue };
        if m.sent_at.date_naive() > *rd {
            continue;
        }
        let got = emitted.get(&m.message_id).cloned().unwrap_or_default();
        match classify(&m.message_text) {
            None => out.push(Finding { request_id: m.message_id.clone(), severity: Severity::Warn, code: "EC2_unclassified_message", detail: format!("emits {got:?}: {:.90}", m.message_text) }),
            Some(f) => {
                if !f.required_any.is_empty() && !f.required_any.iter().any(|k| got.contains(*k)) {
                    out.push(err(&m.message_id, "EC3_required_fact_missing", format!("family {} needs one of {:?}, pipeline emits {got:?}", f.name, f.required_any)));
                }
                let bad: Vec<&String> = got.iter().filter(|k| !f.allowed.contains(&k.as_str())).collect();
                if !bad.is_empty() {
                    out.push(err(&m.message_id, "EC4_unexpected_fact", format!("family {} allows {:?}, pipeline emits {bad:?}", f.name, f.allowed)));
                }
            }
        }
    }
    out
}

pub fn check(dataset_dir: &Path, code_dir: &Path) -> Result<Vec<Finding>> {
    let inp = Inputs::load(dataset_dir)?;
    let mut out = Vec::new();
    let mut emitted: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (rid, uid, rd) in all_requests(dataset_dir)? {
        let live = evidence_for(&inp, &uid, rd);
        for r in &live {
            emitted.entry(r.record_id.split('#').next().unwrap_or("").to_string()).or_default().insert(kind(r));
        }
        let live_c = canonical(&live);
        for (label, path) in [
            ("applied", code_dir.join("store/processed/evidence").join(format!("{rid}.json"))),
            ("gen_evidence", code_dir.join("store/evidence").join(format!("{uid}.json"))),
        ] {
            let Ok(text) = std::fs::read_to_string(&path) else { continue };
            let snap = match serde_json::from_str::<Value>(&text) {
                Ok(v) => canonical_json(v.get("value").unwrap_or(&v)),
                Err(e) => {
                    out.push(err(&rid, "EC0_snapshot_unreadable", format!("{}: {e}", path.display())));
                    continue;
                }
            };
            let missing: Vec<&String> = snap.difference(&live_c).collect();
            let extra: Vec<&String> = live_c.difference(&snap).collect();
            if !missing.is_empty() || !extra.is_empty() {
                out.push(err(&rid, "EC1_evidence_drift", format!("{label} snapshot vs live parse: only in snapshot {missing:?}; only in live {extra:?}")));
            }
        }
    }
    // Coverage against the verifier's own family table.
    let reqs = all_requests(dataset_dir)?;
    let rd_of: BTreeMap<String, chrono::NaiveDate> = reqs.iter().map(|(_, u, d)| (u.clone(), *d)).collect();
    out.extend(coverage_findings(&inp.messages, &rd_of, &emitted));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn families_classify_real_templates_in_both_languages() {
        let cases = [
            ("The renewed lease increases monthly rent by 12%. The new amount will be used for the next rent payment.", "rent_increase"),
            ("Perpanjangan sewa menaikkan biaya sewa bulanan sebesar 10%.", "rent_increase"),
            ("Congratulations! You have been selected for a cash prize. Pay the release charge today.", "scam_prize"),
            ("The client approved an invoice payment of INR 196000. Settlement is expected on 2024-12-15.", "invoice_approved_A6"),
            ("Klien menyetujui pembayaran faktur sebesar IDR 30780000.", "invoice_approved_A6"),
            ("One household employment record has ended. The remaining confirmed monthly salary is INR 148000.", "household_income_ended_A4"),
            ("Your quarterly bonus is still subject to the final performance review.", "unconfirmed_money"),
            ("The matching debit and credit came from a transfer between your two accounts.", "own_account_transfer"),
        ];
        for (text, fam) in cases {
            assert_eq!(classify(text).map(|f| f.name), Some(fam), "{text}");
        }
    }

    #[test]
    fn coverage_and_drift_checks_fire() {
        let dataset = Path::new(env!("CARGO_MANIFEST_DIR")).join("../dataset");
        let inp = Inputs::load(&dataset).unwrap();
        let reqs = all_requests(&dataset).unwrap();
        let rd_of: BTreeMap<String, chrono::NaiveDate> = reqs.iter().map(|(_, u, d)| (u.clone(), *d)).collect();
        let mut emitted: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for (_, u, d) in &reqs {
            for r in evidence_for(&inp, u, *d) {
                emitted.entry(r.record_id.split('#').next().unwrap().to_string()).or_default().insert(kind(&r));
            }
        }
        let classified = inp.messages.iter().filter(|m| rd_of.get(&m.user_id).map(|d| m.sent_at.date_naive() <= *d).unwrap_or(false)).filter(|m| classify(&m.message_text).is_some()).count();
        println!("classified {classified} of {} messages; emitting messages {}", inp.messages.len(), emitted.len());
        assert!(coverage_findings(&inp.messages, &rd_of, &emitted).iter().all(|f| f.severity != Severity::Error));

        // message_55 (rent +12%) dropped: required fact missing.
        let mut m1 = emitted.clone();
        m1.remove("message_55");
        assert!(coverage_findings(&inp.messages, &rd_of, &m1).iter().any(|f| f.request_id == "message_55" && f.code == "EC3_required_fact_missing"));
        // Scam prize message given a counted credit: unexpected fact.
        let mut m2 = emitted.clone();
        m2.entry("message_67".into()).or_default().insert("OneTimeFlow".into());
        assert!(coverage_findings(&inp.messages, &rd_of, &m2).iter().any(|f| f.request_id == "message_67" && f.code == "EC4_unexpected_fact"));

        // A gen_evidence snapshot missing message_55 for user_73: drift.
        let tmp = std::env::temp_dir().join(format!("verifier_ec_{}", std::process::id()));
        std::fs::create_dir_all(tmp.join("store/evidence")).unwrap();
        std::fs::write(tmp.join("store/evidence/user_73.json"), "[]").unwrap();
        // An exact snapshot written with struct field order (not sorted keys) must NOT drift.
        let exact = evidence_for(&inp, "user_26", reqs.iter().find(|r| r.1 == "user_26").unwrap().2);
        std::fs::write(tmp.join("store/evidence/user_26.json"), serde_json::to_string_pretty(&exact).unwrap()).unwrap();
        let f = check(&dataset, &tmp).unwrap();
        assert!(f.iter().any(|x| x.request_id == "request_73" && x.code == "EC1_evidence_drift"), "{f:?}");
        assert!(!f.iter().any(|x| x.request_id == "request_26" && x.code == "EC1_evidence_drift"), "{f:?}");
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn live_pipeline_matches_family_table_and_snapshots() {
        let dataset = Path::new(env!("CARGO_MANIFEST_DIR")).join("../dataset");
        let f = check(&dataset, Path::new(env!("CARGO_MANIFEST_DIR"))).unwrap();
        let errors: Vec<String> = f.iter().filter(|x| x.severity == Severity::Error).map(|x| x.to_string()).collect();
        let warns: Vec<String> = f.iter().filter(|x| x.severity == Severity::Warn).map(|x| x.to_string()).collect();
        println!("{}", warns.join("\n"));
        assert!(errors.is_empty(), "{errors:#?}");
    }
}
