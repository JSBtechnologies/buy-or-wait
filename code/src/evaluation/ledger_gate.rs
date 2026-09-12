//! P2a ledger gate: an independent statement of how every event should be treated for cash, to
//! diff against the engine's ledger (status handling, lifecycle de-dup, no duplicate cash effects).
//!
//! Rules: PLAN.md §2.1 plus board `decision.dup_charges` — settled rows are terminal; a pending,
//! failed or cancelled row is superseded by a same-direction successor that moves cash; a settled
//! every row of a linked chain (and the row it links to) is excluded from stream history (RULES.md S2.1).

use std::collections::{BTreeMap, HashMap};

use super::data::{Dataset, Event};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Class {
    /// Already reflected in the balance; usable as history.
    Settled,
    /// Pending debit: money treated as already gone.
    Reserved,
    /// Counts on its settlement date.
    Scheduled,
    /// No cash effect.
    Excluded,
}

impl Class {
    pub fn moves_cash(self) -> bool {
        self != Class::Excluded
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Expected {
    pub event_id: String,
    pub class: Class,
    pub reason: &'static str,
    /// False for every member of a linked chain (RULES.md S2.1 linked-chain rule).
    pub spend_history: bool,
}

fn base(e: &Event) -> (Class, &'static str) {
    match (e.status.as_str(), e.direction.as_str()) {
        (_, "non_cash") | ("unrealized", _) => (Class::Excluded, "non_cash"),
        ("cancelled", _) => (Class::Excluded, "cancelled"),
        ("failed", _) => (Class::Excluded, "failed"),
        ("pending", "credit") => (Class::Excluded, "pending_credit"),
        ("pending", _) => (Class::Reserved, "pending_debit"),
        ("scheduled", _) => (Class::Scheduled, "scheduled"),
        ("settled", _) => (Class::Settled, "settled"),
        _ => (Class::Excluded, "unknown_status"),
    }
}

fn continues_lifecycle(e: &Event) -> bool {
    !matches!(e.event_type.as_str(), "refund" | "investment_sale" | "investment_valuation")
}

/// Expected treatment of every event of `user_id`, keyed by event id. Evidence (messages/images)
/// is not applied: callers compare evidence-free engine ledgers, or skip events evidence touched.
pub fn expected_for_user(ds: &Dataset, user_id: &str) -> BTreeMap<String, Expected> {
    let events: Vec<&Event> = ds
        .events_by_user
        .get(user_id)
        .map(|ids| ids.iter().filter_map(|id| ds.events.get(id)).collect())
        .unwrap_or_default();
    let mut out: BTreeMap<String, Expected> = events
        .iter()
        .map(|e| {
            let (class, reason) = base(e);
            (e.event_id.clone(), Expected { event_id: e.event_id.clone(), class, reason, spend_history: true })
        })
        .collect();
    for succ in &events {
        let Some(pred_id) = &succ.linked_event_id else { continue };
        let Some(pred) = ds.events.get(pred_id) else { continue };
        let succ_moves = out[&succ.event_id].class.moves_cash();
        if continues_lifecycle(succ)
            && succ_moves
            && pred.direction == succ.direction
            && pred.status != "settled"
        {
            if let Some(p) = out.get_mut(pred_id) {
                if p.class.moves_cash() {
                    p.class = Class::Excluded;
                    p.reason = "superseded";
                }
            }
        }
        {
            for id in [pred_id.as_str(), succ.event_id.as_str()] {
                if let Some(x) = out.get_mut(id) {
                    x.spend_history = false;
                }
            }
        }
    }
    out
}

#[derive(Clone, Debug)]
pub struct GateDiff {
    pub event_id: String,
    pub expected: String,
    pub got: String,
}

/// Compare an engine ledger (event id → class, spend_history) with the expectation.
/// Also flags any lifecycle chain in which more than one member still moves cash.
pub fn compare(
    ds: &Dataset,
    user_id: &str,
    engine: &HashMap<String, (Class, bool)>,
    skip: &[String],
) -> Vec<GateDiff> {
    let exp = expected_for_user(ds, user_id);
    let mut diffs = Vec::new();
    for (id, e) in &exp {
        if skip.contains(id) {
            continue;
        }
        match engine.get(id) {
            None => diffs.push(GateDiff { event_id: id.clone(), expected: format!("{:?}", e.class), got: "missing".into() }),
            Some((class, hist)) => {
                if *class != e.class {
                    diffs.push(GateDiff {
                        event_id: id.clone(),
                        expected: format!("{:?} ({})", e.class, e.reason),
                        got: format!("{class:?}"),
                    });
                }
                if *hist != e.spend_history && e.class == Class::Settled {
                    diffs.push(GateDiff {
                        event_id: id.clone(),
                        expected: format!("spend_history={}", e.spend_history),
                        got: format!("spend_history={hist}"),
                    });
                }
            }
        }
    }
    // No duplicate cash effect along a lifecycle link (settled original + reserved open dispute is allowed).
    for id in exp.keys() {
        let ev = &ds.events[id];
        let Some(pred) = &ev.linked_event_id else { continue };
        let (Some((a, _)), Some((b, _))) = (engine.get(id), engine.get(pred)) else { continue };
        let pred_ev = &ds.events[pred];
        let same_leg = pred_ev.direction == ev.direction && continues_lifecycle(ev) && pred_ev.status != "settled";
        if same_leg && a.moves_cash() && b.moves_cash() {
            diffs.push(GateDiff {
                event_id: id.clone(),
                expected: format!("only one of {pred}/{id} moves cash"),
                got: format!("{b:?}/{a:?}"),
            });
        }
    }
    diffs
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn ds() -> Dataset {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../dataset");
        Dataset::load(&dir, &dir.join("requests.csv")).unwrap()
    }

    #[test]
    fn lifecycle_expectations_on_real_rows() {
        let ds = ds();
        let ex = |uid: &str, id: &str| expected_for_user(&ds, uid)[id].clone();
        // cancelled authorization -> settled purchase; the chain is a one-off, not stream history
        assert_eq!(ex("user_01", "event_100").class, Class::Excluded);
        assert_eq!(ex("user_01", "event_101").class, Class::Settled);
        assert!(!ex("user_01", "event_101").spend_history);
        assert!(ex("user_01", "event_01").spend_history);
        // settled charge reversed by settled refund: both stay settled, neither is spending history
        let (c, r) = (ex("user_01", "event_98"), ex("user_01", "event_99"));
        assert_eq!((c.class, r.class), (Class::Settled, Class::Settled));
        assert!(!c.spend_history && !r.spend_history);
        // failed bill -> scheduled retry
        let u = &ds.events["event_5168"].user_id;
        assert_eq!(ex(u, "event_5168").class, Class::Excluded);
        assert_eq!(ex(u, "event_5169").class, Class::Scheduled);
        // settled original + pending possible duplicate (open dispute): both keep their cash effect
        assert_eq!(ex("user_138", "event_12708").class, Class::Settled);
        assert_eq!(ex("user_138", "event_12709").class, Class::Reserved);
        // pending refund never counts; unrealized valuation never counts
        let u = &ds.events["event_1785"].user_id;
        assert_eq!(ex(u, "event_1785").class, Class::Excluded);
        let u = &ds.events["event_1856"].user_id;
        assert_eq!(ex(u, "event_1856").class, Class::Excluded);
    }

    #[test]
    fn compare_catches_wrong_treatment_and_double_counting() {
        let ds = ds();
        let as_engine = |uid: &str| -> HashMap<String, (Class, bool)> {
            expected_for_user(&ds, uid).into_iter().map(|(k, v)| (k, (v.class, v.spend_history))).collect()
        };
        assert!(compare(&ds, "user_01", &as_engine("user_01"), &[]).is_empty());

        // settled original erased by its pending duplicate (the 81e1127 bug)
        let mut m = as_engine("user_138");
        m.insert("event_12708".into(), (Class::Excluded, true));
        assert!(compare(&ds, "user_138", &m, &[]).iter().any(|d| d.event_id == "event_12708"));

        // authorization and settlement both moving cash
        let mut m = as_engine("user_01");
        m.insert("event_100".into(), (Class::Reserved, true));
        let d = compare(&ds, "user_01", &m, &[]);
        assert!(d.iter().any(|x| x.expected.contains("only one of event_100/event_101")), "{d:?}");

        // chain member left in spending history
        let mut m = as_engine("user_01");
        m.insert("event_98".into(), (Class::Settled, true));
        assert!(compare(&ds, "user_01", &m, &[]).iter().any(|d| d.expected == "spend_history=false"));

        // skipped (evidence-touched) events are not compared
        let mut m = as_engine("user_01");
        m.remove("event_98");
        assert!(compare(&ds, "user_01", &m, &["event_98".to_string()]).is_empty());
    }
}
