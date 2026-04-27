//! LLM-driven decision maker for the AI seat. Uses the `async-openai` SDK
//! against any OpenAI-compatible endpoint (OpenAI, DeepSeek, Doubao /
//! 火山引擎, OpenRouter, vLLM, …) by overriding the API base.

use crate::game::{Persona, PlayerAction};
use crate::poker::{Card, DeckMode};
use anyhow::{anyhow, Context, Result};
use async_openai::{
    config::OpenAIConfig,
    types::{
        ChatCompletionRequestSystemMessageArgs, ChatCompletionRequestUserMessageArgs,
        CreateChatCompletionRequestArgs, ResponseFormat,
    },
    Client,
};
use serde::Deserialize;
use tracing::warn;

pub struct LlmClient {
    client: Client<OpenAIConfig>,
    model: String,
}

/// Snapshot of everything the AI needs to make a decision.
pub struct DecisionContext {
    pub mode: DeckMode,
    pub stage: String,
    pub hand_count: u32,
    pub pot: u64,
    pub current_bet: u64,
    pub min_raise: u64,
    pub big_blind: u64,
    pub my_name: String,
    pub my_stack: u64,
    pub my_bet_in_round: u64,
    pub to_call: u64,
    pub my_max_to: u64,    // largest legal raise_to (= chips + bet_in_round)
    pub min_raise_to: u64, // smallest legal raise_to
    pub hole: Vec<Card>,
    pub community: Vec<Card>,
    pub equity: Option<f64>,
    /// Persona archetype shaping decision style.
    pub persona: Option<Persona>,
    /// One line per other player: name / stack / status / bet this round.
    pub others: Vec<String>,
    /// Recent action log lines for this hand.
    pub history: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct RawDecision {
    action: String,
    #[serde(default)]
    raise_to: Option<u64>,
    #[serde(default)]
    #[allow(dead_code)]
    thinking: Option<String>,
    #[serde(default)]
    quip: Option<String>,
}

/// Final decision the bot consumes — the legal action plus an optional
/// in-character one-liner the AI wants to drop in the chat.
#[derive(Debug, Clone)]
pub struct AiDecision {
    pub action: PlayerAction,
    pub quip: Option<String>,
}

impl LlmClient {
    pub fn new(api_key: String, base_url: String, model: String) -> Self {
        let config = OpenAIConfig::new()
            .with_api_key(api_key)
            .with_api_base(base_url);
        let client = Client::with_config(config);
        Self { client, model }
    }

    /// Ask the LLM to pick an action. Returns a clamped, legal action plus
    /// an optional persona-flavoured one-liner the bot may post to the chat.
    /// Falls back to "check if free, otherwise fold" + no quip on failure.
    pub async fn decide(&self, ctx: &DecisionContext) -> AiDecision {
        match self.try_decide(ctx).await {
            Ok(d) => d,
            Err(e) => {
                warn!(?e, "LLM decision failed, falling back");
                AiDecision {
                    action: fallback_action(ctx),
                    quip: None,
                }
            }
        }
    }

