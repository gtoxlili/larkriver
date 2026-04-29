//! 狼人杀 AI 决策。
//!
//! 每个角色有自己的 prompt 和决策结构：
//! - 狼：每晚选一名存活玩家击杀(默认非狼,自刀作为策略性选项),可发表夜间内部讨论(不公开)
//! - 预言家：每晚选一名玩家查验
//! - 女巫：知道今晚被刀的人，选 救 / 毒 / 跳过
//! - 投票：白天选一名玩家投票放逐
//! - 猎人：临死时选一名玩家开枪
//! - 讨论 quip：白天 AI 各自发表观点
//!
//! 所有决策走同一个 `LlmClient::chat_json` 入口；构造 prompt 和解析输出在这里完成。

use crate::game::Persona;
use crate::llm::LlmClient;
use crate::util::{FastHashMap, FoldHashSet};
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
    /// **本玩家自己**的过往 thinking 历史（从 thinking_log 中过滤）。
    /// 让 AI 在每次决策前能看到自己上几轮的内心独白，保持策略弧线的连贯性。
    /// **严格私密**——只含本玩家的条目，绝不包含其他玩家的 thinking。
    pub my_thinking_history: Vec<&'a ThinkingEntry>,
}

impl<'a> PublicView<'a> {
    /// 标准上下文段落，给所有 prompt 用。
    fn render(&self) -> String {
        let mut out = String::new();

        // === Header: 局面 ===
        out.push_str(&format!(
            "## 局面\n\
             第 {} 天 · 阶段：{}\n\
             你是 **{}**，身份 **{}**\n",
            self.day,
            self.stage_label,
            self.me_name,
            self.me_role.label(),
        ));

        // 玩家列表——AI 名字里已经带 #N 全局唯一，直接列名字即可
        let alive_str: Vec<String> = self
            .players
            .iter()
            .filter(|(_, _, a)| *a)
            .map(|(_, n, _)| n.clone())
            .collect();
        let dead_str: Vec<String> = self
            .players
            .iter()
            .filter(|(_, _, a)| !*a)
            .map(|(_, n, _)| n.clone())
            .collect();
        out.push_str(&format!("存活：{}\n", alive_str.join(" · ")));
        if !dead_str.is_empty() {
            out.push_str(&format!("出局：{}\n", dead_str.join(" · ")));
        }

        if !self.teammates.is_empty() {
            let names: Vec<String> = self
                .teammates
                .iter()
                .map(|(_, n, role)| format!("{} ({})", n, role))
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

        // === 你的内心独白历史（私密：你过去每次决策的 thinking，给本次决策做"自我连贯性"参考）===
        // 这是你跨轮的策略弧线——你上把为什么这么投/这么说/这么编，本把要不要延续这个剧本。
        // 高玩不会每把都从头想；上一把"我装女巫诈狼"的设定本把不能突然抛弃。
        if !self.my_thinking_history.is_empty() {
            out.push_str("\n## 🧠 你的内心独白历史（你过去几轮的 thinking，外人看不到）\n");
            for entry in &self.my_thinking_history {
                out.push_str(&format!(
                    "\n### D{} · {}\n{}\n",
                    entry.day,
                    entry.kind.label(),
                    entry.thinking
                ));
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
                    "\n### {}（{}）{}\n",
                    name, status, me_marker
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
        let mut by_player: FastHashMap<usize, Vec<String>> = FastHashMap::default();

        // 已结算的白天（DayLynch 已写入）—— 这些天的投票才公开，
        // 当天还在进行中的投票（DayVote 阶段）不能让下一个投票的 AI 偷看。
        let resolved_lynch_days: FoldHashSet<u32> = self
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
                        Some(t) => self.player_name(*t).unwrap_or("?").to_string(),
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
                        Some(t) => format!("带走 {}", self.player_name(*t).unwrap_or("?")),
                        None => "选择不开枪".to_string(),
                    };
                    by_player
                        .entry(*shooter)
                        .or_default()
                        .push(format!("【D{} 临死开枪】{}", day, target_str));
                }
                RecapEvent::BadgePass { day, from, to } => {
                    let target_str = match to {
                        Some(t) => format!("传给 {}", self.player_name(*t).unwrap_or("?")),
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

    // 私密 thinking 历史：仅本玩家自己的条目，按发生顺序。绝不暴露给其他人。
    let my_thinking_history: Vec<&ThinkingEntry> = game
        .thinking_log
        .iter()
        .filter(|e| e.player == ai_idx)
        .collect();

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
        my_thinking_history,
    }
}

// ============================================================================
// 通用：retry-with-feedback 消息构造
// ============================================================================

/// 历史 = 已经被拒绝的尝试列表：(上次的答案 JSON 文本, 拒绝原因)。
/// caller 在 retry 时把这个 history 传进来，函数内部把它转成多轮对话——
/// LLM 能看见自己刚才的失败并据此调整选择，而不是被无脑重试。
pub type AttemptHistory = Vec<(String, String)>;

/// 把 LLM 返回的 `thinking` 字段标准化：去空白；空 → None。
fn norm_thinking(t: Option<String>) -> Option<String> {
    t.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

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
    let preamble = "## 决策时手头的资源\n\
                    - **玩家档案**：每位玩家的公开发言 + 投票轨迹，按 idx 聚合\n\
                    - **事件历史**：按时间的公开事件流（死讯 / 放逐 / 警徽流转 等）\n\
                    - **内心独白历史**：你过去几轮 thinking 字段写过什么——这是你的策略弧线\n\
                    - **你的公开发言**：你之前在场上说过的话，前后矛盾会被场上抓出\n\n\
                    ## thinking 字段怎么写\n\
                    - 这段 thinking 是**给未来的你**看的，下一次决策时会回到你的上下文里\n\
                    - 你怎么打、按什么思路、押什么注，都写下来；越具体后面越好延续\n\
                    - 战术、跳什么、报不报身份、跟不跟票、要不要弃权——都你自己定，没标准答案\n\n";
    match persona {
        Some(p) => format!(
            "{}## 你的流派烙印：**{}**\n{}",
            preamble,
            p.label(),
            p.werewolf_description()
        ),
        None => format!("{}你没有特定流派，按自己直觉决策。", preamble),
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
) -> (WolfPickDecision, Option<String>) {
    // 候选 = 所有存活玩家(包括狼队友 / 自己)。自刀是狼方合法策略,
    // 让 LLM 看到全部目标 + 在 prompt 里告知"自刀=送验/救场"才能让它
    // 在 EV 评估时把这条路径纳入考虑。
    let candidates: Vec<(usize, String)> = view
        .players
        .iter()
        .filter(|(_, _, alive)| *alive)
        .map(|(i, n, _)| (*i, n.clone()))
        .collect();
    if candidates.is_empty() {
        return (WolfPickDecision { target_idx: 0, chat: None }, None);
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
         你和狼队友需要合议夜杀一名玩家。\n\
         **默认应该刀好人**(村民 / 神职),这是最常规的狼方收益。\n\
         **自刀(刀狼队友 / 自己) 是合法的策略性选择**,但只在如下场景考虑:\n\
         - 送验:队友被预言家公开查杀后,自刀让队友夜里死亡,制造『假预言家查错』的话术空间\n\
         - 骗药:让女巫看到非狼死亡现场,套出救药 / 让她不敢用药\n\
         - 自爆诱饵:用一个边缘队友换一个关键好人查验信息\n\
         自刀有风险(直接减狼方人数 / 暴露身份),除非有清晰的局面收益,否则**优先刀好人**。\n\
         {}\n\n\
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
        Ok(content) => match sonic_rs::from_str::<WolfPickResp>(&content) {
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
                        .map(str::to_owned)
                } else {
                    None
                };
                (WolfPickDecision { target_idx, chat }, norm_thinking(r.thinking))
            }
            Err(e) => {
                warn!(?e, content = %content, "wolf JSON parse failed");
                (WolfPickDecision { target_idx: fallback, chat: None }, None)
            }
        },
        Err(e) => {
            warn!(?e, "wolf LLM call failed");
            (WolfPickDecision { target_idx: fallback, chat: None }, None)
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
    thinking: Option<String>,
}

pub async fn seer_pick(
    llm: &LlmClient,
    view: &PublicView<'_>,
    history: &AttemptHistory,
) -> (usize, Option<String>) {
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
        return (0, None);
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
        Ok(content) => match sonic_rs::from_str::<SeerCheckResp>(&content) {
            Ok(r) => {
                let target = if candidates.iter().any(|(i, _)| *i == r.target_idx) {
                    r.target_idx
                } else {
                    warn!(target = r.target_idx, "seer returned illegal idx, fallback");
                    fallback
                };
                (target, norm_thinking(r.thinking))
            }
            Err(e) => {
                warn!(?e, content = %content, "seer JSON parse failed");
                (fallback, None)
            }
        },
        Err(e) => {
            warn!(?e, "seer LLM call failed");
            (fallback, None)
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
) -> (WitchDecision, Option<String>) {
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
            return (WitchDecision::Skip, None);
        }
    };
    let parsed: WitchResp = match sonic_rs::from_str(&raw) {
        Ok(r) => r,
        Err(e) => {
            warn!(?e, content = %raw, "witch JSON parse failed");
            return (WitchDecision::Skip, None);
        }
    };
    let thinking = norm_thinking(parsed.thinking.clone());

    let decision = match parsed.action.to_lowercase().as_str() {
        "save" => {
            if !game.witch_save_used && game.night_victim.is_some() {
                WitchDecision::Save
            } else {
                WitchDecision::Skip
            }
        }
        "poison" => {
            if game.witch_poison_used {
                WitchDecision::Skip
            } else if let Some(idx) = parsed.poison_target_idx {
                if poison_candidates.iter().any(|(i, _)| *i == idx) {
                    WitchDecision::Poison(idx)
                } else {
                    WitchDecision::Skip
                }
            } else {
                WitchDecision::Skip
            }
        }
        _ => WitchDecision::Skip,
    };
    (decision, thinking)
}

// ============================================================================
// 白天投票
// ============================================================================

#[derive(Debug, Deserialize)]
struct VoteResp {
    /// 玩家 idx 或 -1 弃权
    target_idx: i64,
    #[serde(default)]
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
) -> (VoteDecision, Option<String>) {
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
         返回 JSON: {{\"target_idx\": <整数 idx 或 -1>, \"thinking\": \"<给未来自己看的完整推理>\"}}",
        view.me_role.label(),
        team,
        persona_line(view.persona),
        RULES,
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
            return (VoteDecision { target_idx: None }, None);
        }
    };
    let parsed: VoteResp = match sonic_rs::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            warn!(?e, content = %raw, "vote JSON parse failed");
            return (VoteDecision { target_idx: None }, None);
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
    (VoteDecision { target_idx: target }, norm_thinking(parsed.thinking))
}

// ============================================================================
// 猎人
// ============================================================================

#[derive(Debug, Deserialize)]
struct HunterResp {
    /// 目标 idx 或 -1 不开枪
    target_idx: i64,
    #[serde(default)]
    thinking: Option<String>,
}

pub async fn hunter_pick(
    llm: &LlmClient,
    view: &PublicView<'_>,
    history: &AttemptHistory,
) -> (Option<usize>, Option<String>) {
    let candidates: Vec<(usize, String)> = view
        .players
        .iter()
        .filter(|(i, _, alive)| *alive && *i != view.me_idx)
        .map(|(i, n, _)| (*i, n.clone()))
        .collect();
    if candidates.is_empty() {
        return (None, None);
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
            return (None, None);
        }
    };
    let parsed: HunterResp = match sonic_rs::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            warn!(?e, content = %raw, "hunter JSON parse failed");
            return (None, None);
        }
    };
    let thinking = norm_thinking(parsed.thinking.clone());
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
    (target, thinking)
}

