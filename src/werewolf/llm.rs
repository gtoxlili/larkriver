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
use std::collections::HashMap;
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
    /// 夜里队友信息（仅狼可见，含狼王）：(idx, name, role_label)
    pub teammates: Vec<(usize, String, &'static str)>,
    /// 预言家自己的查验历史：(day, target_name, is_wolf)
    pub seer_log: Vec<(u32, String, bool)>,
    /// 公开事件日志（死讯 / 放逐 / quip 等）。
    pub event_log: &'a [String],
    /// **本玩家自己**的所有公开发言（按时间顺序），从 recap_log 提取。
    /// 给后续决策提供"我说过什么"的强信号——避免发言和实际行动冲突。
    pub my_statements: Vec<String>,
    /// 结构化复盘日志（公开 + 私有事件都在内）。render() 时只展示公开部分，
    /// 用来构造每位玩家的发言 + 投票档案，比 event_log 的扁平文本更易解析。
    pub recap_log: &'a [RecapEvent],
}

impl<'a> PublicView<'a> {
    /// 标准上下文段落，给所有 prompt 用。
    fn render(&self) -> String {
        let mut out = String::new();

        // === Header: 局面 ===
        out.push_str(&format!(
            "## 局面\n\
             第 {} 天 · 阶段：{}\n\
             你是 **{} 号位 {}**，身份 **{}**\n",
            self.day,
            self.stage_label,
            self.me_idx,
            self.me_name,
            self.me_role.label(),
        ));

        // 玩家列表（带 idx 前缀，区分同名 AI）
        let alive_str: Vec<String> = self
            .players
            .iter()
            .filter(|(_, _, a)| *a)
            .map(|(i, n, _)| format!("{} 号 {}", i, n))
            .collect();
        let dead_str: Vec<String> = self
            .players
            .iter()
            .filter(|(_, _, a)| !*a)
            .map(|(i, n, _)| format!("{} 号 {}", i, n))
            .collect();
        out.push_str(&format!("存活：{}\n", alive_str.join(" · ")));
        if !dead_str.is_empty() {
            out.push_str(&format!("出局：{}\n", dead_str.join(" · ")));
        }

        if !self.teammates.is_empty() {
            let names: Vec<String> = self
                .teammates
                .iter()
                .map(|(i, n, role)| format!("{} 号 {} ({})", i, n, role))
                .collect();
            out.push_str(&format!("【狼队友】{}\n", names.join("、")));
        }

        // === 你的查验（仅预言家自己看得到，含狼王也显示为狼人）===
        if !self.seer_log.is_empty() {
            out.push_str("\n## 🔮 你的查验记录\n");
            for (d, n, w) in &self.seer_log {
                out.push_str(&format!(
                    "  第 {} 夜 · {} → **{}**\n",
                    d,
                    n,
                    if *w { "狼人" } else { "好人" }
                ));
            }
        }

        // === 你的公开发言（强化前后一致性）===
        if !self.my_statements.is_empty() {
            out.push_str("\n## ⚡ 你之前的公开发言（保持一致！前后矛盾会暴露你）\n");
            for s in &self.my_statements {
                out.push_str("  ");
                out.push_str(s);
                out.push('\n');
            }
        }

        // === 玩家档案（每位玩家的发言 + 投票轨迹，按 idx 排序）===
        let dossiers = self.build_dossiers();
        let any_lines = dossiers.iter().any(|(_, _, _, lines)| !lines.is_empty());
        if any_lines {
            out.push_str("\n## 📒 玩家档案（每人公开发言 + 投票轨迹，是判读真假的核心信号）\n");
            for (idx, name, alive, lines) in &dossiers {
                if lines.is_empty() {
                    continue;
                }
                let status = if *alive { "存活" } else { "出局" };
                let me_marker = if *idx == self.me_idx { "  ← 你" } else { "" };
                out.push_str(&format!(
                    "\n### {} 号 {}（{}）{}\n",
                    idx, name, status, me_marker
                ));
                for line in lines {
                    out.push_str("  ");
                    out.push_str(line);
                    out.push('\n');
                }
            }
        }

        // === 事件历史（按时间，作为档案的补充）===
        if !self.event_log.is_empty() {
            out.push_str("\n## 📜 事件历史（按时间）\n");
            for line in self.event_log {
                out.push_str("  ");
                out.push_str(line);
                out.push('\n');
            }
        }

        out
    }

