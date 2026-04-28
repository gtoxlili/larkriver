use anyhow::{anyhow, Context, Result};
use arc_swap::ArcSwap;
use reqwest::Client as HttpClient;
use serde_json::Value;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

/// Cached tenant access token + its hard expiry instant.
struct CachedToken {
    value: String,
    expires_at: Instant,
}

pub struct Client {
    app_id: String,
    app_secret: String,
    http: HttpClient,
    /// Lock-free fast path for the tenant access token. Reads on every API
    /// call (high-fan-out) become an `Arc` clone via `ArcSwap::load_full`
    /// instead of acquiring a `Mutex`. The refresh path serialises behind
    /// `refresh_lock` so we don't fan out N concurrent token-fetch RPCs
    /// the moment the cached token expires.
    token: ArcSwap<Option<CachedToken>>,
    refresh_lock: Mutex<()>,
}

impl Client {
    pub fn new(app_id: String, app_secret: String) -> Arc<Self> {
        // Tuned reqwest builder: HTTP/2 keepalive + brotli/gzip/zstd content
        // decoding. Pool the same connection across all Feishu API calls
        // (token refresh, send / patch / delete cards, list members).
        // HTTP/2 negotiation happens via ALPN over rustls (the `http2`
        // feature flag in Cargo.toml is what flips it on). We additionally
        // turn on a generous connection pool, TCP_NODELAY for snappier
        // small-payload latency, and brotli/gzip/zstd at the codec layer
        // so the runtime saves bandwidth on the 10–100 KB card payloads.
        let http = HttpClient::builder()
            .timeout(Duration::from_secs(15))
            .pool_idle_timeout(Some(Duration::from_secs(90)))
            .pool_max_idle_per_host(8)
            .tcp_nodelay(true)
            .build()
            .expect("reqwest client");
        Arc::new(Self {
            app_id,
            app_secret,
            http,
            token: ArcSwap::new(Arc::new(None)),
            refresh_lock: Mutex::new(()),
        })
    }

    pub async fn tenant_access_token(&self) -> Result<String> {
        // Fast path: hit the lock-free snapshot first. If a still-fresh
        // token is cached, clone the small `String` and return it.
        if let Some(cached) = self.token.load_full().as_ref() {
            if cached.expires_at > Instant::now() {
                return Ok(cached.value.clone());
            }
        }

        // Refresh path: only one task at a time fetches a new token.
        // The double-check inside the guard avoids redundant RPCs once
        // a winner has populated the cache.
        let _guard = self.refresh_lock.lock().await;
        if let Some(cached) = self.token.load_full().as_ref() {
            if cached.expires_at > Instant::now() {
                return Ok(cached.value.clone());
            }
        }

        let resp: Value = self
            .http
            .post("https://open.feishu.cn/open-apis/auth/v3/tenant_access_token/internal")
            .json(&serde_json::json!({
                "app_id": self.app_id,
                "app_secret": self.app_secret,
            }))
            .send()
            .await?
            .json()
            .await?;

        if resp["code"].as_i64().unwrap_or(-1) != 0 {
            return Err(anyhow!("auth failed: {resp}"));
        }
        let token = resp["tenant_access_token"]
            .as_str()
            .context("missing tenant_access_token")?
            .to_string();
        let expire = resp["expire"].as_u64().unwrap_or(7200);
        let exp = Instant::now() + Duration::from_secs(expire.saturating_sub(60));
        let snapshot = CachedToken {
            value: token.clone(),
            expires_at: exp,
        };
        self.token.store(Arc::new(Some(snapshot)));
        Ok(token)
    }

    /// Send a message. `content` will be JSON-stringified into the API's content field.
    /// Returns the new message_id.
    pub async fn send_message(
        &self,
        receive_id_type: &str,
        receive_id: &str,
        msg_type: &str,
        content: &Value,
    ) -> Result<String> {
        let token = self.tenant_access_token().await?;
        let url = format!(
            "https://open.feishu.cn/open-apis/im/v1/messages?receive_id_type={receive_id_type}"
        );
        // sonic-rs stringify for the inner content field — this is an
        // *escaped* JSON string the API double-decodes server-side, so
        // we want it as compact and fast as possible.
        let content_str = sonic_rs::to_string(content).unwrap_or_else(|_| content.to_string());
        let body = serde_json::json!({
            "receive_id": receive_id,
            "msg_type": msg_type,
            "content": content_str,
            "uuid": uuid::Uuid::new_v4().to_string(),
        });
        let resp: Value = self
            .http
            .post(&url)
            .bearer_auth(&token)
            .json(&body)
            .send()
            .await?
            .json()
            .await?;
        if resp["code"].as_i64().unwrap_or(-1) != 0 {
            return Err(anyhow!("send_message failed: {resp}"));
        }
        Ok(resp["data"]["message_id"]
            .as_str()
            .unwrap_or("")
            .to_string())
    }

/// Send a card visible only to one user inside a group chat.
    /// Returns the new message_id (this is a normal message id, not threaded).
    pub async fn send_ephemeral_card(
        &self,
        chat_id: &str,
        open_id: &str,
        card: &Value,
    ) -> Result<String> {
        let token = self.tenant_access_token().await?;
        let body = serde_json::json!({
            "chat_id": chat_id,
            "open_id": open_id,
            "msg_type": "interactive",
            "card": card,
        });
        let resp: Value = self
            .http
            .post("https://open.feishu.cn/open-apis/ephemeral/v1/send")
            .bearer_auth(&token)
            .json(&body)
            .send()
            .await?
            .json()
            .await?;
        if resp["code"].as_i64().unwrap_or(-1) != 0 {
            return Err(anyhow!("send_ephemeral_card failed: {resp}"));
        }
        Ok(resp["data"]["message_id"]
            .as_str()
            .unwrap_or("")
            .to_string())
    }

