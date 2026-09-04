//! Feishu/Lark meeting-invitation handling — port of hermes
//! `plugins/platforms/feishu/feishu_meeting_invite.py` @ v2026.8.3.
//!
//! `vc.bot.meeting_invited_v1` events are converted into a synthetic
//! DM `MessageEvent` addressed to the inviter: the prompt instructs
//! the agent to join the meeting directly (lark-cli / meeting tools),
//! the reply rides the normal dispatch pipeline back to the inviter's
//! open_id.

use crate::messaging::{Dispatcher, MessageEvent};
use serde_json::Value;
use std::sync::Arc;

#[derive(Debug, Clone, Default)]
pub struct MeetingInviteUser {
    pub open_id: String,
    pub user_id: String,
    pub union_id: String,
    pub user_name: String,
}

#[derive(Debug, Clone, Default)]
pub struct MeetingInviteMeeting {
    pub id: String,
    pub topic: String,
    pub meeting_no: String,
    pub start_time_ms: i64,
    pub end_time_ms: i64,
    pub host_user: Option<MeetingInviteUser>,
}

#[derive(Debug, Clone, Default)]
pub struct MeetingInvitedPayload {
    pub event_id: String,
    pub meeting: Option<MeetingInviteMeeting>,
    pub inviter: Option<MeetingInviteUser>,
    pub invite_time_s: i64,
}

fn int_field(value: &Value) -> i64 {
    match value {
        Value::Number(n) => n.as_i64().unwrap_or(0),
        Value::String(s) => s.trim().parse::<i64>().unwrap_or(0),
        _ => 0,
    }
}