// ============================================================================
// 守卫
// ============================================================================

#[derive(Debug, Deserialize)]
struct GuardResp {
    target_idx: usize,
    #[serde(default)]
    thinking: Option<String>,
}

pub async fn guard_pick(
    llm: &LlmClient,
    view: &PublicView<'_>,
    game: &WolfGame,
    history: &AttemptHistory,
) -> (usize, Option<String>) {
    let candidates: Vec<(usize, String)> = view
        .players
        .iter()
        .filter(|(i, _, alive)| *alive && game.last_guard_target != Some(*i))
        .map(|(i, n, _)| (*i, n.clone()))
        .collect();
    if candidates.is_empty() {
        return (view.me_idx, None);
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
        Ok(c) => match sonic_rs::from_str::<GuardResp>(&c) {
            Ok(r) if candidates.iter().any(|(i, _)| *i == r.target_idx) => {
                (r.target_idx, norm_thinking(r.thinking))
            }
            Ok(r) => (fallback, norm_thinking(r.thinking)),
            _ => (fallback, None),
        },
        Err(e) => {
            warn!(?e, "guard LLM call failed");
            (fallback, None)
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
    thinking: Option<String>,
}

/// AI 决定是否上警。预言家通常会上（领跳），狼也可能上去抢警；村民/女巫/猎人/守卫看局势。
pub async fn sheriff_run(llm: &LlmClient, view: &PublicView<'_>) -> (bool, Option<String>) {
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
        Ok(c) => match sonic_rs::from_str::<SheriffRunResp>(&c) {
            Ok(r) => (r.run, norm_thinking(r.thinking)),
            Err(e) => {
                warn!(?e, content = %c, "sheriff_run JSON parse failed");
                (view.me_role == Role::Seer, None)
            }
        },
        Err(e) => {
            warn!(?e, "sheriff_run LLM call failed");
            (view.me_role == Role::Seer, None)
        }
    }
}

#[derive(Debug, Deserialize)]
struct SheriffVoteResp {
    target_idx: i64,
    #[serde(default)]
    thinking: Option<String>,
}

/// AI 在警长投票中选一名候选人。-1 = 弃权。
pub async fn sheriff_vote(
    llm: &LlmClient,
    view: &PublicView<'_>,
    candidates: &[(usize, String)],
    history: &AttemptHistory,
) -> (Option<usize>, Option<String>) {
    if candidates.is_empty() {
        return (None, None);
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
         在候选人中投出你支持的警长，或弃权 (target_idx = -1)。\
         候选人不能投票（包含你自己若是候选人）。\n\n\
         返回 JSON: {{\"target_idx\": <整数 idx 或 -1>, \"thinking\": \"<给未来自己看的完整推理>\"}}",
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
        Ok(c) => match sonic_rs::from_str::<SheriffVoteResp>(&c) {
            Ok(r) => {
                let target = if r.target_idx >= 0 {
                    let idx = r.target_idx as usize;
                    if candidates.iter().any(|(i, _)| *i == idx) {
                        Some(idx)
                    } else {
                        None
                    }
                } else {
                    None
                };
                (target, norm_thinking(r.thinking))
            }
            _ => (None, None),
        },
        Err(e) => {
            warn!(?e, "sheriff_vote LLM call failed");
            (None, None)
        }
    }
}

#[derive(Debug, Deserialize)]
struct BadgeResp {
    target_idx: i64,
    #[serde(default)]
    thinking: Option<String>,
}

/// AI 警长临死决定警徽去向。-1 = 撕毁，否则移交给 idx。
pub async fn badge_pass(
    llm: &LlmClient,
    view: &PublicView<'_>,
    history: &AttemptHistory,
) -> (Option<usize>, Option<String>) {
    let candidates: Vec<(usize, String)> = view
        .players
        .iter()
        .filter(|(i, _, alive)| *alive && *i != view.me_idx)
        .map(|(i, n, _)| (*i, n.clone()))
        .collect();
    if candidates.is_empty() {
        return (None, None);
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
        Ok(c) => match sonic_rs::from_str::<BadgeResp>(&c) {
            Ok(r) => {
                let target = if r.target_idx >= 0 {
                    let idx = r.target_idx as usize;
                    if candidates.iter().any(|(i, _)| *i == idx) {
                        Some(idx)
                    } else {
                        None
                    }
                } else {
                    None
                };
                (target, norm_thinking(r.thinking))
            }
            _ => (None, None),
        },
        Err(e) => {
            warn!(?e, "badge_pass LLM call failed");
            (None, None)
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
    thinking: Option<String>,
}

/// AI 上警竞选发言：候选人轮流发言，需要拉票 / 自报身份 / 攻击对手。
/// 失败时返回简短默认词，避免阻塞。
pub async fn sheriff_speech(llm: &LlmClient, view: &PublicView<'_>) -> (String, Option<String>) {
    let team = if view.me_role.is_wolf() {
        "你的阵营是狼。"
    } else {
        "你的阵营是好人。"
    };
    let system = format!(
        "你是飞书群里玩狼人杀的玩家，现在是第 1 天上警阶段，你正在做竞选发言。\
         你的真实身份是 **{}**。{}\n\
         {}\n\n{}\n\n## 任务\n\
         发表一段公开广播的竞选发言。跳什么身份、报不报查验、怎么拉票——都你自己定。\n\n\
         返回 JSON: {{\"speech\": \"<发言>\", \"thinking\": \"<给未来自己看的完整推理>\"}}",
        view.me_role.label(),
        team,
        persona_line(view.persona),
        RULES,
    );
    let user = view.render();
    match llm.chat_json(&system, &user).await {
        Ok(c) => match sonic_rs::from_str::<SpeechResp>(&c) {
            Ok(r) => {
                let thinking = norm_thinking(r.thinking.clone());
                let speech = r
                    .speech
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
                    .unwrap_or_else(|| "我支持平和过渡，先听其他玩家。".into());
                (speech, thinking)
            }
            Err(e) => {
                warn!(?e, content = %c, "sheriff_speech parse failed");
                ("我先表态。".into(), None)
            }
        },
        Err(e) => {
            warn!(?e, "sheriff_speech LLM call failed");
            ("我先表态。".into(), None)
        }
    }
}

/// AI 警长选择 警上 / 警下 起手。返回 true = 警上（顺时针）。
pub async fn sheriff_direction(llm: &LlmClient, view: &PublicView<'_>) -> (bool, Option<String>) {
    #[derive(Debug, Deserialize)]
    struct DirResp {
        clockwise: bool,
        #[serde(default)]
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
        Ok(c) => match sonic_rs::from_str::<DirResp>(&c) {
            Ok(r) => (r.clockwise, norm_thinking(r.thinking)),
            Err(_) => (true, None), // 默认警上
        },
        Err(e) => {
            warn!(?e, "sheriff_direction LLM call failed");
            (true, None)
        }
    }
}

/// AI 死亡遗言。要求按角色 / 阵营立场最大化信息价值。
pub async fn last_words(llm: &LlmClient, view: &PublicView<'_>) -> (String, Option<String>) {
    let team = if view.me_role.is_wolf() {
        "你的阵营是狼。"
    } else {
        "你的阵营是好人。"
    };
    let system = format!(
        "你是飞书群里玩狼人杀的玩家，**你刚刚死亡，正在发表遗言**。\
         你的真实身份是 **{}**。{}\n\
         {}\n\n{}\n\n## 任务\n\
         发表你的遗言，会公开广播给所有玩家。说什么、报不报身份、留什么信号——都你自己定。可空（沉默）也行。\n\n\
         返回 JSON: {{\"speech\": \"<遗言>\", \"thinking\": \"<给未来自己看的完整推理>\"}}",
        view.me_role.label(),
        team,
        persona_line(view.persona),
        RULES,
    );
    let user = view.render();
    match llm.chat_json(&system, &user).await {
        Ok(c) => match sonic_rs::from_str::<SpeechResp>(&c) {
            Ok(r) => {
                let thinking = norm_thinking(r.thinking.clone());
                let speech = r
                    .speech
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
                    .unwrap_or_default();
                (speech, thinking)
            }
            Err(_) => (String::new(), None),
        },
        Err(e) => {
            warn!(?e, "last_words LLM call failed");
            (String::new(), None)
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
) -> (DyingShooterDecision, Option<String>) {
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
        "你是飞书群里的狼人杀玩家，扮演 **{}**，刚刚死亡。{}\n\
         {}\n\n{}\n\n## 任务（一次性两件事）\n\
         1. **遗言**：公开广播给所有玩家\n\
         2. **开枪**：可选一名存活的非自己玩家带走 (target_idx = idx)，\
            或选择不开枪 (target_idx = -1)\n\n\
         **遗言和开枪决策必须一致**——遗言里说『我带走 N 号』就 target_idx = N；\
         遗言里说不开枪 target_idx = -1；遗言里嫁祸 / 留推理也要配合开枪叙事。\n\n\
         注：公开广播只显示『X 临死前开枪带走 Y』或『X 选择不开枪』，**不会标记你的身份**\
         （猎人和狼王共享开枪技能，狼王惯例伪装成猎人）。\n\n\
         返回 JSON: {{\"speech\": \"<遗言>\", \"target_idx\": <整数 idx 或 -1>, \"thinking\": \"<给未来自己看的完整推理>\"}}",
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
        "{}\n\n## 候选开枪目标（仅这些 idx 合法，或 -1 不开枪）\n{}",
        view.render(),
        cands.join("\n"),
    );

    let raw = match chat_with_history(llm, system, user, history).await {
        Ok(c) => c,
        Err(e) => {
            warn!(?e, "dying_hunter_combined LLM call failed");
            return (
                DyingShooterDecision {
                    speech: String::new(),
                    target: None,
                },
                None,
            );
        }
    };
    let parsed: DyingShooterResp = match sonic_rs::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            warn!(?e, content = %raw, "dying_hunter_combined JSON parse failed");
            return (
                DyingShooterDecision {
                    speech: String::new(),
                    target: None,
                },
                None,
            );
        }
    };
    let thinking = norm_thinking(parsed.thinking.clone());
    let speech = parsed
        .speech
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
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
    (DyingShooterDecision { speech, target }, thinking)
}

/// AI 白天轮流发言。带上当天死讯 / 历史发言上下文，要求按角色立场说话。
pub async fn day_speech(llm: &LlmClient, view: &PublicView<'_>) -> (String, Option<String>) {
    let team = if view.me_role.is_wolf() {
        "你的阵营是狼。"
    } else {
        "你的阵营是好人。"
    };
    let system = format!(
        "你是飞书群里玩狼人杀的玩家，现在是白天，轮到你发言（一次性单次发言，提交后回合结束）。\
         你的真实身份是 **{}**。{}\n\
         {}\n\n{}\n\n## 任务\n\
         发表一段公开广播给全场的看法。怎么说、说什么、要不要藏身份、跟不跟其他人——都你自己定。\n\n\
         返回 JSON: {{\"speech\": \"<发言>\", \"thinking\": \"<给未来自己看的完整推理>\"}}",
        view.me_role.label(),
        team,
        persona_line(view.persona),
        RULES,
    );
    let user = view.render();
    match llm.chat_json(&system, &user).await {
        Ok(c) => match sonic_rs::from_str::<SpeechResp>(&c) {
            Ok(r) => {
                let thinking = norm_thinking(r.thinking.clone());
                let speech = r
                    .speech
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
                    .unwrap_or_default();
                (speech, thinking)
            }
            Err(e) => {
                warn!(?e, content = %c, "day_speech parse failed");
                (String::new(), None)
            }
        },
        Err(e) => {
            warn!(?e, "day_speech LLM call failed");
            (String::new(), None)
        }
    }
}


/// 把 (potentially) failed JSON parse into anyhow context for fmt'ing.
#[allow(dead_code)]
fn parse_or_err<T: for<'de> Deserialize<'de>>(s: &str) -> Result<T> {
    sonic_rs::from_str(s).with_context(|| format!("LLM JSON: {s}"))
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
