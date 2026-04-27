//! 狼人杀 AI 决策。
//!
//! 每个角色有自己的 prompt 和决策结构：
//! - 狼：每晚选一名非狼存活玩家击杀，可发表夜间内部讨论（不公开）
//! - 预言家：每晚选一名玩家查验
//! - 女巫：知道今晚被刀的人，选 救 / 毒 / 跳过
//! - 投票：白天选一名玩家投票放逐
//! - 猎人：临死时选一名玩家开枪
//! - 讨论 quip：白天 AI 各自发表观点
//!
//! 所有决策走同一个 `LlmClient::chat_json` 入口；构造 prompt 和解析输出在这里完成。

use crate::game::Persona;
use crate::llm::LlmClient;
use crate::werewolf::game::*;
use anyhow::{Context, Result};
use serde::Deserialize;
use tracing::warn;

// ============================================================================
// 公共上下文：每个 AI 都看到的信息
// ============================================================================

/// AI 视角的玩家公开信息（不含其他人身份，除非该 AI 是狼且对方是队友）。
pub struct PublicView<'a> {
    pub day: u32,
    pub stage_label: &'a str,
    pub me_idx: usize,
    pub me_name: &'a str,
    pub me_role: Role,
    pub persona: Option<Persona>,
    /// (idx, name, alive)
    pub players: Vec<(usize, String, bool)>,
    /// 夜里队友信息（仅狼可见）：(idx, name)
    pub teammates: Vec<(usize, String)>,
    /// 预言家自己的查验历史：(day, target_name, is_wolf)
    pub seer_log: Vec<(u32, String, bool)>,
    /// 公开事件日志（死讯 / 放逐 / quip 等）。
    pub event_log: &'a [String],
}

impl<'a> PublicView<'a> {
    /// 标准上下文段落，给所有 prompt 用。
    fn render(&self) -> String {
        let players_str: Vec<String> = self
            .players
            .iter()
            .map(|(_, n, alive)| {
                if *alive {
                    format!("{} (存活)", n)
                } else {
                    format!("{} (出局)", n)
                }
            })
            .collect();
        let mut out = format!(
            "## 局面\n\
             第 {} 天 · 阶段：{}\n\
             你是 **{}**，身份 **{}**\n\
             场上玩家：{}\n",
            self.day,
            self.stage_label,
            self.me_name,
            self.me_role.label(),
            players_str.join(" · ")
        );
        if !self.teammates.is_empty() {
            let names: Vec<String> = self.teammates.iter().map(|(_, n)| n.clone()).collect();
            out.push_str(&format!("狼队友：{}\n", names.join("、")));
        }
        if !self.seer_log.is_empty() {
            out.push_str("\n你之前查验过：\n");
            for (d, n, w) in &self.seer_log {
                out.push_str(&format!(
                    "  第 {} 夜：{} 是 {}\n",
                    d,
                    n,
                    if *w { "狼人" } else { "好人" }
                ));
            }
        }
        if !self.event_log.is_empty() {
            out.push_str("\n## 事件历史\n");
            for line in self.event_log {
                out.push_str("  ");
                out.push_str(line);
                out.push('\n');
            }
        }
        out
    }
}

/// 把 game + 一名 AI 玩家组装成 PublicView。
pub fn build_view<'a>(game: &'a WolfGame, ai_idx: usize) -> PublicView<'a> {
    let p = &game.players[ai_idx];
    let role = p.role.expect("AI player has role");

    let players: Vec<(usize, String, bool)> = game
        .players
        .iter()
        .enumerate()
        .map(|(i, p)| (i, p.name.clone(), p.alive))
        .collect();

    let teammates = if role == Role::Werewolf {
        game.players
            .iter()
            .enumerate()
            .filter(|(i, p)| p.role == Some(Role::Werewolf) && *i != ai_idx)
            .map(|(i, p)| (i, p.name.clone()))
            .collect()
    } else {
        vec![]
    };

    let seer_log = if role == Role::Seer {
        game.seer_history
            .iter()
            .map(|c| {
                (
                    c.day,
                    game.players[c.target_idx].name.clone(),
                    c.is_wolf,
                )
            })
            .collect()
    } else {
        vec![]
    };

    PublicView {
        day: game.day,
        stage_label: game.stage.label(),
        me_idx: ai_idx,
        me_name: &p.name,
        me_role: role,
        persona: p.persona,
        players,
        teammates,
        seer_log,
        event_log: &game.event_log,
    }
}

// ============================================================================
// 通用：retry-with-feedback 消息构造
// ============================================================================

/// 历史 = 已经被拒绝的尝试列表：(上次的答案 JSON 文本, 拒绝原因)。
/// caller 在 retry 时把这个 history 传进来，函数内部把它转成多轮对话——
/// LLM 能看见自己刚才的失败并据此调整选择，而不是被无脑重试。
pub type AttemptHistory = Vec<(String, String)>;

