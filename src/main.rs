mod bot;
mod config;
mod feishu;
mod game;
mod llm;
mod poker;
mod server;
mod storage;
mod util;
mod werewolf;

use anyhow::Result;
use std::time::Duration;

// Global allocator. mimalloc consistently beats glibc malloc on
// short-lived alloc-heavy workloads (JSON parse/encode, webhook task
// spawning, LLM message construction). Free 5–15 % p99 latency win.
#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let cfg = config::Config::from_env()?;
    let bind_addr = cfg.bind_addr.clone();

    let client = feishu::Client::new(cfg.app_id.clone(), cfg.app_secret.clone());
    // Warm up token & resolve bot's own open_id (best-effort).
    let _ = client.tenant_access_token().await?;
    if let Ok(id) = lookup_bot_open_id(&client).await {
        tracing::info!("bot open_id = {id}");
    }

    let store = storage::Store::open(std::path::Path::new(&cfg.db_path))?;
    tracing::info!(path = %cfg.db_path, "persistent store opened");
    let bot = bot::Bot::new(client.clone(), cfg, store);
    if let Ok(id) = lookup_bot_open_id(&client).await {
        bot.set_bot_open_id(id);
    }

    // One-off `--mock <recipient_open_id>` mode: blast all card mocks to the
    // configured chat (ALLOWED_CHAT_ID) and exit. The recipient is the user
    // who will receive ephemeral cards (welcome / hole cards / actor prompt
    // / help / chips / error feedback) — pass any human open_id in the chat.
    let args: Vec<String> = std::env::args().collect();
    if args.len() >= 2 && args[1] == "--mock" {
        let chat_id = bot
            .cfg()
            .allowed_chat_id
            .clone()
            .ok_or_else(|| anyhow::anyhow!("ALLOWED_CHAT_ID must be set in .env for --mock"))?;
        let recipient = args.get(2).cloned().ok_or_else(|| {
            anyhow::anyhow!(
                "usage: larkriver --mock <recipient_open_id>\n\
                 (open_id of a real user in the chat, who will receive the ephemeral mocks)"
            )
        })?;
        tracing::info!("sending mock cards to chat={chat_id} recipient={recipient}");
        bot.send_all_mocks(&chat_id, &recipient).await?;
        return Ok(());
    }

    // `--debug-lobby <name|open_id>`: 把代表性的大厅卡（8 个按钮）作为
    // ephemeral 发给指定用户。用于在不重新部署的前提下迭代 UI 布局。
    // 参数若以 `ou_` 开头视为 open_id；否则当作群成员姓名查表。
    if args.len() >= 2 && args[1] == "--debug-lobby" {
        let chat_id = bot
            .cfg()
            .allowed_chat_id
            .clone()
            .ok_or_else(|| anyhow::anyhow!("ALLOWED_CHAT_ID must be set in .env"))?;
        let target = args.get(2).cloned().ok_or_else(|| {
            anyhow::anyhow!(
                "usage: larkriver --debug-lobby <name|open_id>\n\
                 (name = 群成员姓名, 如 \"高国泰\"；open_id 直接以 ou_ 开头)"
            )
        })?;
        let recipient_oid = if target.starts_with("ou_") {
            target.clone()
        } else {
            tracing::info!(name = %target, "looking up chat member by name");
            let members = client.list_chat_members(&chat_id).await?;
            members
                .iter()
                .find(|(_, name)| name == &target)
                .map(|(oid, _)| oid.clone())
                .ok_or_else(|| {
                    let names: Vec<String> = members.iter().map(|(_, n)| n.clone()).collect();
                    anyhow::anyhow!(
                        "群里没找到名字 \"{target}\"。\n群成员：{}",
                        names.join(" / ")
                    )
                })?
        };
        tracing::info!(
            "sending debug lobby card to chat={chat_id} recipient={recipient_oid}"
        );
        bot.send_debug_lobby(&chat_id, &recipient_oid).await?;
        return Ok(());
    }

    server::run(bot, &bind_addr).await
}

fn init_tracing() {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("larkriver=info,tower_http=info"));
    fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_thread_ids(false)
        .init();
}

async fn lookup_bot_open_id(client: &feishu::Client) -> Result<String> {
    let token = client.tenant_access_token().await?;
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()?;
    let resp: serde_json::Value = http
        .get("https://open.feishu.cn/open-apis/bot/v3/info")
        .bearer_auth(&token)
        .send()
        .await?
        .json()
        .await?;
    Ok(resp["bot"]["open_id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("no bot.open_id in {resp}"))?
        .to_string())
}