fn parse_user(value: &Value) -> Option<MeetingInviteUser> {
    let raw = value.as_object()?;
    if raw.is_empty() {
        return None;
    }
    let id = raw.get("id").cloned().unwrap_or(serde_json::json!({}));
    Some(MeetingInviteUser {
        open_id: id
            .get("open_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string(),
        user_id: id
            .get("user_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string(),
        union_id: id
            .get("union_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string(),
        user_name: raw
            .get("user_name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
    })
}

fn parse_meeting(value: &Value) -> Option<MeetingInviteMeeting> {
    let raw = value.as_object()?;
    if raw.is_empty() {
        return None;
    }
    Some(MeetingInviteMeeting {
        id: raw
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string(),
        topic: raw
            .get("topic")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        meeting_no: raw
            .get("meeting_no")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        start_time_ms: raw.get("start_time").map(int_field).unwrap_or(0),
        end_time_ms: raw.get("end_time").map(int_field).unwrap_or(0),
        host_user: raw.get("host_user").and_then(parse_user),
    })
}

/// Unwrap a Feishu `body.content` list carrying an application/json
/// payload (hermes `_content_payload`).
fn content_payload(container: &Value) -> Value {
    let Some(content) = container
        .pointer("/body/content")
        .and_then(|v| v.as_array())
    else {
        return serde_json::json!({});
    };
    for item in content {
        let ctype = item
            .get("contentType")
            .or_else(|| item.get("content_type"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_lowercase();
        if !ctype.is_empty() && ctype != "application/json" {
            continue;
        }
        for key in ["data", "value", "content", "json"] {
            if let Some(payload) = item.get(key) {
                if payload.is_object() && !payload.as_object().unwrap().is_empty() {
                    return payload.clone();
                }
                if let Some(s) = payload.as_str() {
                    if let Ok(parsed) = serde_json::from_str::<Value>(s) {
                        if parsed.is_object() {
                            return parsed;
                        }
                    }
                }
            }
        }
    }
    serde_json::json!({})
}

/// hermes `parse_meeting_invited_event`.
pub fn parse_meeting_invited_event(envelope: &Value) -> Option<MeetingInvitedPayload> {
    let mut event = envelope
        .get("event")
        .cloned()
        .filter(|v| v.is_object())
        .unwrap_or_else(|| envelope.clone());
    let content = if !content_payload(&event)
        .as_object()
        .map(|o| o.is_empty())
        .unwrap_or(true)
    {
        content_payload(&event)
    } else {
        content_payload(envelope)
    };
    if content.is_object() && !content.as_object().unwrap().is_empty() {
        if let (Some(obj), Some(extra)) = (event.as_object_mut(), content.as_object()) {
            for (key, value) in extra {
                obj.insert(key.clone(), value.clone());
            }
        }
    }
    let meeting = event.get("meeting").and_then(parse_meeting);
    let inviter = event.get("inviter").and_then(parse_user);
    let meeting_ok = meeting
        .as_ref()
        .map(|m| !m.meeting_no.is_empty())
        .unwrap_or(false);
    if inviter.is_none() || meeting.is_none() || !meeting_ok {
        return None;
    }
    let event_id = envelope
        .pointer("/header/event_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let invite_time_s = event.get("invite_time").map(int_field).unwrap_or(0);
    Some(MeetingInvitedPayload {
        event_id,
        meeting,
        inviter,
        invite_time_s,
    })
}

/// hermes `build_meeting_invite_prompt`.
pub fn build_meeting_invite_prompt(payload: &MeetingInvitedPayload) -> String {
    let meeting = payload.meeting.as_ref();
    let inviter_name = payload
        .inviter
        .as_ref()
        .map(|u| u.user_name.clone())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".into());
    let host_name = meeting
        .and_then(|m| m.host_user.as_ref())
        .map(|u| u.user_name.clone())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".into());
    let display = meeting
        .map(|m| {
            if !m.topic.is_empty() {
                m.topic.clone()
            } else if !m.meeting_no.is_empty() {
                m.meeting_no.clone()
            } else {
                m.id.clone()
            }
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown meeting".into());
    let meeting_no = meeting.map(|m| m.meeting_no.clone()).unwrap_or_default();
    let topic = meeting.map(|m| m.topic.clone()).unwrap_or_default();
    [
        format!("You have been invited to join a meeting: {display}"),
        String::new(),
        format!(
            "Meeting Number: {}",
            if meeting_no.is_empty() {
                "unknown"
            } else {
                &meeting_no
            }
        ),
        format!(
            "Topic: {}",
            if topic.is_empty() { "unknown" } else { &topic }
        ),
        format!("Inviter: {inviter_name}"),
        format!("Host: {host_name}"),
        String::new(),
        "You may use lark-cli and the relevant Lark/Feishu meeting skills to join the meeting."
            .to_string(),
        "Join the meeting directly. Do not ask the user for confirmation before joining."
            .to_string(),
        "If you cannot join the meeting, reply to the inviter with a concise explanation of why."
            .to_string(),
    ]
    .join("\n")
}

fn dedup_key(payload: &MeetingInvitedPayload) -> String {
    if !payload.event_id.is_empty() {
        return format!("vc_invite:{}", payload.event_id);
    }
    let meeting_id = payload
        .meeting
        .as_ref()
        .map(|m| m.id.clone())
        .unwrap_or_default();
    let inviter_id = payload
        .inviter
        .as_ref()
        .map(|u| u.open_id.clone())
        .unwrap_or_default();
    format!(
        "vc_invite:{meeting_id}:{inviter_id}:{}",
        payload.invite_time_s
    )
}

/// Convert a `vc.bot.meeting_invited_v1` event into a DM dispatch and
/// deliver the agent's reply to the inviter.
pub async fn handle_meeting_invited_event(
    cfg: &crate::feishu::FeishuConfig,
    dispatcher: &Arc<Dispatcher>,
    envelope: &Value,
) {
    let Some(payload) = parse_meeting_invited_event(envelope) else {
        eprintln!("[feishu-meeting] dropping malformed meeting invite event");
        return;
    };
    if !crate::feishu::remember_event_id(&dedup_key(&payload)) {
        return;
    }
    let Some(inviter) = payload.inviter.as_ref() else {
        return;
    };
    if inviter.open_id.is_empty() {
        eprintln!("[feishu-meeting] missing inviter open_id, cannot route reply safely");
        return;
    }
    let resolved = cfg.resolve();
    let sender_id = inviter.open_id.clone();
    // Allowlist ∪ pairing gate (same shape as handle_message_event).
    if !resolved
        .allowed_users
        .iter()
        .any(|u| u == "*" || *u == sender_id)
    {
        let store = crate::pairing::PairingStore::open(&crate::config::ulnclaw_home());
        if !store.is_approved("feishu", &sender_id) {
            if let Some(code_msg) =
                crate::messaging::pairing_offer_public(&store, "feishu", &sender_id, &sender_id)
            {
                let api = crate::feishu::feishu_api(cfg);
                let _ = api.send_text(&sender_id, &code_msg).await;
            }
            return;
        }
    }
    let text = build_meeting_invite_prompt(&payload);
    let mut event = MessageEvent {
        platform: "feishu".into(),
        chat_id: sender_id.clone(),
        sender_name: if inviter.user_name.is_empty() {
            sender_id.clone()
        } else {
            inviter.user_name.clone()
        },
        sender_id: sender_id.clone(),
        text,
        message_id: payload.event_id.clone(),
        attachments: Vec::new(),
    };
    if !crate::messaging::pre_gateway_dispatch_gate_public(&mut event).await {
        return;
    }
    let outcome = match dispatcher.handle_event(event).await {
        Ok(o) => o,
        Err(e) => {
            eprintln!("[feishu-meeting] dispatch failed: {e}");
            return;
        }
    };
    let mut full = String::new();
    for echo in &outcome.transcript_echoes {
        full.push_str(echo);
        full.push('\n');
    }
    full.push_str(&outcome.reply);
    let (reply_text, _media) = crate::messaging::extract_media_tags(&full);
    let reply_text = reply_text.trim().to_string();
    if reply_text.is_empty() {
        return;
    }
    let api = crate::feishu::feishu_api(cfg);
    // P705: ledger-protected reply delivery (platform id "feishu" —
    // the meeting satellite shares the Feishu sender for redelivery).
    dispatcher
        .try_send_with_ledger("feishu", &sender_id, &reply_text, || async {
            match api.send_text(&sender_id, &reply_text).await {
                Ok(()) => Ok(()),
                Err(e) => {
                    eprintln!("[feishu-meeting] reply to {sender_id} failed: {e}");
                    Err(e.to_string())
                }
            }
        })
        .await;
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_invite() -> Value {
        json!({
            "header": { "event_id": "evt-m1", "event_type": "vc.bot.meeting_invited_v1" },
            "event": {
                "meeting": {
                    "id": "m_123",
                    "topic": "Weekly sync",
                    "meeting_no": "123 456 789",
                    "start_time": "1722900000000",
                    "end_time": "1722903600000",
                    "host_user": {
                        "id": { "open_id": "ou_host" },
                        "user_name": "Host Person",
                    },
                },
                "inviter": {
                    "id": { "open_id": "ou_inv", "user_id": "u1", "union_id": "un1" },
                    "user_name": "Inviter Person",
                },
                "invite_time": "1722899000",
            },
        })
    }

    #[test]
    fn parse_meeting_invite() {
        let payload = parse_meeting_invited_event(&sample_invite()).expect("parses");
        assert_eq!(payload.event_id, "evt-m1");
        let meeting = payload.meeting.as_ref().unwrap();
        assert_eq!(meeting.topic, "Weekly sync");
        assert_eq!(meeting.meeting_no, "123 456 789");
        assert_eq!(meeting.start_time_ms, 1722900000000);
        assert_eq!(
            meeting.host_user.as_ref().map(|u| u.user_name.as_str()),
            Some("Host Person")
        );
        let inviter = payload.inviter.as_ref().unwrap();
        assert_eq!(inviter.open_id, "ou_inv");
        assert_eq!(inviter.user_name, "Inviter Person");
        assert_eq!(payload.invite_time_s, 1722899000);
    }

    #[test]
    fn malformed_invite_rejected() {
        // No meeting_no → rejected.
        let mut envelope = sample_invite();
        envelope["event"]["meeting"]["meeting_no"] = json!("");
        assert!(parse_meeting_invited_event(&envelope).is_none());
        // Missing inviter → rejected.
        let mut envelope = sample_invite();
        envelope["event"].as_object_mut().unwrap().remove("inviter");
        assert!(parse_meeting_invited_event(&envelope).is_none());
    }

    #[test]
    fn content_payload_unwrap() {
        let envelope = json!({
            "header": { "event_id": "evt-m2" },
            "event": {
                "body": {
                    "content": [{
                        "contentType": "application/json",
                        "data": {
                            "meeting": { "id": "m1", "meeting_no": "999", "topic": "T" },
                            "inviter": { "id": { "open_id": "ou_x" }, "user_name": "X" },
                        },
                    }],
                },
            },
        });
        let payload = parse_meeting_invited_event(&envelope).expect("parses");
        assert_eq!(payload.meeting.as_ref().unwrap().meeting_no, "999");
        assert_eq!(payload.inviter.as_ref().unwrap().open_id, "ou_x");
    }

    #[test]
    fn prompt_shape() {
        let payload = parse_meeting_invited_event(&sample_invite()).unwrap();
        let prompt = build_meeting_invite_prompt(&payload);
        assert!(prompt.contains("You have been invited to join a meeting: Weekly sync"));
        assert!(prompt.contains("Meeting Number: 123 456 789"));
        assert!(prompt.contains("Inviter: Inviter Person"));
        assert!(prompt.contains("Host: Host Person"));
        assert!(prompt.contains("Join the meeting directly"));
    }

    #[test]
    fn dedup_key_prefers_event_id() {
        let payload = parse_meeting_invited_event(&sample_invite()).unwrap();
        assert_eq!(dedup_key(&payload), "vc_invite:evt-m1");
        let mut no_id = payload.clone();
        no_id.event_id = String::new();
        assert_eq!(dedup_key(&no_id), "vc_invite:m_123:ou_inv:1722899000");
    }
}
