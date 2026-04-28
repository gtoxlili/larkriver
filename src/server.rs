use crate::bot::Bot;
use crate::feishu::events::{parse_card_action, parse_inbound_message, parse_member_added};
use anyhow::Result;
use axum::{
    body::Bytes,
    extract::State,
    http::{header, StatusCode},
    response::IntoResponse,
    routing::{get, post},
    Router,
};
use serde_json::{json, Value};
use std::sync::Arc;
use tracing::{info, warn};

#[derive(Clone)]
struct AppState {
    bot: Arc<Bot>,
}

pub async fn run(bot: Arc<Bot>, addr: &str) -> Result<()> {
    let state = AppState { bot };
    let app = Router::new()
        .route("/", get(|| async { "larkriver bot" }))
        .route("/healthz", get(|| async { "ok" }))
        .route("/webhook/event", post(event_handler))
        .route("/webhook/card", post(card_handler))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!("listening on {addr}");
    axum::serve(listener, app).await?;
    Ok(())
}

/// Build a JSON response from a `serde_json::Value` using sonic-rs for the
/// stringification step. Saves a `serde_json::to_vec` call on every webhook
/// reply.
fn sonic_json(value: &Value) -> axum::response::Response {
    let bytes = sonic_rs::to_vec(value).unwrap_or_else(|_| value.to_string().into_bytes());
    axum::response::Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(axum::body::Body::from(bytes))
        .expect("static response builder cannot fail")
}

/// Parse the inbound webhook body with sonic-rs (SIMD-accelerated JSON).
/// On failure we return an empty `null` Value so downstream code can skip
/// gracefully instead of returning 400 — Feishu retries 4xx responses.
fn parse_body(bytes: &Bytes) -> Value {
    sonic_rs::from_slice(bytes).unwrap_or(Value::Null)
}

async fn event_handler(State(state): State<AppState>, body_bytes: Bytes) -> impl IntoResponse {
    let body = parse_body(&body_bytes);

    // url_verification challenge (sent during initial setup)
    if body.get("type").and_then(|v| v.as_str()) == Some("url_verification") {
        if let Some(challenge) = body.get("challenge").and_then(|v| v.as_str()) {
            return sonic_json(&json!({ "challenge": challenge }));
        }
    }

    // Optional verification token check
    let header_token = body
        .pointer("/header/token")
        .or_else(|| body.get("token"))
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    if let Some(expected) = state.bot.cfg().verification_token.clone() {
        if !expected.is_empty() && header_token != expected {
            warn!("rejecting event with bad verification token");
            return (StatusCode::UNAUTHORIZED, "bad token").into_response();
        }
    }

    let event_type = body
        .pointer("/header/event_type")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    match event_type {
        "im.message.receive_v1" => {
            if let Some(msg) = parse_inbound_message(&body) {
                let bot = state.bot.clone();
                tokio::spawn(async move {
                    if let Err(e) = bot.handle_message(msg).await {
                        warn!(?e, "message handler error");
                    }
                });
            }
        }
        "im.chat.member.user.added_v1" => {
            if let Some(evt) = parse_member_added(&body) {
                let bot = state.bot.clone();
                tokio::spawn(async move {
                    if let Err(e) = bot.handle_member_added(evt).await {
                        warn!(?e, "member_added handler error");
                    }
                });
            }
        }
        // url_verification can also arrive in v2 schema header form
        "url_verification" => {
            if let Some(c) = body.pointer("/event/challenge").and_then(|v| v.as_str()) {
                return sonic_json(&json!({ "challenge": c }));
            }
        }
        other => {
            info!(event = other, "ignoring event");
        }
    }

    sonic_json(&json!({ "ok": true }))
}

async fn card_handler(State(state): State<AppState>, body_bytes: Bytes) -> impl IntoResponse {
    let body = parse_body(&body_bytes);

    // Card action callbacks may also include url_verification during setup.
    if body.get("type").and_then(|v| v.as_str()) == Some("url_verification") {
        if let Some(challenge) = body.get("challenge").and_then(|v| v.as_str()) {
            return sonic_json(&json!({ "challenge": challenge }));
        }
    }

    let header_token = body
        .pointer("/header/token")
        .or_else(|| body.get("token"))
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    if let Some(expected) = state.bot.cfg().verification_token.clone() {
        if !expected.is_empty() && header_token != expected {
            return (StatusCode::UNAUTHORIZED, "bad token").into_response();
        }
    }

    let Some(action) = parse_card_action(&body) else {
        return sonic_json(&json!({}));
    };

    match state.bot.clone().handle_card_action(action).await {
        Ok(value) => sonic_json(&value),
        Err(e) => {
            warn!(?e, "card handler error");
            sonic_json(&json!({
                "toast": { "type": "error", "content": format!("{e}") }
            }))
        }
    }
}
