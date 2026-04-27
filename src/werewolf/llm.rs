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
// 通用：人设段
// ============================================================================

fn persona_line(persona: Option<Persona>) -> String {
    match persona {
        Some(p) => format!(
            "你的性格：**{}** —— {}\n按你性格的方式说话和决策，不要太教科书。",
            p.label(),
            p.description()
        ),
        None => "性格随性，按一般直觉决策。".into(),
    }
}

const RULES: &str = r#"## 规则提示
- 屠城规则：好人胜需击杀全部狼人；狼人胜利条件是存活狼数 ≥ 存活好人数
- 角色：狼人 / 预言家 / 女巫 / 猎人 / 村民
- 预言家每晚验一人；女巫一局共一瓶救药一瓶毒药；猎人被狼杀或被放逐时可开枪带走一人，被毒不能开枪
- 你必须扮演自己的角色：村民假装好人发言，狼伪装成村民，预言家挑时机跳出来
- **只返回 JSON，不要任何其他文字 / markdown / 代码块**"#;

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
pub async fn wolf_pick(
    llm: &LlmClient,
    view: &PublicView<'_>,
    game: &WolfGame,
    speak_enabled: bool,
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
        "\n\n## 狼频道发言\n\
         队伍里有人类狼，你可以在 \"chat\" 字段说一句协调队友的话（≤ 30 字、自然口语、可选）。\
         **大多数时候应该说点什么**——表态、建议刀谁、或者跟队友的发言。"
    } else {
        ""
    };
    let chat_field = if speak_enabled {
        ", \"chat\": \"<≤30 字发言, 不想说就 null>\""
    } else {
        ""
    };

    let system = format!(
        "你是飞书群里玩狼人杀的玩家，扮演一名狼人。{}\n\n{}\n\n## 狼人夜间任务\n\
         今晚你和狼队友需要协同选择一名非狼玩家击杀。\
         优先目标：神职（预言家 / 女巫 / 猎人 / 守卫）、显身份的强玩家；避免明显有利狼方的人。{}\n\n\
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

    match llm.chat_json(&system, &user).await {
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

pub async fn seer_pick(llm: &LlmClient, view: &PublicView<'_>) -> usize {
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
        "你是飞书群里玩狼人杀的玩家，扮演预言家。{}\n\n{}\n\n## 预言家夜间任务\n\
         每晚选一名玩家查验。优先验：白天发言奇怪、被怀疑的人，或下场后能给场上提供新信息的目标。\n\n\
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

    match llm.chat_json(&system, &user).await {
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

pub async fn witch_decide(llm: &LlmClient, view: &PublicView<'_>, game: &WolfGame) -> WitchDecision {
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
        "你是飞书群里玩狼人杀的玩家，扮演女巫。{}\n\n{}\n\n## 女巫夜间任务\n\
         你看得到今晚被狼刀的人。选择：\n\
         - save: 用救药救他（救药已用过则不可）\n\
         - poison: 用毒药毒一名存活玩家（毒药已用过则不可），需 poison_target_idx\n\
         - skip: 不做动作\n\
         **同晚不可救+毒**。第一夜尽量谨慎用药。\n\n\
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

    let raw = match llm.chat_json(&system, &user).await {
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
    quip: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    thinking: Option<String>,
}

#[derive(Debug, Clone)]
pub struct VoteDecision {
    pub target_idx: Option<usize>,
    pub quip: Option<String>,
}

pub async fn vote_pick(llm: &LlmClient, view: &PublicView<'_>) -> VoteDecision {
    let candidates: Vec<(usize, String)> = view
        .players
        .iter()
        .filter(|(i, _, alive)| *alive && *i != view.me_idx)
        .map(|(i, n, _)| (*i, n.clone()))
        .collect();

    let alignment = if view.me_role.is_wolf() {
        "你是狼人。投票时把怀疑导向真好人，保护狼队友，**绝对不能投自己的狼队友**。"
    } else {
        "你是好人。基于你能掌握的信息（事件历史、查验结果、其他人的发言），找出最可能是狼的人投出。"
    };

    let system = format!(
        "你是飞书群里玩狼人杀的玩家，正在白天投票阶段。{}\n\n{}\n\n## 投票指引\n{}\n\
         如果完全没头绪可以弃权（target_idx = -1）。\n\n\
         返回 JSON: {{\"target_idx\": <整数 idx 或 -1>, \"quip\": \"<≤30 字的发言, 可选>\", \"thinking\": \"...\"}}\n\
         quip 是你公开发言的内容，会广播到群里——好人可以分析、爆身份、领跳；狼人要伪装、带节奏、嫁祸。",
        persona_line(view.persona),
        RULES,
        alignment
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

    let raw = match llm.chat_json(&system, &user).await {
        Ok(c) => c,
        Err(e) => {
            warn!(?e, "vote LLM call failed, abstain");
            return VoteDecision { target_idx: None, quip: None };
        }
    };
    let parsed: VoteResp = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            warn!(?e, content = %raw, "vote JSON parse failed");
            return VoteDecision { target_idx: None, quip: None };
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
    let quip = parsed
        .quip
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.chars().take(60).collect::<String>());
    VoteDecision { target_idx: target, quip }
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

pub async fn hunter_pick(llm: &LlmClient, view: &PublicView<'_>) -> Option<usize> {
    let candidates: Vec<(usize, String)> = view
        .players
        .iter()
        .filter(|(i, _, alive)| *alive && *i != view.me_idx)
        .map(|(i, n, _)| (*i, n.clone()))
        .collect();
    if candidates.is_empty() {
        return None;
    }
    let alignment = match view.me_role {
        Role::WolfKing => "你是狼王，刚被处决 / 反向送葬。开枪带走最威胁狼方的好人（神职 / 警长优先）。",
        Role::Hunter => "你是猎人，刚被处决 / 杀害。开枪带走你最怀疑的狼人。",
        _ if view.me_role.is_wolf() => "你是狼，但获得了开枪技能。开枪带走最威胁狼方的好人。",
        _ => "你刚刚临死。开枪带走你最怀疑的狼人。",
    };
    let system = format!(
        "你是飞书群里的狼人杀玩家，刚刚临死，可以选一名玩家陪葬。{}\n\n{}\n\n{}\n\
         不开枪也是合法选项（target_idx = -1）。\n\n\
         返回 JSON: {{\"target_idx\": <整数 idx 或 -1>, \"thinking\": \"...\"}}",
        persona_line(view.persona),
        RULES,
        alignment
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

    let raw = match llm.chat_json(&system, &user).await {
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

pub async fn guard_pick(llm: &LlmClient, view: &PublicView<'_>, game: &WolfGame) -> usize {
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
        "你是飞书群里玩狼人杀的玩家，扮演守卫。{}\n\n{}\n\n## 守卫夜间任务\n\
         每晚选一名存活玩家守护（含自己），不能连续两晚守同一个人。\
         同守同救（被救+被守者死亡）。优先守可能被狼刀的神职 / 关键玩家。\n\n\
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
    match llm.chat_json(&system, &user).await {
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
    let alignment = match view.me_role {
        Role::Werewolf | Role::WolfKing => {
            "你是狼阵营，可以选择上警去抢警长（伪装成预言家）或不上保持低调。\
             团队最好至少有一只狼上警搅局。"
        }
        Role::Seer => "你是预言家，建议上警领跳并报告查验结果。",
        Role::Witch | Role::Hunter | Role::Guard => {
            "你是神职，看局势——上去抢警可以保护好人阵营但会暴露身份。"
        }
        Role::Villager => "你是村民，没必要上警，除非你想压狼。",
    };
    let system = format!(
        "你是飞书群里玩狼人杀的玩家。{}\n\n{}\n\n## 上警决策\n{}\n\n\
         返回 JSON: {{\"run\": <true/false>, \"thinking\": \"...\"}}",
        persona_line(view.persona),
        RULES,
        alignment
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
) -> Option<usize> {
    if candidates.is_empty() {
        return None;
    }
    let alignment = if view.me_role.is_wolf() {
        "你是狼，给最可能是狼队友的候选人投票，或者干扰好人阵营。"
    } else {
        "你是好人，挑你认为最像神职 / 最可信的候选人。"
    };
    let system = format!(
        "你是飞书群里玩狼人杀的玩家，正在警长投票阶段。{}\n\n{}\n\n{}\n\
         如果实在没头绪可以弃权（target_idx = -1）。\n\n\
         返回 JSON: {{\"target_idx\": <整数 idx 或 -1>, \"thinking\": \"...\"}}",
        persona_line(view.persona),
        RULES,
        alignment,
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
    match llm.chat_json(&system, &user).await {
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
pub async fn badge_pass(llm: &LlmClient, view: &PublicView<'_>) -> Option<usize> {
    let candidates: Vec<(usize, String)> = view
        .players
        .iter()
        .filter(|(i, _, alive)| *alive && *i != view.me_idx)
        .map(|(i, n, _)| (*i, n.clone()))
        .collect();
    if candidates.is_empty() {
        return None;
    }
    let alignment = if view.me_role.is_wolf() {
        "你是狼且是警长，想想把警徽给谁能继续搅乱场面，或者撕毁让好人没警权。"
    } else {
        "你是好人警长，把警徽传给最可信的玩家，或在所有人都可疑时撕毁。"
    };
    let system = format!(
        "你是飞书群里的狼人杀玩家，刚刚作为警长死亡。{}\n\n{}\n\n{}\n\
         撕毁警徽 = target_idx = -1。\n\n\
         返回 JSON: {{\"target_idx\": <整数 idx 或 -1>, \"thinking\": \"...\"}}",
        persona_line(view.persona),
        RULES,
        alignment
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
    match llm.chat_json(&system, &user).await {
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
    let alignment = match view.me_role {
        Role::Werewolf | Role::WolfKing => {
            "你是狼上警抢警。**绝对不能暴露身份**。可以伪装成预言家悍跳——\
             编造一个查验结果；或装好人喊跟某人。"
        }
        Role::Seer => {
            "你是预言家上警。可以选择真预言家——直接报昨晚查验结果；或者藏身份不爆。"
        }
        _ => "你是好人神 / 民上警。说说你为什么能当好警长，给场上分析方向。",
    };
    let system = format!(
        "你是飞书群里玩狼人杀的玩家，现在是上警阶段，你正在做竞选发言。\
         {}\n\n{}\n\n## 发言指引\n{}\n\
         发言风格：群聊语气、自然口语、≤ 80 字、一段话。\n\n\
         返回 JSON: {{\"speech\": \"<发言内容>\", \"thinking\": \"...\"}}",
        persona_line(view.persona),
        RULES,
        alignment,
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
    let alignment = if view.me_role.is_wolf() {
        "你是狼，没上警。可以选择支持狼队友（如果上警的有狼）/ 攻击好人候选 / 中立。"
    } else {
        "你是好人，没上警。基于警上发言判断哪个候选人更可信。"
    };
    let system = format!(
        "你是飞书群里玩狼人杀的玩家，正在警下发言阶段（非候选人对警上候选人发表看法）。\
         {}\n\n{}\n\n## 发言指引\n{}\n\
         发言风格：群聊语气、≤ 60 字、一段话；可空（沉默）。\n\n\
         返回 JSON: {{\"speech\": \"<发言>\", \"thinking\": \"...\"}}",
        persona_line(view.persona),
        RULES,
        alignment,
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
    let system = format!(
        "你是新当选的警长，要决定白天发言方向。{}\n\n{}\n\n## 决策\n\
         - clockwise=true（警上）：从你右手起手，顺时针；\n\
         - clockwise=false（警下）：从你左手起手，逆时针。\n\
         一般思路：让你信任的玩家最先发言（信号清楚）；或让重点玩家最后发言（先听情报）。\n\n\
         返回 JSON: {{\"clockwise\": <bool>, \"thinking\": \"...\"}}",
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
    let alignment = match view.me_role {
        Role::WolfKing => {
            "你是狼王，刚死，即将开枪。遗言可以倒钩 / 暴狼策略；说出你想带走谁的理由也行。"
        }
        Role::Werewolf => {
            "你是狼，刚死。遗言可以倒钩（继续装好人引开怀疑）/ 或干脆暴狼帮场上信息（少见）。"
        }
        Role::Seer => {
            "你是预言家，刚死。**强烈建议爆身份 + 报所有查验结果**——这是你能给好人留下的最后情报。"
        }
        Role::Witch => {
            "你是女巫。可以爆身份说用药情况（救了谁、毒了谁），帮好人理清线索。"
        }
        Role::Hunter => {
            "你是猎人，即将开枪。遗言里可以说你想带走谁的理由，给场上信息。"
        }
        Role::Guard => {
            "你是守卫。可以爆身份说前夜守了谁，给场上还原信息。"
        }
        Role::Villager => {
            "你是村民。说说你怀疑谁、信任谁，给好人阵营留点判断方向。"
        }
    };
    let system = format!(
        "你是飞书群里玩狼人杀的玩家，**你刚刚死亡，正在发表遗言**。{}\n\n{}\n\n## 遗言指引\n{}\n\
         风格：群聊语气、≤ 100 字；可空（沉默）但不建议——遗言是你最后机会。\n\n\
         返回 JSON: {{\"speech\": \"<遗言>\", \"thinking\": \"...\"}}",
        persona_line(view.persona),
        RULES,
        alignment,
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
    let alignment = match view.me_role {
        Role::Werewolf | Role::WolfKing => {
            "你是狼。**伪装成好人**：分析、表态、推狼，但要保护狼队友、把怀疑导向真好人。\
             如果有狼队友被前面发言的人怀疑，可以替他洗或转移焦点。"
        }
        Role::Seer => {
            "你是预言家。可以跳预报查验（公开身份+目标）；或者藏身份观察。\
             跳预可以给场上提供决定性信息，但容易被狼第二夜刀。"
        }
        _ => "你是好人。基于事件历史 / 之前发言找出最可疑的人，表达投票倾向。",
    };
    let system = format!(
        "你是飞书群里玩狼人杀的玩家，现在是白天，轮到你发言（一次性单次发言，提交后就结束）。\
         {}\n\n{}\n\n## 发言指引\n{}\n\
         发言风格：群聊语气、自然口语、≤ 80 字、一段话；可以空（沉默）但不建议。\n\n\
         返回 JSON: {{\"speech\": \"<发言内容>\", \"thinking\": \"...\"}}",
        persona_line(view.persona),
        RULES,
        alignment,
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
