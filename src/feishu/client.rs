use anyhow::{anyhow, Context, Result};
use parking_lot::Mutex;
use reqwest::Client as HttpClient;
use serde_json::Value;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub struct Client {
    app_id: String,
    app_secret: String,
    http: HttpClient,
    token: Mutex<Option<(String, Instant)>>,
}

impl Client {
    pub fn new(app_id: String, app_secret: String) -> Arc<Self> {
        let http = HttpClient::builder()
            .timeout(Duration::from_secs(15))
            .build()
            .expect("reqwest client");
        Arc::new(Self {
            app_id,
            app_secret,
            http,
            token: Mutex::new(None),
        })
    }

    pub async fn tenant_access_token(&self) -> Result<String> {
        {
            let guard = self.token.lock();
            if let Some((token, exp)) = guard.as_ref() {
                if *exp > Instant::now() {
                    return Ok(token.clone());
                }
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
        *self.token.lock() = Some((token.clone(), exp));
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
        let body = serde_json::json!({
            "receive_id": receive_id,
            "msg_type": msg_type,
            "content": content.to_string(),
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

    /// Replace an interactive card's content.
    pub async fn update_card(&self, message_id: &str, card: &Value) -> Result<()> {
        let token = self.tenant_access_token().await?;
        let url = format!("https://open.feishu.cn/open-apis/im/v1/messages/{message_id}");
        let body = serde_json::json!({ "content": card.to_string() });
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