    async fn try_decide(&self, ctx: &DecisionContext) -> Result<AiDecision> {
        let system = system_prompt(ctx.persona);
        let prompt = build_prompt(ctx);
        let req = CreateChatCompletionRequestArgs::default()
            .model(&self.model)
            .messages([
                ChatCompletionRequestSystemMessageArgs::default()
                    .content(system)
                    .build()?
                    .into(),
                ChatCompletionRequestUserMessageArgs::default()
                    .content(prompt)
                    .build()?
                    .into(),
            ])
            .response_format(ResponseFormat::JsonObject)
            // Higher temperature so the AI's play has personality / variance —
            // casual chat-room poker is more fun if the bot doesn't always pick
            // the same action in the same spot. reasoning_effort defaults to
            // "high" for normal requests on V4 models (only auto-bumps to
            // "max" for agent-style calls), which is what we want — leaving
            // it unset.
            .temperature(0.9)
            .max_tokens(200u32)
            .build()?;

        let response = self.client.chat().create(req).await?;
        let content = response
            .choices
            .first()
            .and_then(|c| c.message.content.clone())
            .ok_or_else(|| anyhow!("empty LLM response"))?;
        let raw: RawDecision = serde_json::from_str(&content)
            .with_context(|| format!("LLM bad JSON: {content}"))?;
        let quip = raw
            .quip
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            // Sanity cap so the LLM doesn't paste an essay.
            .map(|s| s.chars().take(60).collect::<String>());
        let action = clamp_to_legal(raw, ctx);
        Ok(AiDecision { action, quip })
    }
}

/// Build the system prompt, injecting the persona archetype when set so the
/// AI plays a distinguishable style instead of a vanilla "GTO-ish" line.
fn system_prompt(persona: Option<Persona>) -> String {
    let persona_block = match persona {
        Some(p) => format!(
            "\n\n## 你的风格 — {}\n{}\n按这个风格做决策。可以偶尔反向出牌避免被读穿，\
             但整体行为应该明显偏向你的风格。",
            p.label(),
            p.description()
        ),
        None => String::new(),
    };
    format!("{BASE_SYSTEM_PROMPT}{persona_block}")
}

const BASE_SYSTEM_PROMPT: &str = r#"你是德州扑克 AI 玩家。仅返回单个有效 JSON 对象，不要任何其他文字、不要 markdown、不要代码块包装。

## 返回 JSON 结构

{
  "action": "fold" | "check" | "call" | "raise" | "allin",
  "raise_to": <整数, 仅当 action="raise" 时填，表示加注后你这一轮的总下注金额>,
  "thinking": "<一句话内部决策理由，可选, 群里看不到>",
  "quip": "<可选的群聊俏皮话, 不想说就 null>"
}

## 关于 quip

这是你想随口说的一句话，会以 "💬 你: <quip>" 发到群里给其他玩家看。
- 像真人在群聊打字一样，别像扑克节目解说。可以短、可以口语、可以带语气词
- ≤ 30 字、一句话、中文（带 emoji 也行但别堆）
- **大部分时候应该是 null** —— 一直说话很烦。平均每 4 个动作冒一句的节奏
- 弃牌 / 普通 check / 普通 call 通常 null
- 加注 / 全押 / 局面有意思的关键决策可以偶尔来一句
- 你的风格在上面已经写了，按那个语气说，不用解释自己是什么风格

## quip 示例

{"action": "raise", "raise_to": 80, "thinking": "AK 强牌", "quip": "我加点"}
{"action": "raise", "raise_to": 200, "thinking": "他在偷池", "quip": "你这牌不像有戏"}
{"action": "allin", "thinking": "短码值得搏", "quip": "行 我跟你拼了"}
{"action": "call", "thinking": "底池赔率合适", "quip": "嗯 跟"}
{"action": "call", "thinking": "看下一张", "quip": null}
{"action": "fold", "thinking": "牌太烂", "quip": "这把先撤"}
{"action": "fold", "thinking": "明显被打到了", "quip": null}
{"action": "check", "thinking": "免费看牌", "quip": null}
{"action": "check", "thinking": "陷阱一下", "quip": "过"}

## 约束 (违反会被拒)

- 当前 to_call=0 时只能 check / raise / allin (不能 fold/call)
- 当前 to_call>0 时只能 fold / call / raise / allin (不能 check)
- raise_to 必须 ≥ min_raise_to 且 ≤ my_max_to
- 决策应基于 equity / 底池赔率 / 位置 / 对手行动倾向，不要纯随机"#;

fn build_prompt(c: &DecisionContext) -> String {
    let mode_str = match c.mode {
        DeckMode::Standard => "标准 (52 张)",
        DeckMode::ShortDeck => "短牌 / 6+ Hold'em (36 张, 同花>葫芦, 三条>顺子, A-6-7-8-9 是顺子)",
    };
    let hole_str = c.hole.iter().map(|c| c.label()).collect::<Vec<_>>().join(" ");
    let community_str = if c.community.is_empty() {
        "（暂无）".to_string()
    } else {
        c.community.iter().map(|c| c.label()).collect::<Vec<_>>().join(" ")
    };
    let equity_str = c
        .equity
        .map(|e| format!("{:.0}%", e * 100.0))
        .unwrap_or_else(|| "未计算".into());
    let others = if c.others.is_empty() {
        "（无）".into()
    } else {
        c.others.join("\n  ")
    };
    let history = if c.history.is_empty() {
        "（本手尚无行动）".into()
    } else {
        c.history.join("\n  ")
    };
    format!(
        "## 牌局\n\
         模式: {mode}\n\
         阶段: {stage} (第 {hand} 局)\n\
         底池: {pot} | 当前下注: {cb} | 大盲: {bb}\n\
         公共牌: {community}\n\n\
         ## 你 ({me})\n\
         筹码: {stack} | 本轮已下: {my_bet} | 需要跟注: {tc}\n\
         手牌: {hole}\n\
         胜率 (Monte Carlo, vs 随机对手): {eq}\n\
         合法加注金额范围: {min_to} ≤ raise_to ≤ {max_to}\n\n\
         ## 其他玩家\n  {others}\n\n\
         ## 本手行动历史\n  {history}",
        mode = mode_str,
        stage = c.stage,
        hand = c.hand_count,
        pot = c.pot,
        cb = c.current_bet,
        bb = c.big_blind,
        community = community_str,
        me = c.my_name,
        stack = c.my_stack,
        my_bet = c.my_bet_in_round,
        tc = c.to_call,
        hole = hole_str,
        eq = equity_str,
        min_to = c.min_raise_to,
        max_to = c.my_max_to,
        others = others,
        history = history,
    )
}

/// Coerce the LLM's raw output into a legal `PlayerAction`. Anything we don't
/// recognise (or that violates the betting rules for the current state) is
/// downgraded toward fold/check rather than crashing the hand.
fn clamp_to_legal(raw: RawDecision, c: &DecisionContext) -> PlayerAction {
    match raw.action.to_lowercase().as_str() {
        "fold" => {
            if c.to_call == 0 {
                PlayerAction::Check
            } else {
                PlayerAction::Fold
            }
        }
        "check" => {
            if c.to_call == 0 {
                PlayerAction::Check
            } else {
                fallback_action(c)
            }
        }
        "call" => {
            if c.to_call == 0 {
                PlayerAction::Check
            } else if c.my_stack <= c.to_call {
                PlayerAction::AllIn
            } else {
                PlayerAction::Call
            }
        }
        "allin" | "all_in" | "all-in" => PlayerAction::AllIn,
        "raise" | "bet" => {
            let want = raw.raise_to.unwrap_or(c.min_raise_to);
            let want = want.max(c.min_raise_to).min(c.my_max_to);
            if want >= c.my_max_to {
                PlayerAction::AllIn
            } else if c.my_stack <= c.to_call {
                PlayerAction::AllIn
            } else {
                PlayerAction::RaiseTo(want)
            }
        }
        _ => fallback_action(c),
    }
}

/// Default safe action when the LLM fails or returns garbage: check if it's
/// free, otherwise fold.
fn fallback_action(c: &DecisionContext) -> PlayerAction {
    if c.to_call == 0 {
        PlayerAction::Check
    } else {
        PlayerAction::Fold
    }
}
