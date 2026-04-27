use anyhow::{Context, Result};

#[derive(Debug, Clone)]
pub struct Config {
    pub app_id: String,
    pub app_secret: String,
    pub bind_addr: String,
    pub allowed_chat_id: Option<String>,
    pub verification_token: Option<String>,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let _ = load_dotenv();
        Ok(Self {
            app_id: std::env::var("FEISHU_APP_ID").context("FEISHU_APP_ID is required")?,
            app_secret: std::env::var("FEISHU_APP_SECRET").context("FEISHU_APP_SECRET is required")?,
            bind_addr: std::env::var("BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".to_string()),
            allowed_chat_id: env_opt("ALLOWED_CHAT_ID"),
            verification_token: env_opt("FEISHU_VERIFICATION_TOKEN"),
        })
    }
}

fn env_opt(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

fn load_dotenv() -> Result<()> {
    let contents = std::fs::read_to_string(".env")?;
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            let v = v.trim().trim_matches('"').trim_matches('\'');
            if std::env::var(k.trim()).is_err() {
                std::env::set_var(k.trim(), v);
            }
        }
    }
    Ok(())
}