/// 把 system / user / 历次失败 拼成多轮 messages，喂给 LLM。
async fn chat_with_history(
    llm: &LlmClient,
    system: String,
    user: String,
    history: &AttemptHistory,
) -> Result<String> {
    let mut msgs: Vec<(String, String)> = Vec::with_capacity(2 + history.len() * 2);
    msgs.push(("system".into(), system));
    msgs.push(("user".into(), user));
    for (answer, reason) in history {
        msgs.push(("assistant".into(), answer.clone()));
        msgs.push((
            "user".into(),
            format!(
                "⚠️ 上面的答案不合法：{reason}\n请基于这个反馈**重新选择一个合法的答案**，仅返回新的 JSON 对象。"
            ),
        ));
    }
    llm.chat_json_with_messages(&msgs).await
}

// ============================================================================
// 通用：人设段
// ============================================================================

fn persona_line(persona: Option<Persona>) -> String {
    match persona {
        Some(p) => format!(
            "## 你的高玩档案：**{}**\n{}\n\n\
             这是你的流派烙印——你的发言、投票、站警、跳身份的所有决策都要让其他玩家\
             从行为里**读得出你是这个流派的高玩**。不是娱乐玩家，是有套路、能算几层的人。\
             不要打成 GTO 模板化的均衡 AI——风格越鲜明越好。",
            p.label(),
            p.werewolf_description()
        ),
        None => "你没有特定流派，按一般直觉决策。".into(),
    }
}

const RULES: &str = r#"## 规则速览
- **胜负**：好人胜 = 击杀全部狼（含狼王）；狼胜 = 存活狼 > 存活好人，或 1:1 时警长不在好人手上
- **角色**：狼人 / 狼王 / 村民 / 预言家 / 女巫 / 猎人 / 守卫
- **技能**：
  - 预言家每晚验 1 人
  - 女巫一局共 1 瓶救药 + 1 瓶毒药（同晚不可救+毒）
  - 猎人 / 狼王被狼刀或被放逐可开枪，**被毒不能**
  - 守卫每晚守 1 人（含自己），不可连守同人；同守同救会死
- **警长** (10+ 板)：1.5x 票权（整数 = 3 vs 普通 2）；死亡时可移交 / 撕毁警徽
- **预言家查验**：狼王也显示为狼人
- **公开开枪广播不会标记角色**——猎人和狼王共享开枪技能
- **只返回单个 JSON 对象，不要任何其他文字 / markdown / 代码块**"#;

// ============================================================================
// 狼 - 选择目标
// ============================================================================

#[derive(Debug, Deserialize)]
struct WolfPickResp {
    /// 玩家的索引。
    target_idx: usize,
    #[serde(default)]
    chat: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    thinking: Option<String>,
}

/// AI 狼的决策：目标 + 可选的狼频道发言。
#[derive(Debug, Clone)]
pub struct WolfPickDecision {
    pub target_idx: usize,
    /// 队友频道里的一句话；混合局有人类时才会用，全 AI 局可忽略。
    pub chat: Option<String>,
}