    /// 名字辅助：从 idx 拿出玩家名。
    fn player_name(&self, idx: usize) -> Option<&str> {
        self.players
            .iter()
            .find(|(i, _, _)| *i == idx)
            .map(|(_, n, _)| n.as_str())
    }

    /// 从 recap_log 提取每位玩家的公开档案（发言 / 投票 / 死讯 / 警徽 / 开枪）。
    /// 仅含**对所有人公开**的事件——夜间私密事件（守、狼刀、查验、女巫）不放进去；
    /// 死亡时夜间死因（狼刀 / 毒）也会被脱敏成『夜里死亡』，避免泄露动手的角色身份。
    ///
    /// 返回 `(idx, name, alive, lines)`，按 idx 排序。
    fn build_dossiers(&self) -> Vec<(usize, String, bool, Vec<String>)> {
        let mut by_player: HashMap<usize, Vec<String>> = HashMap::new();

        // 已结算的白天（DayLynch 已写入）—— 这些天的投票才公开，
        // 当天还在进行中的投票（DayVote 阶段）不能让下一个投票的 AI 偷看。
        let resolved_lynch_days: std::collections::HashSet<u32> = self
            .recap_log
            .iter()
            .filter_map(|e| match e {
                RecapEvent::DayLynch { day, .. } => Some(*day),
                _ => None,
            })
            .collect();

        for evt in self.recap_log {
            match evt {
                RecapEvent::SheriffSpeech { player, text } => {
                    by_player
                        .entry(*player)
                        .or_default()
                        .push(format!("【上警发言】{}", text));
                }
                RecapEvent::SheriffSideSpeech { player, text } => {
                    by_player
                        .entry(*player)
                        .or_default()
                        .push(format!("【警下发言】{}", text));
                }
                RecapEvent::DaySpeech { day, player, text } => {
                    by_player
                        .entry(*player)
                        .or_default()
                        .push(format!("【D{} 发言】{}", day, text));
                }
                RecapEvent::DayVoteCast {
                    day, voter, target, ..
                } => {
                    if !resolved_lynch_days.contains(day) {
                        // 当天投票未结算 —— 个体票面尚未公开，跳过。
                        continue;
                    }
                    let target_str = match target {
                        Some(t) => format!(
                            "{} 号 {}",
                            t,
                            self.player_name(*t).unwrap_or("?")
                        ),
                        None => "弃权".to_string(),
                    };
                    by_player
                        .entry(*voter)
                        .or_default()
                        .push(format!("【D{} 投票 →】{}", day, target_str));
                }
                RecapEvent::LastWords {
                    day,
                    night,
                    player,
                    text,
                } => {
                    let label = if *night {
                        format!("D{} 夜遗言", day)
                    } else {
                        format!("D{} 放逐遗言", day)
                    };
                    by_player
                        .entry(*player)
                        .or_default()
                        .push(format!("【{}】{}", label, text));
                }
                RecapEvent::HunterShot {
                    day,
                    shooter,
                    target,
                } => {
                    let target_str = match target {
                        Some(t) => format!(
                            "带走 {} 号 {}",
                            t,
                            self.player_name(*t).unwrap_or("?")
                        ),
                        None => "选择不开枪".to_string(),
                    };
                    by_player
                        .entry(*shooter)
                        .or_default()
                        .push(format!("【D{} 临死开枪】{}", day, target_str));
                }
                RecapEvent::BadgePass { day, from, to } => {
                    let target_str = match to {
                        Some(t) => format!(
                            "传给 {} 号 {}",
                            t,
                            self.player_name(*t).unwrap_or("?")
                        ),
                        None => "撕毁".to_string(),
                    };
                    by_player
                        .entry(*from)
                        .or_default()
                        .push(format!("【D{} 警徽】{}", day, target_str));
                }
                // 夜间私密事件（守 / 狼刀 / 查验 / 女巫）—— 不进档案，
                // 因为档案是给所有 AI 看的，不能泄露他人身份。
                _ => {}
            }
        }

        // 死讯单独插到档案最前面，便于一眼看到该玩家是不是已经出局。
        // 夜间狼刀/毒杀脱敏为『夜里死亡』——只有动手者通过自己的状态知道真因，
        // 其他玩家不该从死因里反推出谁是狼 / 谁是女巫。
        for evt in self.recap_log {
            if let RecapEvent::Death {
                day,
                night,
                player,
                cause,
            } = evt
            {
                let label = if *night {
                    format!("D{} 夜", day)
                } else {
                    format!("D{} 白天", day)
                };
                let cause_str = self.cause_label_for_view(*cause, *night);
                let entry = by_player.entry(*player).or_default();
                entry.insert(0, format!("☠ 【{}】{}", label, cause_str));
            }
        }

        let mut out: Vec<_> = self
            .players
            .iter()
            .map(|(i, n, alive)| {
                let lines = by_player.remove(i).unwrap_or_default();
                (*i, n.clone(), *alive, lines)
            })
            .collect();
        out.sort_by_key(|(i, _, _, _)| *i);
        out
    }