    /// 删除一张 ephemeral 卡片。ephemeral 卡片（om_x 前缀的 message_id）
    /// 不能用标准 PATCH /im/v1/messages/{id} 接口更新——会返回 230001
    /// invalid message_id。所以更新一张 ephemeral 必须 delete 旧的 + send 新的。
    pub async fn delete_ephemeral(&self, message_id: &str) -> Result<()> {
        let token = self.tenant_access_token().await?;
        let body = serde_json::json!({ "message_id": message_id });
        let resp: Value = self
            .http
            .post("https://open.feishu.cn/open-apis/ephemeral/v1/delete")
            .bearer_auth(&token)
            .json(&body)
            .send()
            .await?
            .json()
            .await?;
        if resp["code"].as_i64().unwrap_or(-1) != 0 {
            return Err(anyhow!("delete_ephemeral failed: {resp}"));
        }
        Ok(())
    }

    /// Replace an interactive card's content.
    pub async fn update_card(&self, message_id: &str, card: &Value) -> Result<()> {
        let token = self.tenant_access_token().await?;
        let url = format!("https://open.feishu.cn/open-apis/im/v1/messages/{message_id}");
        // SIMD stringify of the (potentially deep) card body.
        let card_str = sonic_rs::to_string(card).unwrap_or_else(|_| card.to_string());
        let body = serde_json::json!({ "content": card_str });
        let resp: Value = self
            .http
            .patch(&url)
            .bearer_auth(&token)
            .json(&body)
            .send()
            .await?
            .json()
            .await?;
        if resp["code"].as_i64().unwrap_or(-1) != 0 {
            return Err(anyhow!("update_card failed: {resp}"));
        }
        Ok(())
    }

    /// Resolve a user's display name from their open_id (best-effort).
    pub async fn user_name(&self, open_id: &str) -> Result<String> {
        let token = self.tenant_access_token().await?;
        let url = format!(
            "https://open.feishu.cn/open-apis/contact/v3/users/{open_id}?user_id_type=open_id"
        );
        let resp: Value = self
            .http
            .get(&url)
            .bearer_auth(&token)
            .send()
            .await?
            .json()
            .await?;
        if resp["code"].as_i64().unwrap_or(-1) != 0 {
            return Err(anyhow!("user_name failed: {resp}"));
        }
        Ok(resp["data"]["user"]["name"]
            .as_str()
            .unwrap_or("玩家")
            .to_string())
    }

    /// 列出群成员 (open_id, name)。需要 `im:chat:readonly` 权限。分页拉到尾。
    pub async fn list_chat_members(&self, chat_id: &str) -> Result<Vec<(String, String)>> {
        let token = self.tenant_access_token().await?;
        let mut out: Vec<(String, String)> = vec![];
        let mut page_token: Option<String> = None;
        loop {
            let mut url = format!(
                "https://open.feishu.cn/open-apis/im/v1/chats/{chat_id}/members?\
                 member_id_type=open_id&page_size=100"
            );
            if let Some(pt) = &page_token {
                url.push_str(&format!("&page_token={pt}"));
            }
            let resp: Value = self
                .http
                .get(&url)
                .bearer_auth(&token)
                .send()
                .await?
                .json()
                .await?;
            if resp["code"].as_i64().unwrap_or(-1) != 0 {
                return Err(anyhow!("list_chat_members failed: {resp}"));
            }
            if let Some(items) = resp["data"]["items"].as_array() {
                for item in items {
                    let oid = item["member_id"].as_str().unwrap_or("").to_string();
                    let name = item["name"].as_str().unwrap_or("").to_string();
                    if !oid.is_empty() {
                        out.push((oid, name));
                    }
                }
            }
            let has_more = resp["data"]["has_more"].as_bool().unwrap_or(false);
            if !has_more {
                break;
            }
            page_token = resp["data"]["page_token"].as_str().map(String::from);
            if page_token.is_none() {
                break;
            }
        }
        Ok(out)
    }
}