/// 让狼 AI 选今晚的目标 + 可能在狼频道里说一句话。
/// `history` 是被拒绝过的尝试历史（retry-with-feedback）。
pub async fn wolf_pick(
    llm: &LlmClient,
    view: &PublicView<'_>,
    game: &WolfGame,
    speak_enabled: bool,
    history: &AttemptHistory,
) -> WolfPickDecision {
    let candidates: Vec<(usize, String)> = view
        .players
        .iter()
        .filter(|(i, _, alive)| {
            *alive && !game.is_wolf(*i)
        })
        .map(|(i, n, _)| (*i, n.clone()))
        .collect();
    if candidates.is_empty() {
        return WolfPickDecision { target_idx: 0, chat: None };
    }
    let fallback = candidates[0].0;

    let teammate_picks: Vec<String> = game
        .wolf_kill_votes
        .iter()
        .filter(|(w, _)| *w != view.me_idx)
        .map(|(w, t)| {
            format!(
                "{} 选择杀 {}",
                game.players[*w].name, game.players[*t].name
            )
        })
        .collect();
    let chat_history: Vec<String> = game
        .wolf_chat
        .iter()
        .map(|(idx, msg)| format!("{}: {}", game.players[*idx].name, msg))
        .collect();

    let chat_section = if speak_enabled {
        "\n\n## 狼频道发言（仅狼可见）\n\
         队伍里有人类狼，你可以在 \"chat\" 字段说一句协调队友的话（≤ 30 字，可选 / 可为 null）。"
    } else {
        ""
    };
    let chat_field = if speak_enabled {
        ", \"chat\": \"<≤30 字发言, 不想说就 null>\""
    } else {
        ""
    };

    let system = format!(
        "你是飞书群里玩狼人杀的玩家，扮演**狼人**。你的阵营是狼，目标是狼方胜利。\n\
         {}\n\n{}\n\n## 任务\n\
         你和狼队友需要合议击杀一名非狼玩家。{}\n\n\
         返回 JSON: {{\"target_idx\": <整数>{}, \"thinking\": \"...\"}}",
        persona_line(view.persona),
        RULES,
        chat_section,
        chat_field
    );
    let candidates_str: Vec<String> = candidates
        .iter()
        .map(|(i, n)| format!("{} = {}", i, n))
        .collect();
    let user = format!(
        "{}\n\n## 候选目标（仅这些 idx 是合法的）\n{}\n\n{}\n\n{}",
        view.render(),
        candidates_str.join("\n"),
        if teammate_picks.is_empty() {
            "队友尚未提交选择".into()
        } else {
            format!("队友进度：\n{}", teammate_picks.join("\n"))
        },
        if chat_history.is_empty() {
            "狼频道暂无发言".into()
        } else {
            format!("狼频道历史：\n{}", chat_history.join("\n"))
        },
    );

    match chat_with_history(llm, system, user, history).await {
        Ok(content) => match serde_json::from_str::<WolfPickResp>(&content) {
            Ok(r) => {
                let target_idx = if candidates.iter().any(|(i, _)| *i == r.target_idx) {
                    r.target_idx
                } else {
                    warn!(target = r.target_idx, "wolf returned illegal idx, fallback");
                    fallback
                };
                let chat = if speak_enabled {
                    r.chat
                        .as_deref()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(|s| s.chars().take(60).collect::<String>())
                } else {
                    None
                };
                WolfPickDecision { target_idx, chat }
            }
            Err(e) => {
                warn!(?e, content = %content, "wolf JSON parse failed");
                WolfPickDecision { target_idx: fallback, chat: None }
            }
        },
        Err(e) => {
            warn!(?e, "wolf LLM call failed");
            WolfPickDecision { target_idx: fallback, chat: None }
        }
    }
}

// ============================================================================
// 预言家 - 查验
// ============================================================================

#[derive(Debug, Deserialize)]
struct SeerCheckResp {
    target_idx: usize,
    #[serde(default)]
    #[allow(dead_code)]
    thinking: Option<String>,
}

pub async fn seer_pick(
    llm: &LlmClient,
    view: &PublicView<'_>,
    history: &AttemptHistory,
) -> usize {
    // 先收集已查过的玩家名（去重提示），让 prompt 引导 LLM 优先查没查过的。
    let already_checked: std::collections::HashSet<&str> = view
        .seer_log
        .iter()
        .map(|(_, n, _)| n.as_str())
        .collect();
    let candidates: Vec<(usize, String)> = view
        .players
        .iter()
        .filter(|(i, _, alive)| *alive && *i != view.me_idx)
        .map(|(i, n, _)| (*i, n.clone()))
        .collect();
    if candidates.is_empty() {
        return 0;
    }
    // fallback 优先选没查过的
    let fallback = candidates
        .iter()
        .find(|(_, n)| !already_checked.contains(n.as_str()))
        .map(|(i, _)| *i)
        .unwrap_or(candidates[0].0);

    let system = format!(
        "你是飞书群里玩狼人杀的玩家，扮演**预言家**。你的阵营是好人，目标是好人胜。\n\
         {}\n\n{}\n\n## 任务\n\
         选一名存活的非自己玩家查验。\n\n\
         返回 JSON: {{\"target_idx\": <整数>, \"thinking\": \"...\"}}",
        persona_line(view.persona),
        RULES
    );
    let candidates_str: Vec<String> = candidates
        .iter()
        .map(|(i, n)| format!("{} = {}", i, n))
        .collect();
    let user = format!(
        "{}\n\n## 候选目标（仅这些 idx 合法）\n{}",
        view.render(),
        candidates_str.join("\n"),
    );

    match chat_with_history(llm, system, user, history).await {
        Ok(content) => match serde_json::from_str::<SeerCheckResp>(&content) {
            Ok(r) => {
                if candidates.iter().any(|(i, _)| *i == r.target_idx) {
                    r.target_idx
                } else {
                    warn!(target = r.target_idx, "seer returned illegal idx, fallback");
                    fallback
                }
            }
            Err(e) => {
                warn!(?e, content = %content, "seer JSON parse failed");
                fallback
            }
        },
        Err(e) => {
            warn!(?e, "seer LLM call failed");
            fallback
        }
    }
}

// ============================================================================
// 女巫
// ============================================================================

#[derive(Debug, Deserialize)]
struct WitchResp {
    /// "save" | "poison" | "skip"
    action: String,
    #[serde(default)]
    poison_target_idx: Option<usize>,
    #[serde(default)]
    #[allow(dead_code)]
    thinking: Option<String>,
}