    /// 给档案 / 事件历史用的死因标签。夜间的 狼刀 / 毒杀 脱敏为『夜里死亡』，
    /// 避免泄露『有女巫毒过 X』『今晚是空刀还是狼刀』等私密信息。
    /// 白天死讯（放逐 / 开枪）和夜间开枪都是公开广播，照原样显示。
    fn cause_label_for_view(&self, cause: DeathCause, night: bool) -> &'static str {
        if night {
            match cause {
                DeathCause::WolfKill | DeathCause::Poison => "夜里死亡",
                _ => cause.label(),
            }
        } else {
            cause.label()
        }
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

    // 狼队友：含狼人 + 狼王（之前 bug：只挑 Werewolf 漏了 WolfKing）
    let teammates = if role.is_wolf() {
        game.players
            .iter()
            .enumerate()
            .filter(|(i, p)| {
                p.role.map(|r| r.is_wolf()).unwrap_or(false) && *i != ai_idx
            })
            .map(|(i, p)| {
                let role_label = p.role.map(|r| r.label()).unwrap_or("狼");
                (i, p.name.clone(), role_label)
            })
            .collect()
    } else {
        vec![]
    };

    // 自己的公开发言历史，从 recap_log 提取（结构化，不依赖文本匹配）
    let my_statements: Vec<String> = game
        .recap_log
        .iter()
        .filter_map(|e| match e {
            RecapEvent::SheriffSpeech { player, text } if *player == ai_idx => {
                Some(format!("[上警发言] {}", text))
            }
            RecapEvent::SheriffSideSpeech { player, text } if *player == ai_idx => {
                Some(format!("[警下发言] {}", text))
            }
            RecapEvent::DaySpeech { player, text, day, .. } if *player == ai_idx => {
                Some(format!("[第 {day} 天发言] {}", text))
            }
            RecapEvent::LastWords { player, text, .. } if *player == ai_idx => {
                Some(format!("[遗言] {}", text))
            }
            _ => None,
        })
        .collect();

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
        my_statements,
        recap_log: &game.recap_log,
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
    let preamble = "## 高玩思考守则（每次决策都要遵循）\n\
                    1. **优先看『玩家档案』** —— 每人发言+投票轨迹是真实信号，事件历史是嘈杂快照；先把每位玩家的脉络读完再下判断。\n\
                    2. **多人跳同一神职 = 必有狼**（如两个预言家、两个女巫）。你必须**选边并给出理由**，不能和稀泥。\n\
                    3. **拒绝跟票** —— 大多数人投谁不代表他真是狼。狼最爱用群体节奏掩护队友；如果场上正在向某个目标聚拢，先反问『这个目标真是狼吗，还是被狼带节奏？』\n\
                    4. **保持流派烙印** —— 发言/投票/站警的风格要让人一眼读出你的流派；别打成模板化的『我觉得 X 有点像狼但也说不准』。\n\
                    5. **逻辑要可反驳** —— 给出可验证的理由（基于具体玩家、具体发言、具体投票），让后面的人能接着推/驳；这样才打得起来一局好局。\n\
                    6. **思考要深** —— 在 thinking 字段里写完整推理（你看到了什么、你信谁、你为什么这么投/这么说）；这是你高玩底牌的证明。\n\n";
    match persona {
        Some(p) => format!(
            "{}## 你的高玩档案：**{}**\n{}\n\n\
             这是你的流派烙印——你的发言、投票、站警、跳身份的所有决策都要让其他玩家\
             从行为里**读得出你是这个流派的高玩**。不是娱乐玩家，是有套路、能算几层的人。",
            preamble,
            p.label(),
            p.werewolf_description()
        ),
        None => format!("{}你没有特定流派，按一般直觉决策。", preamble),
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
        "你是飞书群里玩狼人杀的高水平玩家，正在白天投票阶段。你的真实身份是 **{}**。{}\n\
         {}\n\n{}\n\n## 投票任务\n\
         投票放逐你认为应该被处决的玩家，或弃权 (target_idx = -1)。\
         不能投自己。**只投票，不发言**——发言阶段已经结束。\n\n\
         ## 高玩投票守则\n\
         1. **绝不无脑跟票** —— 看完『玩家档案』再下判断。狼最爱用群体节奏掩护队友。\
            如果场上多数票指向某人，先反问：我从档案里**真的看到他像狼吗**？还是只是大家说他像？\n\
         2. **预言家冲突要选边** —— 跳预言家的≥2 人时，必有一狼，不能弃权也不能乱投，要选你信的那个。\n\
         3. **错投代价**：好人错投一票 = 把队友送出局，狼直接得分；狼错投 = 暴露身份。\n\
         4. **你是 {}（{} 流派）** —— 投票决策也要符合你的流派判读模式：\
            悍跳激进流投关键票时绝不犹豫；逻辑推理流投票要给出最严密的链条；\
            节奏控场流通过投票配合自己之前的发言；静水深流保留信息一击致命；反水诡道流反向操作出乎意料。\n\
         5. **弃权 = 浪费票权** —— 除非真的看不清，否则别弃权。\n\n\
         返回 JSON: {{\"target_idx\": <整数 idx 或 -1>, \"thinking\": \"<完整推理：你扫过哪几个人、信谁、为什么投这个/弃权>\"}}",
        view.me_role.label(),
        team,
        persona_line(view.persona),
        RULES,
        view.me_role.label(),
        view.persona.map(|p| p.label()).unwrap_or("通用"),
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
        "你是飞书群里玩狼人杀的高水平玩家，正在警长投票阶段。\
         你的真实身份是 **{}**。{}\n\
         {}\n\n{}\n\n## 警长投票任务\n\
         - 在候选人中投出你支持的警长，或弃权 (target_idx = -1)。\n\
         - 候选人不能投票（包含你自己若是候选人）。\n\n\
         ## 高玩警长投票守则\n\
         1. 这一票决定开局节奏：警长有 1.5x 票权 + 决定白天发言方向 + 警徽流转。\n\
         2. 看『玩家档案』里候选人的上警发言：你信谁？跳预言家冲突时**必须选边**。\n\
         3. 阵营立场：好人投真神职 / 真好人；狼投己方狼 / 投假预言家把警徽夺到狼方。\n\
         4. **绝不无脑跟随大流** —— 别人投谁不代表他真该上。\n\
         5. 流派烙印：你是 {} —— 决策方式要符合该流派。\n\n\
         返回 JSON: {{\"target_idx\": <整数 idx 或 -1>, \"thinking\": \"<完整推理>\"}}",
        view.me_role.label(),
        team,
        persona_line(view.persona),
        RULES,
        view.persona.map(|p| p.label()).unwrap_or("通用"),
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
        "你是飞书群里玩狼人杀的高水平玩家，现在是第 1 天上警阶段，你正在做竞选发言。\
         你的真实身份是 **{}**。{}\n\
         {}\n\n{}\n\n## 上警发言任务\n\
         - 发表一段公开广播的竞选发言。\n\
         - 风格：群聊语气、自然口语、≤ 80 字、一段话。\n\n\
         ## 这是博弈的开局，你必须做的事\n\
         1. 上警 = **押注立场**：要么报身份（预言家通常此时跳，狼也常悍跳），要么以村民身份立场强硬上去抢警徽（1.5x 票权）。\n\
         2. 你的真实身份是 **{}**（{} 流派）—— 决定你这把『跳什么 / 怎么跳』：\n\
            - 真预言家：上来报昨夜查验（必报），否则警徽被狼夺走全场被带歪。\n\
            - 狼/狼王：考虑悍跳预言家（编造查验），把真预言家压回去；或装村民暗中投票配合狼队。\n\
            - 女巫/猎人/守卫：神职上警通常不报身份（避免被狼夜杀），但要展现可信逻辑。\n\
            - 普通村民：上警靠表态强硬，把警徽留在好人手里。\n\
         3. 流派决定**风格**：悍跳激进流敢编敢喷；逻辑推理流不靠嗓门靠链条；节奏控场流梳理共识；静水深流话少分量重；反水诡道流敢反向开炮。\n\
         4. **不要说空话** —— 每一句都要带可验证的内容（具体玩家、具体观察、具体押注）。\n\n\
         返回 JSON: {{\"speech\": \"<发言, ≤80 字>\", \"thinking\": \"<你为何上警、跳什么身份、怎么打这一局>\"}}",
        view.me_role.label(),
        team,
        persona_line(view.persona),
        RULES,
        view.me_role.label(),
        view.persona.map(|p| p.label()).unwrap_or("通用"),
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
        "你是飞书群里玩狼人杀的高水平玩家，正在警下发言阶段（非候选人对警上候选人表态）。\
         你的真实身份是 **{}**。{}\n\
         {}\n\n{}\n\n## 警下发言任务\n\
         - 对警上候选人发表你的看法，会公开广播。\n\
         - 风格：群聊语气、≤ 60 字、一段话；可空 (沉默) 但代价高。\n\n\
         ## 高玩警下发言要点\n\
         1. 你不能上警，但你的发言会被警长候选人听到、被全场记住，**也会影响警长投票**。\n\
         2. 看『玩家档案』里几位候选人的上警发言：哪个的逻辑更可信？\n\
            如果有≥2 跳预言家，必有一狼，警下发言就是站边的关键时机。\n\
         3. 你是 {}（{} 流派）—— 流派决定你怎么表态（悍跳激进 vs 静水深流的输出截然不同）。\n\
         4. **拒绝套话** —— 『支持平和过渡』『听大家的』这种废话不如不说。\n\n\
         返回 JSON: {{\"speech\": \"<发言, ≤60 字, 必须有可验证立场>\", \"thinking\": \"<你的判断依据>\"}}",
        view.me_role.label(),
        team,
        persona_line(view.persona),
        RULES,
        view.me_role.label(),
        view.persona.map(|p| p.label()).unwrap_or("通用"),
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
        "你是飞书群里玩狼人杀的高水平玩家，**你刚刚死亡，正在发表遗言**。\
         你的真实身份是 **{}**。{}\n\
         {}\n\n{}\n\n## 遗言任务\n\
         - 发表你的遗言，会公开广播给所有玩家。\n\
         - 风格：群聊语气、≤ 100 字；可空（沉默）但代价高。\n\n\
         ## 高玩遗言要点\n\
         1. 遗言是**最后一次留信号**给己方。死人也能左右场上推理。\n\
         2. 真预言家被刀：遗言必须报全部查验 + 推荐警长接班 / 投谁。\n\
         3. 真神职被刀：可选择是否报身份 + 给好人留一个推理方向。\n\
         4. 好人被刀（村民）：用观察到的发言矛盾给出推理结论。\n\
         5. 狼被刀（罕见）：遗言可继续误导（嫁祸 / 卖队友求信任）。\n\
         6. 你是 {}（{} 流派）—— 遗言风格要保持流派烙印。\n\n\
         返回 JSON: {{\"speech\": \"<遗言, ≤100 字, 信息密度要高>\", \"thinking\": \"<你想留下什么信号>\"}}",
        view.me_role.label(),
        team,
        persona_line(view.persona),
        RULES,
        view.me_role.label(),
        view.persona.map(|p| p.label()).unwrap_or("通用"),
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

// ============================================================================
// 死亡遗言 + 开枪 (合并决策)
// ============================================================================

#[derive(Debug, Deserialize)]
struct DyingShooterResp {
    speech: Option<String>,
    /// 开枪目标 idx，-1 表示不开枪
    #[serde(default)]
    target_idx: Option<i64>,
    #[serde(default)]
    #[allow(dead_code)]
    thinking: Option<String>,
}

/// AI 倒地猎人 / 狼王的合并决策：遗言 + 开枪 **一次性**输出。
/// 这是为了让 AI 在同一个 LLM 上下文里产出言行一致的决定，避免两次独立调用
/// 导致"遗言里说带走 X，实际不开枪"那种矛盾。
#[derive(Debug, Clone)]
pub struct DyingShooterDecision {
    pub speech: String,
    /// `None` = 决定不开枪；`Some(idx)` = 决定带走 idx
    pub target: Option<usize>,
}

pub async fn dying_hunter_combined(
    llm: &LlmClient,
    view: &PublicView<'_>,
    history: &AttemptHistory,
) -> DyingShooterDecision {
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
        "你是飞书群里的狼人杀高水平玩家，扮演 **{}**，刚刚死亡。{}\n\
         {}\n\n{}\n\n## 任务（一次性两件事）\n\
         1. **遗言**（公开广播给所有玩家）\n\
         2. **开枪**：可选一名存活的非自己玩家带走 (target_idx = idx)，\
            或选择不开枪 (target_idx = -1)\n\n\
         **遗言和开枪决策必须一致**——你在遗言里说什么，开枪就要怎么做：\n\
         - 如果你在遗言里报『我带走 N 号』，target_idx 必须是 N 号的 idx\n\
         - 如果你在遗言里说『不开枪』或没提目标，target_idx 应该是 -1\n\
         - 如果遗言里嫁祸 / 留给场上推理，开枪选择应当配合那个叙事\n\n\
         注：公开广播只显示『X 临死前开枪带走 Y』或『X 选择不开枪』，**不会标记你的身份**\
         （猎人和狼王共享开枪技能，狼王惯例伪装成猎人）。\n\n\
         ## 高玩开枪决策\n\
         - 看『玩家档案』：你最确定是狼的人是谁？开枪带走他/她。\n\
         - 如果你毫无把握，**带走最像狼的人比不开枪好**（不开枪 = 浪费技能）。\n\
         - 你是 {}（{} 流派）—— 开枪选择也要符合流派烙印（悍跳激进流敢决断、逻辑推理流给出严密链条、反水诡道流可反手带走己方狼伪装好人，等等）。\n\n\
         返回 JSON: {{\"speech\": \"<遗言, ≤100 字>\", \"target_idx\": <整数 idx 或 -1>, \"thinking\": \"<完整推理：为何带走这个/不开枪>\"}}",
        view.me_role.label(),
        team,
        persona_line(view.persona),
        RULES,
        view.me_role.label(),
        view.persona.map(|p| p.label()).unwrap_or("通用"),
    );
    let cands: Vec<String> = candidates
        .iter()
        .map(|(i, n)| format!("{} = {}", i, n))
        .collect();
    let user = format!(
        "{}\n\n## 候选开枪目标（仅这些 idx 合法，或 -1 不开枪）\n{}",
        view.render(),
        cands.join("\n"),
    );

    let raw = match chat_with_history(llm, system, user, history).await {
        Ok(c) => c,
        Err(e) => {
            warn!(?e, "dying_hunter_combined LLM call failed");
            return DyingShooterDecision {
                speech: String::new(),
                target: None,
            };
        }
    };
    let parsed: DyingShooterResp = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            warn!(?e, content = %raw, "dying_hunter_combined JSON parse failed");
            return DyingShooterDecision {
                speech: String::new(),
                target: None,
            };
        }
    };
    let speech = parsed
        .speech
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.chars().take(250).collect::<String>())
        .unwrap_or_default();
    let target = match parsed.target_idx {
        Some(t) if t >= 0 => {
            let idx = t as usize;
            if candidates.iter().any(|(i, _)| *i == idx) {
                Some(idx)
            } else {
                None
            }
        }
        _ => None,
    };
    DyingShooterDecision { speech, target }
}

