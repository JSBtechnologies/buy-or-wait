//! Per-user/per-request evidence index (PLAN.md §3 retrieval lever): only messages/images
//! that could change a forecast fact reach a model — same user, sent on or before the
//! evaluation date. Preprocessing (PLAN.md §2.3/§2.4) runs once per user, before any
//! request is bound; `for_request` narrows further for a user with more than one request
//! (the final-run dataset happens to have exactly one request per user, PLAN.md §3 usage
//! report note, so the two levels currently coincide).

use chrono::NaiveDate;

use crate::model::{Image, Message, Request};

#[derive(Debug, Clone, Default)]
pub struct Evidence<'a> {
    pub messages: Vec<&'a Message>,
    pub images: Vec<&'a Image>,
}

/// Every message/image belonging to `user_id`, sent on or before `as_of`.
pub fn for_user<'a>(
    user_id: &str,
    as_of: NaiveDate,
    messages: &'a [Message],
    images: &'a [Image],
) -> Evidence<'a> {
    Evidence {
        messages: messages
            .iter()
            .filter(|m| m.user_id == user_id && m.sent_at.date_naive() <= as_of)
            .collect(),
        images: images.iter().filter(|i| i.user_id == user_id).collect(),
    }
}

/// Narrow user-level evidence to one request: items tied to it directly, plus employer/
/// financial-service updates with no `request_id` (they amend the ledger regardless of
/// which request is being evaluated).
pub fn for_request<'a>(request: &Request, evidence: &Evidence<'a>) -> Evidence<'a> {
    Evidence {
        messages: evidence
            .messages
            .iter()
            .copied()
            .filter(|m| {
                m.request_id.as_deref() == Some(request.request_id.as_str()) || m.request_id.is_none()
            })
            .collect(),
        images: evidence
            .images
            .iter()
            .copied()
            .filter(|i| i.request_id == request.request_id)
            .collect(),
    }
}