#[derive(Debug, Clone)]
pub enum WitchDecision {
    Save,
    Poison(usize),
    Skip,
}

pub async fn witch_decide(
    llm: &LlmClient,
    view: &PublicView<'_>,
    game: &WolfGame,
    history: &AttemptHistory,
) -> WitchDecision {
    // 候选毒目标：所有存活的非自己玩家
    let poison_candidates: Vec<(usize, String)> = view
        .players
        .iter()
        .filter(|(i, _, alive)| *alive && *i != view.me_idx)
        .map(|(i, n, _)| (*i, n.clone()))
        .collect();

    let victim_str = match game.night_victim {
        Some(v) => format!("今晚狼刀目标：{}", game.players[v].name),
        None => "今晚狼人空刀，没人需要救".into(),
    };

    let system = format!(
        "你是飞书群里玩狼人杀的玩家，扮演**女巫**。你的阵营是好人，目标是好人胜。\n\
         {}\n\n{}\n\n## 任务\n\
         你看得到今晚被狼刀的人。三选一：\n\
         - save: 用救药救今晚的猎物\n\
         - poison: 用毒药毒一名存活的非自己玩家（需 poison_target_idx）\n\
         - skip: 不动作\n\n\
         返回 JSON: {{\"action\": \"save|poison|skip\", \"poison_target_idx\": <可选整数>, \"thinking\": \"...\"}}",
        persona_line(view.persona),
        RULES
    );
    let pcands: Vec<String> = poison_candidates
        .iter()
        .map(|(i, n)| format!("{} = {}", i, n))
        .collect();
    let user = format!(
        "{}\n\n{}\n\n药剂状态：救药 {} · 毒药 {}\n\n## 毒药候选目标（仅这些 idx 合法）\n{}",
        view.render(),
        victim_str,
        if game.witch_save_used { "✗ 已用" } else { "✓ 可用" },
        if game.witch_poison_used { "✗ 已用" } else { "✓ 可用" },
        pcands.join("\n"),
    );

    let raw = match chat_with_history(llm, system, user, history).await {
        Ok(c) => c,
        Err(e) => {
            warn!(?e, "witch LLM call failed, skip");
            return WitchDecision::Skip;
        }
    };
    let parsed: WitchResp = match serde_json::from_str(&raw) {
        Ok(r) => r,
        Err(e) => {
            warn!(?e, content = %raw, "witch JSON parse failed");
            return WitchDecision::Skip;
        }
    };

    match parsed.action.to_lowercase().as_str() {
        "save" => {
            if !game.witch_save_used && game.night_victim.is_some() {
                WitchDecision::Save
            } else {
                WitchDecision::Skip
            }
        }
        "poison" => {
            if game.witch_poison_used {
                return WitchDecision::Skip;
            }
            let Some(idx) = parsed.poison_target_idx else {
                return WitchDecision::Skip;
            };
            if poison_candidates.iter().any(|(i, _)| *i == idx) {
                WitchDecision::Poison(idx)
            } else {
                WitchDecision::Skip
            }
        }
        _ => WitchDecision::Skip,
    }
}

// ============================================================================
// 白天投票
// ============================================================================

#[derive(Debug, Deserialize)]
struct VoteResp {
    /// 玩家 idx 或 -1 弃权
    target_idx: i64,
    #[serde(default)]
    #[allow(dead_code)]
    thinking: Option<String>,
}

#[derive(Debug, Clone)]
pub struct VoteDecision {
    pub target_idx: Option<usize>,
}

pub async fn vote_pick(
    llm: &LlmClient,
    view: &PublicView<'_>,
    history: &AttemptHistory,
) -> VoteDecision {
    let candidates: Vec<(usize, String)> = view
        .players
        .iter()
        .filter(|(i, _, alive)| *alive && *i != view.me_idx)
        .map(|(i, n, _)| (*i, n.clone()))
        .collect();

    let team = if view.me_role.is_wolf() {
        "你的阵营是狼。"
    } else {
        "你的阵营是好人。"
    };

    let system = format!(
        "你是飞书群里玩狼人杀的玩家，正在白天投票阶段。你的真实身份是 **{}**。{}\n\
         {}\n\n{}\n\n## 任务\n\
         投票放逐你认为应该被处决的玩家，或弃权 (target_idx = -1)。\
         不能投自己。**只投票，不发言**——发言阶段已经结束。\n\n\
         返回 JSON: {{\"target_idx\": <整数 idx 或 -1>, \"thinking\": \"...\"}}",
        view.me_role.label(),
        team,
        persona_line(view.persona),
        RULES
    );
    let cands_str: Vec<String> = candidates
        .iter()
        .map(|(i, n)| format!("{} = {}", i, n))
        .collect();
    let user = format!(
        "{}\n\n## 候选目标（idx 合法值）\n{}\n-1 = 弃权",
        view.render(),
        cands_str.join("\n"),
    );

    let raw = match chat_with_history(llm, system, user, history).await {
        Ok(c) => c,
        Err(e) => {
            warn!(?e, "vote LLM call failed, abstain");
            return VoteDecision { target_idx: None };
        }
    };
    let parsed: VoteResp = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            warn!(?e, content = %raw, "vote JSON parse failed");
            return VoteDecision { target_idx: None };
        }
    };
    let target = if parsed.target_idx < 0 {
        None
    } else {
        let idx = parsed.target_idx as usize;
        if candidates.iter().any(|(i, _)| *i == idx) {
            Some(idx)
        } else {
            None
        }
    };
    VoteDecision { target_idx: target }
}