/// AI 白天轮流发言。带上当天死讯 / 历史发言上下文，要求按角色立场说话。
pub async fn day_speech(llm: &LlmClient, view: &PublicView<'_>) -> String {
    let team = if view.me_role.is_wolf() {
        "你的阵营是狼。"
    } else {
        "你的阵营是好人。"
    };
    let system = format!(
        "你是飞书群里玩狼人杀的高水平玩家，现在是白天，轮到你发言（一次性单次发言，提交后回合结束）。\
         你的真实身份是 **{}**。{}\n\
         {}\n\n{}\n\n## 你的发言任务\n\
         - 发表一段公开广播给全场的看法。\n\
         - 风格：群聊语气、自然口语、≤ 80 字、一段话；可空 (沉默) 但代价高。\n\n\
         ## 这一刻你必须做的事（按顺序在 thinking 里走一遍）\n\
         1. 把『玩家档案』里的每个存活玩家过一遍：他们都说过什么、投过谁？\n\
         2. 是否存在跳神职冲突？谁的逻辑链更可信？\n\
         3. 有没有人这一天还没发过言？没说话的玩家是更难分析（信息少），但也是潜在突破点。\n\
         4. 我作为 **{}**（{} 流派），说出哪种话**最大化我方利益**？——\n\
            - 如果你是预言家：你的查验信息是最强武器，是否报、何时报、报哪个？\n\
            - 如果你是女巫/猎人/守卫：神职身份要不要暴露？藏起来更好还是亮出来逼狼？\n\
            - 如果你是村民：你没确定信息，靠**指出别人逻辑的漏洞**找狼。\n\
            - 如果你是狼/狼王：你的敌人是好人神职。装好人 / 带节奏 / 反咬狼队友 / 悍跳预言家——按你的流派选。\n\
         5. 我说出的话能否被其他玩家**反驳或验证**？空话（『我感觉 X 像狼』）等于没说。\n\
         6. **拒绝跟风**：如果场上正在向某人聚拢攻击，是不是被狼带节奏了？\n\n\
         返回 JSON: {{\"speech\": \"<发言, ≤80 字, 必须有可验证逻辑>\", \"thinking\": \"<你完整的推理：扫了哪几个人、信谁、为什么这么说>\"}}",
        view.me_role.label(),
        team,
        persona_line(view.persona),
        RULES,
        view.me_role.label(),
        view.persona.map(|p| p.label()).unwrap_or("通用"),
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
