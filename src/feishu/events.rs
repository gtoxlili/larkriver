//! Strongly-typed views over the parts of Feishu webhook payloads we care about.
//!
//! Feishu actually has two webhook formats: the v2 schema (`{schema: "2.0", header, event}`)
//! and the v1 schema for url_verification challenges. We accept the request as a generic
//! `serde_json::Value` and pull out only the fields we use.

use serde_json::Value;

#[derive(Debug, Clone)]
pub struct InboundMessage {
    pub event_id: String,
    pub chat_id: String,
    pub chat_type: String, // "group" | "p2p"
    pub sender_open_id: String,
    pub message_type: String,
    pub text: String,
    pub mentions: Vec<Mention>,
}

#[derive(Debug, Clone)]
pub struct Mention {
    pub key: String,
    pub open_id: String,
}

pub fn parse_inbound_message(payload: &Value) -> Option<InboundMessage> {
    let event_id = payload
        .pointer("/header/event_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let event = payload.get("event").unwrap_or(payload);
    let sender = event.pointer("/sender/sender_id/open_id")?.as_str()?;
    let m = event.get("message")?;
    let chat_id = m.get("chat_id")?.as_str()?;
    let chat_type = m.get("chat_type")?.as_str().unwrap_or("group");
    let message_type = m.get("message_type")?.as_str().unwrap_or("text");
    let content_str = m.get("content")?.as_str()?;
    // SIMD JSON parse for the inner content blob — this runs on every
    // inbound text message webhook.
    let content_json: Value = sonic_rs::from_str(content_str).unwrap_or(Value::Null);
    let text = content_json
        .get("text")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let mut mentions = vec![];
    if let Some(arr) = m.get("mentions").and_then(|v| v.as_array()) {
        for mention in arr {
            let key = mention.get("key").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let open_id = mention
                .pointer("/id/open_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            mentions.push(Mention { key, open_id });
        }
    }
    Some(InboundMessage {
        event_id,
        chat_id: chat_id.to_string(),
        chat_type: chat_type.to_string(),
        sender_open_id: sender.to_string(),
        message_type: message_type.to_string(),
        text,
        mentions,
    })
}

#[derive(Debug, Clone)]
pub struct CardAction {
    /// Stable identifier the bot uses to dedupe duplicate deliveries of the
    /// same click — Feishu may invoke the callback URL more than once for a
    /// single user action (retries, schema-version mirroring, etc.).
    pub event_id: String,
    pub open_id: String,
    pub open_chat_id: String,
    pub value: Value,
    /// `event.action.form_value` — populated when a form_submit button is clicked.
    /// Keys are the `name` of each input/select inside the form.
    pub form_value: Value,
}

#[derive(Debug, Clone)]
pub struct MemberAdded {
    pub event_id: String,
    pub chat_id: String,
    pub users: Vec<AddedUser>,
}

#[derive(Debug, Clone)]
pub struct AddedUser {
    pub open_id: String,
    pub name: String,
}

/// Parse a `im.chat.member.user.added_v1` event payload (full body, with header).
pub fn parse_member_added(payload: &Value) -> Option<MemberAdded> {
    let event_id = payload
        .pointer("/header/event_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let event = payload.get("event").unwrap_or(payload);
    let chat_id = event.get("chat_id")?.as_str()?.to_string();
    let users_arr = event.get("users")?.as_array()?;
    let users: Vec<AddedUser> = users_arr
        .iter()
        .filter_map(|u| {
            let open_id = u.pointer("/user_id/open_id")?.as_str()?.to_string();
            let name = u
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("新玩家")
                .to_string();
            Some(AddedUser { open_id, name })
        })
        .collect();
    if users.is_empty() {
        None
    } else {
        Some(MemberAdded {
            event_id,
            chat_id,
            users,
        })
    }
}

/// Card action payload comes in two slightly different shapes depending on the
/// "Card Callback" version selected in the bot's settings. Try both.
pub fn parse_card_action(payload: &Value) -> Option<CardAction> {
    // Newer schema 2.0 wrapper
    if payload.get("schema").is_some() {
        let header = payload.get("header")?;
        if header.get("event_type")?.as_str()? != "card.action.trigger" {
            return None;
        }
        let event_id = header
            .get("event_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let event = payload.get("event")?;
        let open_id = event.pointer("/operator/open_id")?.as_str()?.to_string();
        let open_chat_id = event
            .pointer("/context/open_chat_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let value = event.pointer("/action/value").cloned().unwrap_or(Value::Null);
        let form_value = event
            .pointer("/action/form_value")
            .cloned()
            .unwrap_or(Value::Null);
        return Some(CardAction {
            event_id,
            open_id,
            open_chat_id,
            value,
            form_value,
        });
    }

    // Legacy flat payload from the card request URL
    let event_id = payload
        .get("uuid")
        .or_else(|| payload.get("event_id"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let open_id = payload.get("open_id")?.as_str()?.to_string();
    let open_chat_id = payload
        .get("open_chat_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let value = payload
        .pointer("/action/value")
        .cloned()
        .unwrap_or(Value::Null);
    let form_value = payload
        .pointer("/action/form_value")
        .cloned()
        .unwrap_or(Value::Null);
    Some(CardAction {
        event_id,
        open_id,
        open_chat_id,
        value,
        form_value,
    })
}