// ============================================================================
// 猎人
// ============================================================================

#[derive(Debug, Deserialize)]
struct HunterResp {
    /// 目标 idx 或 -1 不开枪
    target_idx: i64,
    #[serde(default)]
    #[allow(dead_code)]
    thinking: Option<String>,
}

pub async fn hunter_pick(
    llm: &LlmClient,
    view: &PublicView<'_>,
    history: &AttemptHistory,
) -> Option<usize> {
    let candidates: Vec<(usize, String)> = view
        .players
        .iter()
        .filter(|(i, _, alive)| *alive && *i != view.me_idx)
        .map(|(i, n, _)| (*i, n.clone()))
        .collect();
    if candidates.is_empty() {
        return None;
    }
    let team = if view.me_role.is_wolf() {
        "你的阵营是狼。"
    } else {
        "你的阵营是好人。"
    };
    let system = format!(
        "你是飞书群里的狼人杀玩家，扮演 **{}**，刚刚死亡。{}\n\
         {}\n\n{}\n\n## 任务\n\
         可以选一名存活的非自己玩家开枪带走，或选择不开枪 (target_idx = -1)。\n\
         注：公开广播只显示 \"X 临死前开枪带走 Y\"，不会标记你的身份。\n\n\
         返回 JSON: {{\"target_idx\": <整数 idx 或 -1>, \"thinking\": \"...\"}}",
        view.me_role.label(),
        team,
        persona_line(view.persona),
        RULES
    );
    let cands: Vec<String> = candidates
        .iter()
        .map(|(i, n)| format!("{} = {}", i, n))
        .collect();
    let user = format!(
        "{}\n\n## 候选目标\n{}\n-1 = 不开枪",
        view.render(),
        cands.join("\n"),
    );

    let raw = match chat_with_history(llm, system, user, history).await {
        Ok(c) => c,
        Err(e) => {
            warn!(?e, "hunter LLM call failed");
            return None;
        }
    };
    let parsed: HunterResp = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            warn!(?e, content = %raw, "hunter JSON parse failed");
            return None;
        }
    };
    if parsed.target_idx < 0 {
        return None;
    }
    let idx = parsed.target_idx as usize;
    if candidates.iter().any(|(i, _)| *i == idx) {
        Some(idx)
    } else {
        None
    }
}

// ============================================================================
// 守卫
// ============================================================================

#[derive(Debug, Deserialize)]
struct GuardResp {
    target_idx: usize,
    #[serde(default)]
    #[allow(dead_code)]
    thinking: Option<String>,
}

pub async fn guard_pick(
    llm: &LlmClient,
    view: &PublicView<'_>,
    game: &WolfGame,
    history: &AttemptHistory,
) -> usize {
    let candidates: Vec<(usize, String)> = view
        .players
        .iter()
        .filter(|(i, _, alive)| *alive && game.last_guard_target != Some(*i))
        .map(|(i, n, _)| (*i, n.clone()))
        .collect();
    if candidates.is_empty() {
        return view.me_idx;
    }
    let fallback = candidates
        .iter()
        .find(|(i, _)| *i == view.me_idx)
        .map(|(i, _)| *i)
        .unwrap_or(candidates[0].0);

    let last_guard_str = game
        .last_guard_target
        .map(|i| format!("（昨夜守过：{}，本夜不可再守）", game.players[i].name))
        .unwrap_or_else(|| "（首夜，无限制）".to_string());

    let system = format!(
        "你是飞书群里玩狼人杀的玩家，扮演**守卫**。你的阵营是好人，目标是好人胜。\n\
         {}\n\n{}\n\n## 任务\n\
         选一名存活玩家守护（含自己），不能连续两晚守同一个人。\n\n\
         返回 JSON: {{\"target_idx\": <整数>, \"thinking\": \"...\"}}",
        persona_line(view.persona),
        RULES
    );
    let cands: Vec<String> = candidates
        .iter()
        .map(|(i, n)| format!("{} = {}", i, n))
        .collect();
    let user = format!(
        "{}\n\n{}\n\n## 候选目标\n{}",
        view.render(),
        last_guard_str,
        cands.join("\n"),
    );
    match chat_with_history(llm, system, user, history).await {
        Ok(c) => match serde_json::from_str::<GuardResp>(&c) {
            Ok(r) if candidates.iter().any(|(i, _)| *i == r.target_idx) => r.target_idx,
            _ => fallback,
        },
        Err(e) => {
            warn!(?e, "guard LLM call failed");
            fallback
        }
    }
}

// ============================================================================
// 上警 / 警长投票 / 警徽流转
// ============================================================================

#[derive(Debug, Deserialize)]
struct SheriffRunResp {
    run: bool,
    #[serde(default)]
    #[allow(dead_code)]
    thinking: Option<String>,
}

/// AI 决定是否上警。预言家通常会上（领跳），狼也可能上去抢警；村民/女巫/猎人/守卫看局势。
pub async fn sheriff_run(llm: &LlmClient, view: &PublicView<'_>) -> bool {
    let team = if view.me_role.is_wolf() {
        "你的阵营是狼。"
    } else {
        "你的阵营是好人。"
    };
    let system = format!(
        "你是飞书群里玩狼人杀的玩家，正在第 1 天上警阶段。\
         你的真实身份是 **{}**。{}\n\
         {}\n\n{}\n\n## 任务\n\
         决定是否参选警长。警长有 1.5x 票权，死亡时可移交 / 撕毁警徽。\n\n\
         返回 JSON: {{\"run\": <true/false>, \"thinking\": \"...\"}}",
        view.me_role.label(),
        team,
        persona_line(view.persona),
        RULES
    );
    let user = view.render();
    match llm.chat_json(&system, &user).await {
        Ok(c) => match serde_json::from_str::<SheriffRunResp>(&c) {
            Ok(r) => r.run,
            Err(e) => {
                warn!(?e, content = %c, "sheriff_run JSON parse failed");
                view.me_role == Role::Seer
            }
        },
        Err(e) => {
            warn!(?e, "sheriff_run LLM call failed");
            view.me_role == Role::Seer
        }
    }
}

#[derive(Debug, Deserialize)]
struct SheriffVoteResp {
    target_idx: i64,
    #[serde(default)]
    #[allow(dead_code)]
    thinking: Option<String>,
}

/// AI 在警长投票中选一名候选人。-1 = 弃权。
pub async fn sheriff_vote(
    llm: &LlmClient,
    view: &PublicView<'_>,
    candidates: &[(usize, String)],
    history: &AttemptHistory,
) -> Option<usize> {
    if candidates.is_empty() {
        return None;
    }
    let team = if view.me_role.is_wolf() {
        "你的阵营是狼。"
    } else {
        "你的阵营是好人。"
    };
    let system = format!(
        "你是飞书群里玩狼人杀的玩家，正在警长投票阶段。\
         你的真实身份是 **{}**。{}\n\
         {}\n\n{}\n\n## 任务\n\
         在候选人中投出你支持的警长，或弃权 (target_idx = -1)。候选人不能投票（包含你自己若是候选人）。\n\n\
         返回 JSON: {{\"target_idx\": <整数 idx 或 -1>, \"thinking\": \"...\"}}",
        view.me_role.label(),
        team,
        persona_line(view.persona),
        RULES,
    );
    let cands: Vec<String> = candidates
        .iter()
        .map(|(i, n)| format!("{} = {}", i, n))
        .collect();
    let user = format!(
        "{}\n\n## 警长候选人（仅这些 idx 合法）\n{}",
        view.render(),
        cands.join("\n"),
    );
    match chat_with_history(llm, system, user, history).await {
        Ok(c) => match serde_json::from_str::<SheriffVoteResp>(&c) {
            Ok(r) if r.target_idx >= 0 => {
                let idx = r.target_idx as usize;
                if candidates.iter().any(|(i, _)| *i == idx) {
                    Some(idx)
                } else {
                    None
                }
            }
            _ => None,
        },
        Err(e) => {
            warn!(?e, "sheriff_vote LLM call failed");
            None
        }
    }
}

#[derive(Debug, Deserialize)]
struct BadgeResp {
    target_idx: i64,
    #[serde(default)]
    #[allow(dead_code)]
    thinking: Option<String>,
}

/// AI 警长临死决定警徽去向。-1 = 撕毁，否则移交给 idx。
pub async fn badge_pass(
    llm: &LlmClient,
    view: &PublicView<'_>,
    history: &AttemptHistory,
) -> Option<usize> {
    let candidates: Vec<(usize, String)> = view
        .players
        .iter()
        .filter(|(i, _, alive)| *alive && *i != view.me_idx)
        .map(|(i, n, _)| (*i, n.clone()))
        .collect();
    if candidates.is_empty() {
        return None;
    }
    let team = if view.me_role.is_wolf() {
        "你的阵营是狼。"
    } else {
        "你的阵营是好人。"
    };
    let system = format!(
        "你是飞书群里的狼人杀玩家，刚刚作为警长死亡。\
         你的真实身份是 **{}**。{}\n\
         {}\n\n{}\n\n## 任务\n\
         决定警徽去向：移交给一名存活的非自己玩家（target_idx = idx），或撕毁 (target_idx = -1)。\n\n\
         返回 JSON: {{\"target_idx\": <整数 idx 或 -1>, \"thinking\": \"...\"}}",
        view.me_role.label(),
        team,
        persona_line(view.persona),
        RULES
    );
    let cands: Vec<String> = candidates
        .iter()
        .map(|(i, n)| format!("{} = {}", i, n))
        .collect();
    let user = format!(
        "{}\n\n## 候选接警人\n{}\n-1 = 撕毁警徽",
        view.render(),
        cands.join("\n"),
    );
    match chat_with_history(llm, system, user, history).await {
        Ok(c) => match serde_json::from_str::<BadgeResp>(&c) {
            Ok(r) if r.target_idx >= 0 => {
                let idx = r.target_idx as usize;
                if candidates.iter().any(|(i, _)| *i == idx) {
                    Some(idx)
                } else {
                    None
                }
            }
            _ => None,
        },
        Err(e) => {
            warn!(?e, "badge_pass LLM call failed");
            None
        }
    }
}

// ============================================================================
// 上警 / 白天 顺序发言
// ============================================================================

#[derive(Debug, Deserialize)]
struct SpeechResp {
    speech: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    thinking: Option<String>,
}

/// AI 上警竞选发言：候选人轮流发言，需要拉票 / 自报身份 / 攻击对手。
/// 失败时返回简短默认词，避免阻塞。
pub async fn sheriff_speech(llm: &LlmClient, view: &PublicView<'_>) -> String {
    let team = if view.me_role.is_wolf() {
        "你的阵营是狼。"
    } else {
        "你的阵营是好人。"
    };
    let system = format!(
        "你是飞书群里玩狼人杀的玩家，现在是第 1 天上警阶段，你正在做竞选发言。\
         你的真实身份是 **{}**。{}\n\
         {}\n\n{}\n\n## 任务\n\
         发表一段竞选发言，会公开广播给所有玩家。\n\
         风格：群聊语气、自然口语、≤ 80 字、一段话。\n\n\
         返回 JSON: {{\"speech\": \"<发言内容>\", \"thinking\": \"...\"}}",
        view.me_role.label(),
        team,
        persona_line(view.persona),
        RULES,
    );
    let user = view.render();
    match llm.chat_json(&system, &user).await {
        Ok(c) => match serde_json::from_str::<SpeechResp>(&c) {
            Ok(r) => r
                .speech
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| s.chars().take(200).collect::<String>())
                .unwrap_or_else(|| "我支持平和过渡，先听其他玩家。".into()),
            Err(e) => {
                warn!(?e, content = %c, "sheriff_speech parse failed");
                "我先表态。".into()
            }
        },
        Err(e) => {
            warn!(?e, "sheriff_speech LLM call failed");
            "我先表态。".into()
        }
    }
}

/// AI 警下发言（非候选人对警上候选人发表观点）。
pub async fn sheriff_side_speech(llm: &LlmClient, view: &PublicView<'_>) -> String {
    let team = if view.me_role.is_wolf() {
        "你的阵营是狼。"
    } else {
        "你的阵营是好人。"
    };
    let system = format!(
        "你是飞书群里玩狼人杀的玩家，正在警下发言阶段（非候选人对警上候选人表态）。\
         你的真实身份是 **{}**。{}\n\
         {}\n\n{}\n\n## 任务\n\
         对警上候选人发表你的看法，会公开广播。\n\
         风格：群聊语气、≤ 60 字、一段话；可空 (沉默)。\n\n\
         返回 JSON: {{\"speech\": \"<发言>\", \"thinking\": \"...\"}}",
        view.me_role.label(),
        team,
        persona_line(view.persona),
        RULES,
    );
    let user = view.render();
    match llm.chat_json(&system, &user).await {
        Ok(c) => match serde_json::from_str::<SpeechResp>(&c) {
            Ok(r) => r
                .speech
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| s.chars().take(150).collect::<String>())
                .unwrap_or_default(),
            Err(_) => String::new(),
        },
        Err(e) => {
            warn!(?e, "sheriff_side_speech LLM call failed");
            String::new()
        }
    }
}

/// AI 警长选择 警上 / 警下 起手。返回 true = 警上（顺时针）。
pub async fn sheriff_direction(llm: &LlmClient, view: &PublicView<'_>) -> bool {
    #[derive(Debug, Deserialize)]
    struct DirResp {
        clockwise: bool,
        #[serde(default)]
        #[allow(dead_code)]
        thinking: Option<String>,
    }
    let team = if view.me_role.is_wolf() {
        "你的阵营是狼。"
    } else {
        "你的阵营是好人。"
    };
    let system = format!(
        "你是新当选的警长，要决定白天发言起手方向。\
         你的真实身份是 **{}**。{}\n\
         {}\n\n{}\n\n## 任务\n\
         - clockwise=true（警上）：从你右手起手，顺时针\n\
         - clockwise=false（警下）：从你左手起手，逆时针\n\
         你本人将最后发言（归票）。\n\n\
         返回 JSON: {{\"clockwise\": <bool>, \"thinking\": \"...\"}}",
        view.me_role.label(),
        team,
        persona_line(view.persona),
        RULES
    );
    let user = view.render();
    match llm.chat_json(&system, &user).await {
        Ok(c) => match serde_json::from_str::<DirResp>(&c) {
            Ok(r) => r.clockwise,
            Err(_) => true, // 默认警上
        },
        Err(e) => {
            warn!(?e, "sheriff_direction LLM call failed");
            true
        }
    }
}

/// AI 死亡遗言。要求按角色 / 阵营立场最大化信息价值。
pub async fn last_words(llm: &LlmClient, view: &PublicView<'_>) -> String {
    let team = if view.me_role.is_wolf() {
        "你的阵营是狼。"
    } else {
        "你的阵营是好人。"
    };
    let system = format!(
        "你是飞书群里玩狼人杀的玩家，**你刚刚死亡，正在发表遗言**。\
         你的真实身份是 **{}**。{}\n\
         {}\n\n{}\n\n## 任务\n\
         发表你的遗言，会公开广播。\n\
         风格：群聊语气、≤ 100 字；可空（沉默）。\n\n\
         返回 JSON: {{\"speech\": \"<遗言>\", \"thinking\": \"...\"}}",
        view.me_role.label(),
        team,
        persona_line(view.persona),
        RULES,
    );
    let user = view.render();
    match llm.chat_json(&system, &user).await {
        Ok(c) => match serde_json::from_str::<SpeechResp>(&c) {
            Ok(r) => r
                .speech
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| s.chars().take(250).collect::<String>())
                .unwrap_or_default(),
            Err(_) => String::new(),
        },
        Err(e) => {
            warn!(?e, "last_words LLM call failed");
            String::new()
        }
    }
}

/// AI 白天轮流发言。带上当天死讯 / 历史发言上下文，要求按角色立场说话。
pub async fn day_speech(llm: &LlmClient, view: &PublicView<'_>) -> String {
    let team = if view.me_role.is_wolf() {
        "你的阵营是狼。"
    } else {
        "你的阵营是好人。"
    };
    let system = format!(
        "你是飞书群里玩狼人杀的玩家，现在是白天，轮到你发言（一次性单次发言，提交后回合结束）。\
         你的真实身份是 **{}**。{}\n\
         {}\n\n{}\n\n## 任务\n\
         发表你的看法，会公开广播。\n\
         风格：群聊语气、自然口语、≤ 80 字、一段话；可空 (沉默)。\n\n\
         返回 JSON: {{\"speech\": \"<发言内容>\", \"thinking\": \"...\"}}",
        view.me_role.label(),
        team,
        persona_line(view.persona),
        RULES,
    );
    let user = view.render();
    match llm.chat_json(&system, &user).await {
        Ok(c) => match serde_json::from_str::<SpeechResp>(&c) {
            Ok(r) => r
                .speech
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| s.chars().take(200).collect::<String>())
                .unwrap_or_default(),
            Err(e) => {
                warn!(?e, content = %c, "day_speech parse failed");
                String::new()
            }
        },
        Err(e) => {
            warn!(?e, "day_speech LLM call failed");
            String::new()
        }
    }
}


/// 把 (potentially) failed JSON parse into anyhow context for fmt'ing.
#[allow(dead_code)]
fn parse_or_err<T: for<'de> Deserialize<'de>>(s: &str) -> Result<T> {
    serde_json::from_str(s).with_context(|| format!("LLM JSON: {s}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persona_line_renders() {
        let line = persona_line(Some(Persona::Maniac));
        assert!(line.contains("头铁") || line.contains("Maniac") || line.contains("梭哈"));
    }

    #[test]
    fn render_view_shows_role() {
        let mut g = WolfGame::new("c".into());
        for i in 0..9 {
            g.add_player(format!("p{i}"), format!("P{i}")).unwrap();
        }
        g.start_game().unwrap();
        let view = build_view(&g, 0);
        let s = view.render();
        assert!(s.contains("P0"));
        assert!(s.contains("第 1 天"));
    }
}
