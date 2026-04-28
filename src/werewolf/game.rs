//! 狼人杀 (Werewolf) game state machine.
//!
//! 屠城规则：
//! - 好人胜：所有狼人死亡
//! - 狼人胜：存活狼人 ≥ 存活好人
//!
//! 角色：狼人 / 村民 / 预言家 / 女巫 / 猎人
//!
//! 阶段（每天循环）：
//! WolvesPick → SeerPick → WitchAct → DayReveal
//! → (HunterShoot if hunter died)
//! → SheriffSpeech (10+ 板) → SheriffVote → DaySpeech → DayVote → DayLynch
//! → (HunterShoot if hunter lynched)
//! → 检查胜负 → 下一夜 / Ended

use crate::game::Persona;
use anyhow::{anyhow, Result};
use rand::seq::SliceRandom;
use serde::{Deserialize, Serialize};

/// 7 种身份。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    Werewolf,
    /// 狼王：狼阵营，被投票或被狼刀杀死时可开枪带走一人；被毒不能开枪。
    WolfKing,
    Villager,
    Seer,
    Witch,
    Hunter,
    Guard,
}

impl Role {
    pub fn label(self) -> &'static str {
        match self {
            Role::Werewolf => "狼人",
            Role::WolfKing => "狼王",
            Role::Villager => "村民",
            Role::Seer => "预言家",
            Role::Witch => "女巫",
            Role::Hunter => "猎人",
            Role::Guard => "守卫",
        }
    }

    pub fn emoji(self) -> &'static str {
        match self {
            Role::Werewolf => "🐺",
            Role::WolfKing => "👑",
            Role::Villager => "👨‍🌾",
            Role::Seer => "🔮",
            Role::Witch => "🧪",
            Role::Hunter => "🏹",
            Role::Guard => "🛡️",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Role::Werewolf => "夜晚和狼队友一起决定要杀的人。白天伪装成好人混进投票。",
            Role::WolfKing => "狼阵营。和其他狼一起夜里杀人。被投票放逐 / 被同伴反水（罕见）/ 被狼刀（不会发生）时可开枪带走一人；**被毒不能开枪**。预言家查验显示为狼。",
            Role::Villager => "没有夜间技能。靠白天投票把狼找出来。",
            Role::Seer => "每晚可以查验一名玩家的身份（狼人 / 好人）。",
            Role::Witch => "知道当晚狼人要杀谁，可选用救药救人（仅 1 次，全局），或用毒药毒一人（仅 1 次，全局）。同晚不可同时救+毒。",
            Role::Hunter => "被投票放逐 / 被狼人杀害时可开枪带走任意一名玩家。被毒杀则不能开枪。",
            Role::Guard => "每晚守护一名存活玩家（含自己），不能连续两晚守同一个人。同守同救：被守 + 被救者依然死亡。",
        }
    }

    pub fn is_wolf(self) -> bool {
        matches!(self, Role::Werewolf | Role::WolfKing)
    }
}

/// 给定玩家数量的角色配比（标准 9-12 人板）。
///
/// - 9 人：3 狼 + 预言家 + 女巫 + 猎人 + 3 村民（不上警）
/// - 10 人：2 狼 + **狼王** + 预言家 + 女巫 + 猎人 + 守卫 + 3 村民
/// - 11 人：2 狼 + **狼王** + 预言家 + 女巫 + 猎人 + 守卫 + 4 村民
/// - 12 人：3 狼 + **狼王** + 预言家 + 女巫 + 猎人 + 守卫 + 4 村民
///
/// 10+ 人板替换 1 个普通狼为狼王，狼总数不变。
pub fn role_distribution(n: usize) -> Result<Vec<Role>> {
    use Role::*;
    let roles: Vec<Role> = match n {
        9 => vec![
            Werewolf, Werewolf, Werewolf, Seer, Witch, Hunter, Villager, Villager, Villager,
        ],
        10 => vec![
            Werewolf, Werewolf, WolfKing, Seer, Witch, Hunter, Guard, Villager, Villager, Villager,
        ],
        11 => vec![
            Werewolf, Werewolf, WolfKing, Seer, Witch, Hunter, Guard, Villager, Villager, Villager,
            Villager,
        ],
        12 => vec![
            Werewolf, Werewolf, Werewolf, WolfKing, Seer, Witch, Hunter, Guard, Villager, Villager,
            Villager, Villager,
        ],
        _ => return Err(anyhow!("狼人杀需要 9-12 名玩家，当前 {}", n)),
    };
    Ok(roles)
}

/// 是否启用上警（警长选举）：10 人及以上的板。9 人板按惯例不上警。
pub fn has_sheriff_election(n: usize) -> bool {
    n >= 10
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Stage {
    /// 大厅，等待玩家加入。
    Lobby,
    /// 夜晚——守卫守护。
    GuardPick,
    /// 夜晚——狼人投票杀人。
    WolvesPick,
    /// 夜晚——预言家查验。
    SeerPick,
    /// 夜晚——女巫救人 / 毒人 / 跳过。
    WitchAct,
    /// 白天——公布昨夜死亡。
    DayReveal,
    /// 白天 1——上警阶段：玩家选择是否参选警长。
    SheriffNominate,
    /// 白天 1——警长候选人轮流竞选发言。
    SheriffSpeech,
    /// 白天 1——上警投票：非候选人投出警长。
    SheriffVote,
    /// 警长产生后选择白天发言方向（警上 / 警下）。
    SheriffPickDirection,
    /// 白天——按顺序轮流发言（替代旧的并发 quip 模式）。
    DaySpeech,
    /// 白天——投票放逐。
    DayVote,
    /// 死亡遗言：被狼刀 / 放逐者依次说话。被毒 / 被开枪者无遗言。
    LastWords,
    /// 猎人开枪（被杀 / 被放逐时触发，被毒不触发）。
    HunterShoot,
    /// 警长死亡，等待移交 / 撕毁警徽。
    BadgePass,
    /// 游戏已结束。
    Ended,
}

impl Stage {
    pub fn label(self) -> &'static str {
        match self {
            Stage::Lobby => "等待玩家",
            Stage::GuardPick => "夜·守卫行动",
            Stage::WolvesPick => "夜·狼人行动",
            Stage::SeerPick => "夜·预言家查验",
            Stage::WitchAct => "夜·女巫行动",
            Stage::DayReveal => "白天·公布死讯",
            Stage::SheriffNominate => "白天·上警阶段",
            Stage::SheriffSpeech => "白天·上警发言",
            Stage::SheriffVote => "白天·警长投票",
            Stage::SheriffPickDirection => "白天·警长选择方向",
            Stage::DaySpeech => "白天·轮流发言",
            Stage::DayVote => "白天·投票",
            Stage::LastWords => "死亡遗言",
            Stage::HunterShoot => "猎人开枪",
            Stage::BadgePass => "警徽流转",
            Stage::Ended => "游戏结束",
        }
    }
}

/// 一名玩家在游戏中的角色和状态。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Player {
    pub open_id: String,
    pub name: String,
    /// 分配后才有值；大厅状态下为 None。
    pub role: Option<Role>,
    pub alive: bool,
    pub is_ai: bool,
    #[serde(default)]
    pub persona: Option<Persona>,
}

/// 死因，用于公告渲染。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeathCause {
    /// 被狼人击杀。
    WolfKill,
    /// 被女巫毒杀。
    Poison,
    /// 白天被投票放逐。
    Lynch,
    /// 被猎人开枪带走。
    HunterShot,
}

impl DeathCause {
    pub fn label(self) -> &'static str {
        match self {
            DeathCause::WolfKill => "被狼人杀害",
            DeathCause::Poison => "被毒杀",
            DeathCause::Lynch => "被投票放逐",
            DeathCause::HunterShot => "被猎人开枪带走",
        }
    }
}

/// 一夜或一天结束时的死亡事件，用于事件历史。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeathEvent {
    pub day: u32,
    pub night: bool,
    pub player_idx: usize,
    pub cause: DeathCause,
}

/// 预言家某次查验的结果（白天 AI 决策时用得到）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SeerCheck {
    pub day: u32,
    pub target_idx: usize,
    pub is_wolf: bool,
}

/// 投票或夜杀的提交：一票 = 一对（投票人, 目标）。用 Vec 替代 HashMap，
/// 避免 serde_json 对非字符串键的兼容问题，同时保留写入顺序便于平票决策。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Ballot {
    pub votes: Vec<(usize, Option<usize>)>, // (voter_idx, Some(target) | None=弃权)
}

impl Ballot {
    pub fn cast(&mut self, voter: usize, target: Option<usize>) {
        if let Some(slot) = self.votes.iter_mut().find(|(v, _)| *v == voter) {
            slot.1 = target;
        } else {
            self.votes.push((voter, target));
        }
    }

    pub fn for_voter(&self, voter: usize) -> Option<Option<usize>> {
        self.votes.iter().find(|(v, _)| *v == voter).map(|(_, t)| *t)
    }

    pub fn clear(&mut self) {
        self.votes.clear();
    }
}

/// 整盘狼人杀的全部状态。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WolfGame {
    pub chat_id: String,
    pub players: Vec<Player>,
    pub stage: Stage,
    /// 第几天（1-indexed），开局首夜 = 1。
    pub day: u32,
    /// 整桌已开过几局，重置每次 +1，给"陈旧按钮"判定用。
    pub game_count: u32,
    /// 大厅卡片的 message_id，原地更新。
    pub lobby_msg_id: Option<String>,

    // ---- 当晚临时状态（每夜清空） ----
    pub wolf_kill_votes: Vec<(usize, usize)>, // wolf_idx -> target_idx
    /// 狼人当晚的聊天记录：(wolf_idx, message)。仅在有人类狼时启用。
    #[serde(default)]
    pub wolf_chat: Vec<(usize, String)>,
    /// 已点击"我决定了"的狼人 idx 列表（每夜清空）。
    #[serde(default)]
    pub wolf_ready: Vec<usize>,
    /// 每只狼当晚的行动卡 message_id，给 update_card 复用。
    #[serde(default)]
    pub wolf_night_msgs: Vec<(usize, String)>,
    pub seer_check_target: Option<usize>,
    pub witch_save_choice: Option<bool>,
    pub witch_poison_target: Option<usize>,
    /// 守卫这一晚守护的目标（含自己）。
    #[serde(default)]
    pub guard_target: Option<usize>,
    /// 狼人投票得出的最终目标（女巫看的就是这个）；None = 空刀。
    pub night_victim: Option<usize>,
    pub witch_acted: bool, // 女巫这一夜是否已经做出选择（救/毒/跳过）

    // ---- 跨晚持久状态 ----
    pub witch_save_used: bool,
    pub witch_poison_used: bool,
    pub seer_history: Vec<SeerCheck>,
    /// 上一晚守卫守的目标（不能连续两晚守同一个人）。
    #[serde(default)]
    pub last_guard_target: Option<usize>,

    // ---- 白天临时状态（每天清空） ----
    pub day_votes: Ballot,
    pub last_night_deaths: Vec<usize>,    // 渲染白天公告
    pub last_day_lynched: Option<usize>,  // 上次放逐结果
    /// 白天发言顺序（玩家 idx 列表）。
    #[serde(default)]
    pub day_speech_order: Vec<usize>,
    /// 当前轮到的玩家在 day_speech_order 中的位置。
    #[serde(default)]
    pub day_speech_idx: usize,
    /// 已发表的白天发言：(speaker_idx, text)。空 text = 弃权 / 沉默。
    #[serde(default)]
    pub day_speeches: Vec<(usize, String)>,
    /// 白天发言公开卡 message_id（update_card 复用）。
    #[serde(default)]
    pub day_speech_public_msg: Option<String>,
    /// 当前发言人收到的 ephemeral 输入卡 message_id（update_card 复用）。
    #[serde(default)]
    pub day_speech_private_msg: Option<String>,

    // ---- 警长 / 上警 ----
    /// 当前警长玩家索引；None = 没有（未选 / 警徽撕毁）。
    #[serde(default)]
    pub sheriff_idx: Option<usize>,
    /// 上警阶段是否启用（取决于人数 ≥ 10）。
    #[serde(default)]
    pub sheriff_enabled: bool,
    /// 上警阶段每位玩家是否参选：(player_idx, is_running)。
    #[serde(default)]
    pub sheriff_nominations: Vec<(usize, bool)>,
    /// 上警投票（候选人不能投票）。
    #[serde(default)]
    pub sheriff_votes: Ballot,
    /// 警长候选人发言顺序（按提名顺序）。
    #[serde(default)]
    pub sheriff_speech_order: Vec<usize>,
    /// 当前发言候选人在 sheriff_speech_order 中的位置。
    #[serde(default)]
    pub sheriff_speech_idx: usize,
    /// 警长候选人发言记录。
    #[serde(default)]
    pub sheriff_speeches: Vec<(usize, String)>,
    /// 上警发言公开卡 message_id。
    #[serde(default)]
    pub sheriff_speech_public_msg: Option<String>,
    /// 上警发言时给当前候选人的 ephemeral 卡 message_id。
    #[serde(default)]
    pub sheriff_speech_private_msg: Option<String>,

    // ---- 死亡遗言 ----
    /// 待说遗言的玩家队列（按死亡顺序）。被毒 / 被开枪者不在内。
    #[serde(default)]
    pub last_words_queue: Vec<usize>,
    #[serde(default)]
    pub last_words_idx: usize,
    #[serde(default)]
    pub last_words_speeches: Vec<(usize, String)>,
    #[serde(default)]
    pub last_words_public_msg: Option<String>,
    #[serde(default)]
    pub last_words_private_msg: Option<String>,
    /// 遗言全部说完后回到哪个阶段（DayReveal / DayVote 等）。
    #[serde(default)]
    pub last_words_post_stage: Option<Stage>,
    /// 待移交警徽的玩家（即将死亡的警长）。
    #[serde(default)]
    pub pending_badge: Option<usize>,
    /// 警徽移交后回到哪个阶段。
    #[serde(default)]
    pub pending_badge_post_stage: Option<Stage>,

    // ---- 猎人临牌 ----
    /// 待开枪的猎人玩家索引；进入 `HunterShoot` 阶段后由这个字段标识谁需要开枪。
    pub pending_hunter: Option<usize>,
    /// 猎人因什么死的，决定开枪后回到哪个阶段（晚上死了→白天揭晓；白天死了→检查胜负后进夜晚）。
    pub pending_hunter_post_stage: Option<Stage>,
    /// AI 在遗言阶段就一并决策的开枪目标，HunterShoot 阶段直接使用而不再调一次 LLM。
    /// 语义：`None` = 还没决策；`Some(None)` = 决策不开枪；`Some(Some(idx))` = 决策打 idx。
    /// 这是为了让 AI 的"遗言 + 开枪"在**同一个 LLM 上下文**里产出一致的决定，避免
    /// 两次独立调用导致言行矛盾（说"我带走 4 号"但实际不开枪）。
    #[serde(default)]
    pub pending_hunter_ai_decision: Option<Option<usize>>,

    // ---- 公开行动日志 ----
    /// 给 AI 看的事件历史，自然语言一行一条。
    pub event_log: Vec<String>,

    /// 死亡日志（结构化），结算时显示。
    pub deaths: Vec<DeathEvent>,

    /// 结构化复盘日志：所有关键事件按发生顺序追加，结算 summary 时分组渲染。
    /// 与 event_log（自然语言文本，给 AI prompt 用）平行存在。
    #[serde(default)]
    pub recap_log: Vec<RecapEvent>,

    /// AI 玩家的私密"心路历程"——每次 LLM 调用返回的 `thinking` 字段都落地到这里。
    /// 下一次同一玩家做决策时，把他自己的过往 thinking 拼回 prompt 上下文，
    /// 这样他的策略弧线能跨轮延续（而不是每次调用都从头想）。**严格私密**：
    /// 只有该玩家自己能在 build_view 里看到自己的条目，绝不暴露给其他 AI。
    #[serde(default)]
    pub thinking_log: Vec<ThinkingEntry>,
}

/// AI 私密 thinking 条目。每次调用 LLM 后由 handlers 写入。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThinkingEntry {
    pub player: usize,
    pub day: u32,
    pub kind: ThinkingKind,
    pub thinking: String,
}

/// 标记一条 thinking 是哪个决策点产生的。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ThinkingKind {
    GuardPick,
    WolfPick,
    SeerCheck,
    WitchAct,
    SheriffRun,
    SheriffSpeech,
    SheriffVote,
    SheriffDirection,
    DaySpeech,
    DayVote,
    LastWords,
    DyingShoot,
    BadgePass,
    HunterShoot,
}

impl ThinkingKind {
    pub fn label(self) -> &'static str {
        match self {
            ThinkingKind::GuardPick => "守卫守护",
            ThinkingKind::WolfPick => "狼夜杀",
            ThinkingKind::SeerCheck => "预言家查验",
            ThinkingKind::WitchAct => "女巫行动",
            ThinkingKind::SheriffRun => "上警决定",
            ThinkingKind::SheriffSpeech => "上警发言",
            ThinkingKind::SheriffVote => "警长投票",
            ThinkingKind::SheriffDirection => "警上警下选向",
            ThinkingKind::DaySpeech => "白天发言",
            ThinkingKind::DayVote => "白天投票",
            ThinkingKind::LastWords => "遗言",
            ThinkingKind::DyingShoot => "临死开枪+遗言",
            ThinkingKind::BadgePass => "警徽流转",
            ThinkingKind::HunterShoot => "猎人开枪",
        }
    }
}

/// 复盘卡片要展示的所有结构化事件类型。游戏过程中按发生顺序追加到
/// `WolfGame::recap_log`，结算时按天 / 夜 / 类型分组渲染。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RecapEvent {
    /// 守卫守护
    GuardProtect { day: u32, target: usize },
    /// 狼人最终合议击杀目标（None = 空刀）
    WolfFinalTarget { day: u32, target: Option<usize> },
    /// 预言家查验
    SeerCheck { day: u32, target: usize, is_wolf: bool },
    /// 女巫行动 (save = true 表示救了今晚的猎物；poison = Some(idx) 表示毒了谁)
    Witch { day: u32, save: bool, poison: Option<usize> },
    /// 死亡（含死因）
    Death { day: u32, night: bool, player: usize, cause: DeathCause },
    /// 死亡遗言
    LastWords { day: u32, night: bool, player: usize, text: String },
    /// 上警候选人公告
    SheriffCandidates { candidates: Vec<usize> },
    /// 上警发言（候选人）
    SheriffSpeech { player: usize, text: String },
    /// 警长当选（None = 流局）
    SheriffElected { player: Option<usize> },
    /// 警长选起手方向（true = 警上 / 顺时针）
    SheriffDirection { clockwise: bool },
    /// 白天轮流发言
    DaySpeech { day: u32, player: usize, text: String },
    /// 单条投票（含权重，警长 = 3，普通 = 2）
    DayVoteCast { day: u32, voter: usize, target: Option<usize>, weight: u32 },
    /// 当天放逐结果（None = 平票 / 全弃权流局）
    DayLynch { day: u32, target: Option<usize> },
    /// 猎人 / 狼王开枪
    HunterShot { day: u32, shooter: usize, target: Option<usize> },
    /// 警徽流转（to = None 表示撕毁）
    BadgePass { day: u32, from: usize, to: Option<usize> },
}

impl WolfGame {
    pub fn new(chat_id: String) -> Self {
        Self {
            chat_id,
            players: vec![],
            stage: Stage::Lobby,
            day: 0,
            game_count: 0,
            lobby_msg_id: None,
            wolf_kill_votes: vec![],
            wolf_chat: vec![],
            wolf_ready: vec![],
            wolf_night_msgs: vec![],
            seer_check_target: None,
            witch_save_choice: None,
            witch_poison_target: None,
            guard_target: None,
            night_victim: None,
            witch_acted: false,
            witch_save_used: false,
            witch_poison_used: false,
            seer_history: vec![],
            last_guard_target: None,
            day_votes: Ballot::default(),
            last_night_deaths: vec![],
            last_day_lynched: None,
            day_speech_order: vec![],
            day_speech_idx: 0,
            day_speeches: vec![],
            day_speech_public_msg: None,
            day_speech_private_msg: None,
            sheriff_idx: None,
            sheriff_enabled: false,
            sheriff_nominations: vec![],
            sheriff_votes: Ballot::default(),
            sheriff_speech_order: vec![],
            sheriff_speech_idx: 0,
            sheriff_speeches: vec![],
            sheriff_speech_public_msg: None,
            sheriff_speech_private_msg: None,
            last_words_queue: vec![],
            last_words_idx: 0,
            last_words_speeches: vec![],
            last_words_public_msg: None,
            last_words_private_msg: None,
            last_words_post_stage: None,
            pending_badge: None,
            pending_badge_post_stage: None,
            pending_hunter: None,
            pending_hunter_post_stage: None,
            pending_hunter_ai_decision: None,
            event_log: vec![],
            deaths: vec![],
            recap_log: vec![],
            thinking_log: vec![],
        }
    }

    /// 追加一条 AI 私密 thinking。空白文本忽略。
    pub fn push_thinking(&mut self, player: usize, kind: ThinkingKind, thinking: String) {
        let trimmed = thinking.trim();
        if trimmed.is_empty() {
            return;
        }
        self.thinking_log.push(ThinkingEntry {
            player,
            day: self.day,
            kind,
            thinking: trimmed.to_string(),
        });
    }

    pub fn add_player(&mut self, open_id: String, name: String) -> Result<()> {
        self.add_player_inner(open_id, name, false, None)
    }

    pub fn add_ai_player(
        &mut self,
        open_id: String,
        name: String,
        persona: Persona,
    ) -> Result<()> {
        self.add_player_inner(open_id, name, true, Some(persona))
    }

    fn add_player_inner(
        &mut self,
        open_id: String,
        name: String,
        is_ai: bool,
        persona: Option<Persona>,
    ) -> Result<()> {
        if !matches!(self.stage, Stage::Lobby | Stage::Ended) {
            return Err(anyhow!("一局狼人杀正在进行中，等结束后再加入"));
        }
        if self.players.iter().any(|p| p.open_id == open_id) {
            return Err(anyhow!("你已经在桌上了"));
        }
        if self.players.len() >= 12 {
            return Err(anyhow!("人数已满 (12)"));
        }
        // 重新进入 Lobby 时把 stage 抹掉，避免上局结束态把按钮锁住
        if matches!(self.stage, Stage::Ended) {
            self.stage = Stage::Lobby;
        }
        self.players.push(Player {
            open_id,
            name,
            role: None,
            alive: true,
            is_ai,
            persona,
        });
        Ok(())
    }

    pub fn find_player(&self, open_id: &str) -> Option<usize> {
        self.players.iter().position(|p| p.open_id == open_id)
    }

    pub fn alive_indices(&self) -> Vec<usize> {
        self.players
            .iter()
            .enumerate()
            .filter(|(_, p)| p.alive)
            .map(|(i, _)| i)
            .collect()
    }

    /// 所有存活的狼阵营（普通狼 + 狼王）。
    pub fn alive_wolves(&self) -> Vec<usize> {
        self.players
            .iter()
            .enumerate()
            .filter(|(_, p)| p.alive && p.role.map(|r| r.is_wolf()).unwrap_or(false))
            .map(|(i, _)| i)
            .collect()
    }

    pub fn alive_count(&self) -> usize {
        self.players.iter().filter(|p| p.alive).count()
    }

    pub fn alive_wolf_count(&self) -> usize {
        self.alive_wolves().len()
    }

    pub fn alive_good_count(&self) -> usize {
        self.players
            .iter()
            .filter(|p| p.alive && p.role.map(|r| !r.is_wolf()).unwrap_or(false))
            .count()
    }

    pub fn role_idx(&self, role: Role) -> Option<usize> {
        self.players.iter().position(|p| p.role == Some(role))
    }

    pub fn is_witch(&self, idx: usize) -> bool {
        self.players.get(idx).and_then(|p| p.role) == Some(Role::Witch)
    }

    pub fn is_seer(&self, idx: usize) -> bool {
        self.players.get(idx).and_then(|p| p.role) == Some(Role::Seer)
    }

    pub fn is_wolf(&self, idx: usize) -> bool {
        self.players.get(idx).and_then(|p| p.role) == Some(Role::Werewolf)
    }

    pub fn is_guard(&self, idx: usize) -> bool {
        self.players.get(idx).and_then(|p| p.role) == Some(Role::Guard)
    }

    /// 开局：随机分配角色，进入第 1 夜。
    pub fn start_game(&mut self) -> Result<()> {
        if !matches!(self.stage, Stage::Lobby | Stage::Ended) {
            return Err(anyhow!(
                "已经在游戏中（{}）。如需重置请用 reset",
                self.stage.label()
            ));
        }
        let n = self.players.len();
        if !(9..=12).contains(&n) {
            return Err(anyhow!("狼人杀需要 9-12 名玩家，当前 {}", n));
        }
        let mut roles = role_distribution(n)?;
        roles.shuffle(&mut rand::thread_rng());

        for (p, role) in self.players.iter_mut().zip(roles.iter()) {
            p.role = Some(*role);
            p.alive = true;
        }

        // 重置一切局面状态。
        self.day = 1;
        self.game_count += 1;
        self.wolf_kill_votes.clear();
        self.wolf_chat.clear();
        self.wolf_ready.clear();
        self.wolf_night_msgs.clear();
        self.seer_check_target = None;
        self.witch_save_choice = None;
        self.witch_poison_target = None;
        self.guard_target = None;
        self.night_victim = None;
        self.witch_acted = false;
        self.witch_save_used = false;
        self.witch_poison_used = false;
        self.seer_history.clear();
        self.last_guard_target = None;
        self.day_votes.clear();
        self.last_night_deaths.clear();
        self.last_day_lynched = None;
        self.day_speech_order.clear();
        self.day_speech_idx = 0;
        self.day_speeches.clear();
        self.day_speech_public_msg = None;
        self.day_speech_private_msg = None;
        self.sheriff_idx = None;
        self.sheriff_enabled = has_sheriff_election(n);
        self.sheriff_nominations.clear();
        self.sheriff_votes.clear();
        self.sheriff_speech_order.clear();
        self.sheriff_speech_idx = 0;
        self.sheriff_speeches.clear();
        self.sheriff_speech_public_msg = None;
        self.sheriff_speech_private_msg = None;
        self.last_words_queue.clear();
        self.last_words_idx = 0;
        self.last_words_speeches.clear();
        self.last_words_public_msg = None;
        self.last_words_private_msg = None;
        self.last_words_post_stage = None;
        self.pending_badge = None;
        self.pending_badge_post_stage = None;
        self.pending_hunter = None;
        self.pending_hunter_post_stage = None;
        self.pending_hunter_ai_decision = None;
        self.event_log.clear();
        self.deaths.clear();
        self.recap_log.clear();
        self.thinking_log.clear();

        // 第一夜起点：有守卫先守卫，否则狼。
        self.stage = if self.role_idx(Role::Guard).is_some() {
            Stage::GuardPick
        } else {
            Stage::WolvesPick
        };

        self.event_log
            .push(format!("第 1 夜开始，{} 名玩家入夜。", n));

        Ok(())
    }

    /// 守卫选择守护目标（含自己）。规则：不能连续两晚守同一个人。
    pub fn guard_pick(&mut self, guard_open_id: &str, target_open_id: &str) -> Result<()> {
        if self.stage != Stage::GuardPick {
            return Err(anyhow!("当前不是守卫阶段"));
        }
        let g_idx = self
            .find_player(guard_open_id)
            .ok_or_else(|| anyhow!("你不在桌上"))?;
        if !self.is_guard(g_idx) || !self.players[g_idx].alive {
            return Err(anyhow!("你不是存活的守卫"));
        }
        let t_idx = self
            .find_player(target_open_id)
            .ok_or_else(|| anyhow!("目标玩家不存在"))?;
        if !self.players[t_idx].alive {
            return Err(anyhow!("不能守护已死亡的玩家"));
        }
        if self.last_guard_target == Some(t_idx) {
            return Err(anyhow!("不能连续两晚守护同一个人"));
        }
        self.guard_target = Some(t_idx);
        self.last_guard_target = Some(t_idx);
        self.recap_log.push(RecapEvent::GuardProtect { day: self.day, target: t_idx });
        // 守卫完成 → 进入狼人阶段
        self.stage = Stage::WolvesPick;
        Ok(())
    }

    /// 狼人提交击杀目标。提交后若所有狼人都已投票，则计算最终目标并进入下一阶段。
    pub fn wolf_pick(&mut self, wolf_open_id: &str, target_open_id: &str) -> Result<()> {
        if self.stage != Stage::WolvesPick {
            return Err(anyhow!("当前不是狼人投票阶段"));
        }
        let wolf_idx = self
            .find_player(wolf_open_id)
            .ok_or_else(|| anyhow!("你不在桌上"))?;
        if !self.is_wolf(wolf_idx) || !self.players[wolf_idx].alive {
            return Err(anyhow!("你不是存活的狼人"));
        }
        let target_idx = self
            .find_player(target_open_id)
            .ok_or_else(|| anyhow!("目标玩家不存在"))?;
        if !self.players[target_idx].alive {
            return Err(anyhow!("不能选择已死亡的玩家"));
        }
        if self.is_wolf(target_idx) {
            return Err(anyhow!("不能选择狼队友"));
        }
        // 替换或追加这只狼的投票
        if let Some(slot) = self
            .wolf_kill_votes
            .iter_mut()
            .find(|(w, _)| *w == wolf_idx)
        {
            slot.1 = target_idx;
        } else {
            self.wolf_kill_votes.push((wolf_idx, target_idx));
        }
        Ok(())
    }

    #[allow(dead_code)] // 测试用 + 公开 API
    pub fn all_wolves_voted(&self) -> bool {
        let alive_wolves = self.alive_wolves();
        alive_wolves
            .iter()
            .all(|w| self.wolf_kill_votes.iter().any(|(v, _)| v == w))
    }

    /// 狼人在夜间聊天里发言（仅在 WolvesPick 阶段）。
    pub fn wolf_say(&mut self, wolf_open_id: &str, message: String) -> Result<()> {
        if self.stage != Stage::WolvesPick {
            return Err(anyhow!("当前不是狼人阶段"));
        }
        let idx = self
            .find_player(wolf_open_id)
            .ok_or_else(|| anyhow!("你不在桌上"))?;
        if !self.is_wolf(idx) || !self.players[idx].alive {
            return Err(anyhow!("你不是存活的狼人"));
        }
        let trimmed = message.trim();
        if trimmed.is_empty() {
            return Err(anyhow!("消息为空"));
        }
        self.wolf_chat.push((idx, trimmed.to_string()));
        Ok(())
    }

    /// 狼人点击"我决定了"——必须先选过目标。
    pub fn wolf_mark_ready(&mut self, wolf_open_id: &str) -> Result<()> {
        if self.stage != Stage::WolvesPick {
            return Err(anyhow!("当前不是狼人阶段"));
        }
        let idx = self
            .find_player(wolf_open_id)
            .ok_or_else(|| anyhow!("你不在桌上"))?;
        if !self.is_wolf(idx) || !self.players[idx].alive {
            return Err(anyhow!("你不是存活的狼人"));
        }
        if !self.wolf_kill_votes.iter().any(|(w, _)| *w == idx) {
            return Err(anyhow!("请先选目标再确认"));
        }
        if !self.wolf_ready.contains(&idx) {
            self.wolf_ready.push(idx);
        }
        Ok(())
    }

    pub fn is_wolf_ready(&self, idx: usize) -> bool {
        self.wolf_ready.contains(&idx)
    }

    /// 所有存活狼是否都点了"我决定了"。
    pub fn all_alive_wolves_ready(&self) -> bool {
        let alive = self.alive_wolves();
        !alive.is_empty() && alive.iter().all(|w| self.wolf_ready.contains(w))
    }

    /// 记录某只狼的当晚行动卡 message_id，下次 update_card 复用。
    pub fn set_wolf_night_msg(&mut self, wolf_idx: usize, msg_id: String) {
        if let Some(slot) = self.wolf_night_msgs.iter_mut().find(|(w, _)| *w == wolf_idx) {
            slot.1 = msg_id;
        } else {
            self.wolf_night_msgs.push((wolf_idx, msg_id));
        }
    }

    pub fn wolf_night_msg(&self, wolf_idx: usize) -> Option<&str> {
        self.wolf_night_msgs
            .iter()
            .find(|(w, _)| *w == wolf_idx)
            .map(|(_, id)| id.as_str())
    }

    /// 解析狼人投票得出最终猎物：取最高票，平票随机。
    fn resolve_wolf_kill(&mut self) {
        if self.wolf_kill_votes.is_empty() {
            self.night_victim = None;
            return;
        }
        let mut tally: Vec<(usize, u32)> = vec![];
        for (_, target) in &self.wolf_kill_votes {
            if let Some(slot) = tally.iter_mut().find(|(t, _)| t == target) {
                slot.1 += 1;
            } else {
                tally.push((*target, 1));
            }
        }
        let max = tally.iter().map(|(_, c)| *c).max().unwrap_or(0);
        let candidates: Vec<usize> = tally
            .iter()
            .filter(|(_, c)| *c == max)
            .map(|(t, _)| *t)
            .collect();
        let chosen = *candidates
            .choose(&mut rand::thread_rng())
            .expect("non-empty candidates after non-empty votes");
        self.night_victim = Some(chosen);
    }

    /// 狼阶段全部结束后调用：解析猎物，进入下一阶段（预言家 / 女巫 / 白天）。
    pub fn advance_after_wolves(&mut self) -> Result<()> {
        if self.stage != Stage::WolvesPick {
            return Err(anyhow!("当前不是狼人阶段"));
        }
        self.resolve_wolf_kill();
        self.recap_log.push(RecapEvent::WolfFinalTarget {
            day: self.day,
            target: self.night_victim,
        });
        // 进入下一阶段
        self.next_night_stage_after(Stage::WolvesPick);
        Ok(())
    }

    /// 预言家查验。
    pub fn seer_check(&mut self, seer_open_id: &str, target_open_id: &str) -> Result<bool> {
        if self.stage != Stage::SeerPick {
            return Err(anyhow!("当前不是预言家阶段"));
        }
        let idx = self
            .find_player(seer_open_id)
            .ok_or_else(|| anyhow!("你不在桌上"))?;
        if !self.is_seer(idx) || !self.players[idx].alive {
            return Err(anyhow!("你不是存活的预言家"));
        }
        let t_idx = self
            .find_player(target_open_id)
            .ok_or_else(|| anyhow!("目标玩家不存在"))?;
        if !self.players[t_idx].alive {
            return Err(anyhow!("不能查验已死亡的玩家"));
        }
        if t_idx == idx {
            return Err(anyhow!("不能查验自己"));
        }
        let is_wolf = self.is_wolf(t_idx);
        self.seer_check_target = Some(t_idx);
        self.seer_history.push(SeerCheck {
            day: self.day,
            target_idx: t_idx,
            is_wolf,
        });
        self.recap_log.push(RecapEvent::SeerCheck {
            day: self.day,
            target: t_idx,
            is_wolf,
        });
        // 进入下一阶段（女巫或白天）
        self.next_night_stage_after(Stage::SeerPick);
        Ok(is_wolf)
    }

    /// 女巫的三种行动：救人 / 毒人 / 跳过。同晚救+毒互斥。
    pub fn witch_act(
        &mut self,
        witch_open_id: &str,
        save_victim: bool,
        poison_target_open_id: Option<&str>,
    ) -> Result<()> {
        if self.stage != Stage::WitchAct {
            return Err(anyhow!("当前不是女巫阶段"));
        }
        let idx = self
            .find_player(witch_open_id)
            .ok_or_else(|| anyhow!("你不在桌上"))?;
        if !self.is_witch(idx) || !self.players[idx].alive {
            return Err(anyhow!("你不是存活的女巫"));
        }
        if save_victim && poison_target_open_id.is_some() {
            return Err(anyhow!("同一晚不能同时使用救药和毒药"));
        }
        if save_victim && self.witch_save_used {
            return Err(anyhow!("救药已经用过了"));
        }
        if save_victim && self.night_victim.is_none() {
            return Err(anyhow!("今晚是空刀，没人需要救"));
        }
        let mut poison_idx = None;
        if let Some(target_id) = poison_target_open_id {
            if self.witch_poison_used {
                return Err(anyhow!("毒药已经用过了"));
            }
            let t_idx = self
                .find_player(target_id)
                .ok_or_else(|| anyhow!("毒药目标不存在"))?;
            if !self.players[t_idx].alive {
                return Err(anyhow!("不能毒已死亡的玩家"));
            }
            if t_idx == idx {
                return Err(anyhow!("不能毒自己"));
            }
            poison_idx = Some(t_idx);
        }
        self.witch_save_choice = Some(save_victim);
        self.witch_poison_target = poison_idx;
        self.witch_acted = true;
        self.recap_log.push(RecapEvent::Witch {
            day: self.day,
            save: save_victim,
            poison: poison_idx,
        });
        self.next_night_stage_after(Stage::WitchAct);
        Ok(())
    }

    /// 计算夜晚最终死亡者，进入白天阶段。如有猎人死于狼刀，先进入 HunterShoot。
    fn resolve_night(&mut self) {
        let mut deaths: Vec<(usize, DeathCause)> = vec![];

        // 狼刀：守卫守护 / 女巫救人 / 同守同救（双重保护反而死亡）
        if let Some(victim) = self.night_victim {
            let saved = self.witch_save_choice.unwrap_or(false);
            let guarded = self.guard_target == Some(victim);
            // 同守同救规则：被守 + 被救 → 双重保护抵消，依然死亡（标准毒杀类死法）
            let dies = if saved && guarded {
                true
            } else if saved || guarded {
                false
            } else {
                true
            };
            if dies {
                deaths.push((victim, DeathCause::WolfKill));
            }
        }
        // 女巫毒杀（无视守卫；毒杀也不能被守卫挡住）
        if let Some(t) = self.witch_poison_target {
            if !deaths.iter().any(|(i, _)| *i == t) {
                deaths.push((t, DeathCause::Poison));
            }
        }
        // 标记救药/毒药消耗
        if self.witch_save_choice.unwrap_or(false) {
            self.witch_save_used = true;
        }
        if self.witch_poison_target.is_some() {
            self.witch_poison_used = true;
        }

        // 写入日志 & 应用死亡
        self.last_night_deaths.clear();
        for (idx, cause) in &deaths {
            self.players[*idx].alive = false;
            self.last_night_deaths.push(*idx);
            self.deaths.push(DeathEvent {
                day: self.day,
                night: true,
                player_idx: *idx,
                cause: *cause,
            });
            self.recap_log.push(RecapEvent::Death {
                day: self.day,
                night: true,
                player: *idx,
                cause: *cause,
            });
            // 公告中性化：不暴露 狼刀 vs 毒杀 的差别——这是黑信息，
            // 只有动手的角色（狼 / 女巫）通过自己的内部状态知道真因。
            self.event_log.push(format!(
                "第 {} 夜：{} 死亡",
                self.day,
                self.players[*idx].name,
            ));
        }

        // 设置 pending（不立即切阶段）：先走遗言再触发技能链
        // 1. 猎人 / 狼王 死于狼刀 → 准备开枪（毒杀不触发）
        let shooter_just_died = deaths.iter().find(|(i, c)| {
            let role = self.players[*i].role;
            (role == Some(Role::Hunter) || role == Some(Role::WolfKing))
                && *c == DeathCause::WolfKill
        });
        if let Some((h_idx, _)) = shooter_just_died {
            self.pending_hunter = Some(*h_idx);
            self.pending_hunter_post_stage = Some(Stage::DayReveal);
        }
        // 2. 警长死亡（任何死法）→ 警徽流转
        let sheriff_just_died = deaths
            .iter()
            .find(|(i, _)| Some(*i) == self.sheriff_idx);
        if let Some((s_idx, _)) = sheriff_just_died {
            self.pending_badge = Some(*s_idx);
            self.pending_badge_post_stage = Some(Stage::DayReveal);
        }

        // 3. 路由：夜里死的人**不发表遗言**（全部静默）——遗言只发生在白天放逐。
        //    例外：猎人/狼王 单独走 HunterShoot 阶段，由 dying_hunter_combined
        //    一次性输出『临死一句话 + 开枪目标』，那一句话同时充当临终信号；
        //    被开枪带走者本身也无遗言（与放逐不同）。
        if self.pending_hunter.is_some() {
            self.stage = Stage::HunterShoot;
        } else if self.pending_badge.is_some() {
            self.stage = Stage::BadgePass;
        } else {
            self.stage = Stage::DayReveal;
        }
    }

    /// 选择夜晚下一阶段。当前阶段已结束，根据剩余角色决定去哪。
    fn next_night_stage_after(&mut self, just_finished: Stage) {
        let seer_alive = self
            .role_idx(Role::Seer)
            .map(|i| self.players[i].alive)
            .unwrap_or(false);
        let witch_alive = self
            .role_idx(Role::Witch)
            .map(|i| self.players[i].alive)
            .unwrap_or(false);

        let next = match just_finished {
            Stage::WolvesPick => {
                if seer_alive {
                    Stage::SeerPick
                } else if witch_alive {
                    Stage::WitchAct
                } else {
                    // 没有神职，直接结算夜晚
                    self.resolve_night();
                    return;
                }
            }
            Stage::SeerPick => {
                if witch_alive {
                    Stage::WitchAct
                } else {
                    self.resolve_night();
                    return;
                }
            }
            Stage::WitchAct => {
                self.resolve_night();
                return;
            }
            _ => return,
        };
        self.stage = next;
    }

    /// 从 DayReveal 推进。第 1 天且启用警长 → 上警阶段；否则直接发言。
    pub fn enter_day_discuss(&mut self) -> Result<()> {
        if self.stage != Stage::DayReveal {
            return Err(anyhow!("当前不是 DayReveal 阶段"));
        }
        if let Some(_winner) = self.victory() {
            self.stage = Stage::Ended;
            return Ok(());
        }
        if self.day == 1 && self.sheriff_enabled && self.sheriff_idx.is_none() {
            self.stage = Stage::SheriffNominate;
            self.sheriff_nominations.clear();
            return Ok(());
        }
        self.start_day_speech();
        Ok(())
    }

    /// 进入白天发言。
    /// - 警长存活 → SheriffPickDirection（警长选警上 / 警下）
    /// - 没警长 → 随机起点 + 顺时针，直接 DaySpeech
    fn start_day_speech(&mut self) {
        let n = self.players.len();
        if let Some(_) = self.sheriff_idx.filter(|s| self.players[*s].alive) {
            self.stage = Stage::SheriffPickDirection;
            return;
        }
        // 无警长：随机起点
        use rand::seq::SliceRandom;
        let alive: Vec<usize> = self.alive_indices();
        let start = *alive.choose(&mut rand::thread_rng()).unwrap_or(&0);
        let mut order: Vec<usize> = vec![];
        for k in 0..n {
            let idx = (start + k) % n;
            if self.players[idx].alive {
                order.push(idx);
            }
        }
        self.day_speech_order = order;
        self.day_speech_idx = 0;
        self.day_speeches.clear();
        self.day_speech_public_msg = None;
        self.day_speech_private_msg = None;
        self.stage = Stage::DaySpeech;
    }

    /// 警长选择白天发言方向。`clockwise = true` → 警上（警长右手起，顺时针）；
    /// `false` → 警下（警长左手起，逆时针）。**警长本人末位归票**。
    pub fn pick_sheriff_direction(
        &mut self,
        sheriff_open_id: &str,
        clockwise: bool,
    ) -> Result<()> {
        if self.stage != Stage::SheriffPickDirection {
            return Err(anyhow!("当前不是选择方向阶段"));
        }
        let idx = self
            .find_player(sheriff_open_id)
            .ok_or_else(|| anyhow!("你不在桌上"))?;
        if Some(idx) != self.sheriff_idx {
            return Err(anyhow!("不是你来选"));
        }
        let n = self.players.len();
        let mut order: Vec<usize> = vec![];
        for k in 1..n {
            let target = if clockwise {
                (idx + k) % n
            } else {
                (idx + n - k) % n
            };
            if target == idx {
                continue;
            }
            if self.players[target].alive {
                order.push(target);
            }
        }
        // 警长末位归票
        order.push(idx);
        self.day_speech_order = order;
        self.day_speech_idx = 0;
        self.day_speeches.clear();
        self.day_speech_public_msg = None;
        self.day_speech_private_msg = None;
        self.event_log.push(format!(
            "警长 {} 选择 {} 起手",
            self.players[idx].name,
            if clockwise { "警上 (顺时针)" } else { "警下 (逆时针)" }
        ));
        self.recap_log.push(RecapEvent::SheriffDirection { clockwise });
        self.stage = Stage::DaySpeech;
        Ok(())
    }

    /// 当前发言人。None = 已发完。
    pub fn current_day_speaker(&self) -> Option<usize> {
        self.day_speech_order.get(self.day_speech_idx).copied()
    }

    /// 提交白天发言（或弃权时传空字符串）。
    pub fn submit_day_speech(&mut self, speaker_open_id: &str, text: String) -> Result<()> {
        if self.stage != Stage::DaySpeech {
            return Err(anyhow!("当前不是白天发言阶段"));
        }
        let idx = self
            .find_player(speaker_open_id)
            .ok_or_else(|| anyhow!("你不在桌上"))?;
        let expected = self
            .current_day_speaker()
            .ok_or_else(|| anyhow!("发言已结束"))?;
        if idx != expected {
            return Err(anyhow!("还没轮到你发言"));
        }
        let trimmed = text.trim();
        let display = if trimmed.is_empty() {
            "(沉默)".to_string()
        } else {
            trimmed.to_string()
        };
        self.event_log.push(format!(
            "第 {} 天 · {} 发言：{}",
            self.day, self.players[idx].name, display
        ));
        self.recap_log.push(RecapEvent::DaySpeech {
            day: self.day,
            player: idx,
            text: display.clone(),
        });
        self.day_speeches.push((idx, display));
        self.day_speech_idx += 1;
        // 切换发言人 → 旧的私发卡作废（caller 应避免错发）
        self.day_speech_private_msg = None;
        Ok(())
    }

    /// 全员发言完毕？
    #[allow(dead_code)] // 测试 + 公共 API
    pub fn all_day_speeches_done(&self) -> bool {
        self.day_speech_idx >= self.day_speech_order.len()
    }

    /// 上警阶段：玩家提交是否参选警长。
    pub fn nominate_sheriff(&mut self, voter_open_id: &str, running: bool) -> Result<()> {
        if self.stage != Stage::SheriffNominate {
            return Err(anyhow!("当前不是上警阶段"));
        }
        let idx = self
            .find_player(voter_open_id)
            .ok_or_else(|| anyhow!("你不在桌上"))?;
        if !self.players[idx].alive {
            return Err(anyhow!("死人不能上警"));
        }
        if let Some(slot) = self.sheriff_nominations.iter_mut().find(|(i, _)| *i == idx) {
            slot.1 = running;
        } else {
            self.sheriff_nominations.push((idx, running));
        }
        Ok(())
    }

    pub fn all_alive_nominated(&self) -> bool {
        self.alive_indices().iter().all(|i| {
            self.sheriff_nominations.iter().any(|(idx, _)| idx == i)
        })
    }

    pub fn sheriff_candidates(&self) -> Vec<usize> {
        self.sheriff_nominations
            .iter()
            .filter(|(_, run)| *run)
            .map(|(i, _)| *i)
            .collect()
    }

    /// 上警阶段结束。
    /// - ≥ 2 名候选 → 进竞选发言（SheriffSpeech）
    /// - 1 名 → 直接当选 → 白天发言
    /// - 0 名 → 无警长 → 白天发言
    pub fn finish_sheriff_nominate(&mut self) -> Result<()> {
        if self.stage != Stage::SheriffNominate {
            return Err(anyhow!("当前不是上警阶段"));
        }
        let candidates = self.sheriff_candidates();
        self.recap_log.push(RecapEvent::SheriffCandidates {
            candidates: candidates.clone(),
        });
        match candidates.len() {
            0 => {
                self.event_log.push("无人上警，本局无警长。".to_string());
                self.recap_log.push(RecapEvent::SheriffElected { player: None });
                self.start_day_speech();
            }
            1 => {
                let only = candidates[0];
                self.sheriff_idx = Some(only);
                self.event_log.push(format!(
                    "{} 唯一上警，自动当选警长。",
                    self.players[only].name
                ));
                self.recap_log.push(RecapEvent::SheriffElected { player: Some(only) });
                self.start_day_speech();
            }
            _ => {
                // 多候选 → 竞选发言
                self.sheriff_speech_order = candidates;
                self.sheriff_speech_idx = 0;
                self.sheriff_speeches.clear();
                self.sheriff_speech_public_msg = None;
                self.sheriff_speech_private_msg = None;
                self.stage = Stage::SheriffSpeech;
            }
        }
        Ok(())
    }

    /// 当前竞选发言候选人。None = 发完了。
    pub fn current_sheriff_speaker(&self) -> Option<usize> {
        self.sheriff_speech_order.get(self.sheriff_speech_idx).copied()
    }

    pub fn submit_sheriff_speech(&mut self, speaker_open_id: &str, text: String) -> Result<()> {
        if self.stage != Stage::SheriffSpeech {
            return Err(anyhow!("当前不是上警发言阶段"));
        }
        let idx = self
            .find_player(speaker_open_id)
            .ok_or_else(|| anyhow!("你不在桌上"))?;
        let expected = self
            .current_sheriff_speaker()
            .ok_or_else(|| anyhow!("发言已结束"))?;
        if idx != expected {
            return Err(anyhow!("还没轮到你发言"));
        }
        let trimmed = text.trim();
        let display = if trimmed.is_empty() {
            "(沉默)".to_string()
        } else {
            trimmed.to_string()
        };
        self.event_log.push(format!(
            "上警发言 · {}：{}",
            self.players[idx].name, display
        ));
        self.recap_log.push(RecapEvent::SheriffSpeech {
            player: idx,
            text: display.clone(),
        });
        self.sheriff_speeches.push((idx, display));
        self.sheriff_speech_idx += 1;
        self.sheriff_speech_private_msg = None;
        Ok(())
    }

    pub fn all_sheriff_speeches_done(&self) -> bool {
        self.sheriff_speech_idx >= self.sheriff_speech_order.len()
    }

    /// 上警发言全部说完 → 直接进入警长投票。
    pub fn finish_sheriff_speeches(&mut self) -> Result<()> {
        if self.stage != Stage::SheriffSpeech {
            return Err(anyhow!("当前不是上警发言阶段"));
        }
        if !self.all_sheriff_speeches_done() {
            return Err(anyhow!("还有候选人没发言"));
        }
        self.stage = Stage::SheriffVote;
        self.sheriff_votes.clear();
        Ok(())
    }

    // ---- 死亡遗言 ----

    pub fn current_last_words_speaker(&self) -> Option<usize> {
        self.last_words_queue.get(self.last_words_idx).copied()
    }

    pub fn submit_last_words(&mut self, speaker_open_id: &str, text: String) -> Result<()> {
        if self.stage != Stage::LastWords {
            return Err(anyhow!("当前不是遗言阶段"));
        }
        let idx = self
            .find_player(speaker_open_id)
            .ok_or_else(|| anyhow!("你不在桌上"))?;
        let expected = self
            .current_last_words_speaker()
            .ok_or_else(|| anyhow!("遗言已结束"))?;
        if idx != expected {
            return Err(anyhow!("还没轮到你说遗言"));
        }
        let trimmed = text.trim();
        let display = if trimmed.is_empty() {
            "(沉默)".to_string()
        } else {
            trimmed.to_string()
        };
        self.event_log.push(format!(
            "{} 的遗言：{}",
            self.players[idx].name, display
        ));
        // 区分夜晚遗言（在 LastWords post=DayReveal 时）vs 放逐遗言（post=DayVote）
        let night = matches!(self.last_words_post_stage, Some(Stage::DayReveal));
        self.recap_log.push(RecapEvent::LastWords {
            day: self.day,
            night,
            player: idx,
            text: display.clone(),
        });
        self.last_words_speeches.push((idx, display));
        self.last_words_idx += 1;
        self.last_words_private_msg = None;
        Ok(())
    }

    #[allow(dead_code)]
    pub fn all_last_words_done(&self) -> bool {
        self.last_words_idx >= self.last_words_queue.len()
    }

    /// 遗言全部说完后调用：触发待处理的开枪 / 警徽流转，否则回到 post_stage。
    pub fn finish_last_words(&mut self) -> Result<()> {
        if self.stage != Stage::LastWords {
            return Err(anyhow!("当前不是遗言阶段"));
        }
        if self.last_words_idx < self.last_words_queue.len() {
            return Err(anyhow!("还有人没说遗言"));
        }
        if self.pending_hunter.is_some() {
            self.stage = Stage::HunterShoot;
        } else if self.pending_badge.is_some() {
            self.stage = Stage::BadgePass;
        } else {
            let post = self.last_words_post_stage.take();
            self.continue_after_special_stage(post);
        }
        Ok(())
    }

    /// 上警投票（候选人不投）。target 必须是候选人之一，None = 弃权。
    pub fn cast_sheriff_vote(
        &mut self,
        voter_open_id: &str,
        target_open_id: Option<&str>,
    ) -> Result<()> {
        if self.stage != Stage::SheriffVote {
            return Err(anyhow!("当前不是警长投票阶段"));
        }
        let v_idx = self
            .find_player(voter_open_id)
            .ok_or_else(|| anyhow!("你不在桌上"))?;
        if !self.players[v_idx].alive {
            return Err(anyhow!("死人不能投票"));
        }
        // 候选人不能投票
        if self
            .sheriff_nominations
            .iter()
            .any(|(i, run)| *i == v_idx && *run)
        {
            return Err(anyhow!("候选人不能参与警长投票"));
        }
        let t_idx = if let Some(t) = target_open_id {
            let idx = self
                .find_player(t)
                .ok_or_else(|| anyhow!("目标玩家不存在"))?;
            if !self
                .sheriff_nominations
                .iter()
                .any(|(i, run)| *i == idx && *run)
            {
                return Err(anyhow!("目标不是警长候选人"));
            }
            Some(idx)
        } else {
            None
        };
        self.sheriff_votes.cast(v_idx, t_idx);
        Ok(())
    }

    pub fn all_sheriff_voters_cast(&self) -> bool {
        // 投票人 = 存活的非候选人
        let candidates = self.sheriff_candidates();
        let alive: Vec<usize> = self.alive_indices();
        alive
            .iter()
            .filter(|i| !candidates.contains(i))
            .all(|i| self.sheriff_votes.for_voter(*i).is_some())
    }

    /// 解析警长投票，确定警长人选（平票 = 无警长）。
    pub fn resolve_sheriff_vote(&mut self) -> Result<Option<usize>> {
        if self.stage != Stage::SheriffVote {
            return Err(anyhow!("当前不是警长投票阶段"));
        }
        let mut tally: Vec<(usize, u32)> = vec![];
        for (_, target) in &self.sheriff_votes.votes {
            if let Some(t) = target {
                if let Some(slot) = tally.iter_mut().find(|(i, _)| i == t) {
                    slot.1 += 1;
                } else {
                    tally.push((*t, 1));
                }
            }
        }
        if tally.is_empty() {
            self.event_log
                .push("警长投票全员弃权，本局无警长。".to_string());
            self.recap_log.push(RecapEvent::SheriffElected { player: None });
            self.start_day_speech();
            return Ok(None);
        }
        let max = tally.iter().map(|(_, c)| *c).max().unwrap_or(0);
        let top: Vec<usize> = tally
            .iter()
            .filter(|(_, c)| *c == max)
            .map(|(i, _)| *i)
            .collect();
        if top.len() > 1 {
            self.event_log
                .push("警长投票平票，本局无警长。".to_string());
            self.recap_log.push(RecapEvent::SheriffElected { player: None });
            self.start_day_speech();
            return Ok(None);
        }
        let elected = top[0];
        self.sheriff_idx = Some(elected);
        self.event_log
            .push(format!("{} 当选警长。", self.players[elected].name));
        self.recap_log.push(RecapEvent::SheriffElected { player: Some(elected) });
        self.start_day_speech();
        Ok(Some(elected))
    }

    /// 警长投票权重：警长 = 3，普通玩家 = 2（比值 1.5）。
    fn vote_weight(&self, voter_idx: usize) -> u32 {
        if Some(voter_idx) == self.sheriff_idx {
            3
        } else {
            2
        }
    }

    /// 警徽流转：警长死亡时调用。target = 接班人 idx，None = 撕毁警徽。
    pub fn transfer_badge(
        &mut self,
        sheriff_open_id: &str,
        target_open_id: Option<&str>,
    ) -> Result<Option<usize>> {
        if self.stage != Stage::BadgePass {
            return Err(anyhow!("当前不是警徽流转阶段"));
        }
        let s_idx = self
            .find_player(sheriff_open_id)
            .ok_or_else(|| anyhow!("你不在桌上"))?;
        if Some(s_idx) != self.pending_badge {
            return Err(anyhow!("不是你来移交警徽"));
        }
        let new_holder = if let Some(id) = target_open_id {
            let t = self
                .find_player(id)
                .ok_or_else(|| anyhow!("目标玩家不存在"))?;
            if !self.players[t].alive {
                return Err(anyhow!("不能给死人警徽"));
            }
            if t == s_idx {
                return Err(anyhow!("不能给自己"));
            }
            self.sheriff_idx = Some(t);
            self.event_log.push(format!(
                "警徽从 {} 移交给 {}",
                self.players[s_idx].name, self.players[t].name
            ));
            Some(t)
        } else {
            // 撕毁
            self.sheriff_idx = None;
            self.event_log
                .push(format!("{} 撕毁警徽", self.players[s_idx].name));
            None
        };
        self.recap_log.push(RecapEvent::BadgePass {
            day: self.day,
            from: s_idx,
            to: new_holder,
        });
        // 移交完毕，回到 pending_badge_post_stage 指定的阶段
        let next = self.pending_badge_post_stage.take();
        self.pending_badge = None;
        self.continue_after_special_stage(next);
        Ok(new_holder)
    }

    /// 通用：HunterShoot / BadgePass 完成后的下一站。
    fn continue_after_special_stage(&mut self, requested_post: Option<Stage>) {
        match requested_post {
            Some(Stage::DayReveal) => {
                self.stage = Stage::DayReveal;
            }
            Some(Stage::DayVote) | _ => {
                self.advance_to_next_night_or_end();
            }
        }
    }

    /// 标记发言结束，开始投票。
    pub fn enter_day_vote(&mut self) -> Result<()> {
        if self.stage != Stage::DaySpeech {
            return Err(anyhow!("当前不是发言阶段"));
        }
        self.stage = Stage::DayVote;
        self.day_votes.clear();
        Ok(())
    }

    /// 一名玩家投票。target_open_id = None 表示弃权。
    pub fn cast_vote(
        &mut self,
        voter_open_id: &str,
        target_open_id: Option<&str>,
    ) -> Result<()> {
        if self.stage != Stage::DayVote {
            return Err(anyhow!("当前不是投票阶段"));
        }
        let v_idx = self
            .find_player(voter_open_id)
            .ok_or_else(|| anyhow!("你不在桌上"))?;
        if !self.players[v_idx].alive {
            return Err(anyhow!("死人不能投票"));
        }
        let t_idx = if let Some(id) = target_open_id {
            let t = self
                .find_player(id)
                .ok_or_else(|| anyhow!("目标玩家不存在"))?;
            if !self.players[t].alive {
                return Err(anyhow!("不能投死人"));
            }
            if t == v_idx {
                return Err(anyhow!("不能投自己"));
            }
            Some(t)
        } else {
            None
        };
        self.day_votes.cast(v_idx, t_idx);
        let weight = self.vote_weight(v_idx);
        self.recap_log.push(RecapEvent::DayVoteCast {
            day: self.day,
            voter: v_idx,
            target: t_idx,
            weight,
        });
        Ok(())
    }

    pub fn all_alive_voted(&self) -> bool {
        let alive: Vec<usize> = self.alive_indices();
        alive
            .iter()
            .all(|i| self.day_votes.for_voter(*i).is_some())
    }

    /// 计算放逐结果（警长 1.5 倍票权 = 整数 3 vs 普通 2）。
    pub fn resolve_lynch(&mut self) -> Result<Option<usize>> {
        if self.stage != Stage::DayVote {
            return Err(anyhow!("当前不是投票阶段"));
        }
        let mut tally: Vec<(usize, u32)> = vec![];
        for (voter, target) in &self.day_votes.votes {
            if let Some(t) = target {
                let w = self.vote_weight(*voter);
                if let Some(slot) = tally.iter_mut().find(|(idx, _)| idx == t) {
                    slot.1 += w;
                } else {
                    tally.push((*t, w));
                }
            }
        }
        if tally.is_empty() {
            self.last_day_lynched = None;
            self.event_log
                .push(format!("第 {} 天：全员弃权，无人放逐。", self.day));
            self.recap_log.push(RecapEvent::DayLynch { day: self.day, target: None });
            return Ok(None);
        }
        let max = tally.iter().map(|(_, c)| *c).max().unwrap_or(0);
        let top: Vec<usize> = tally
            .iter()
            .filter(|(_, c)| *c == max)
            .map(|(i, _)| *i)
            .collect();
        if top.len() > 1 {
            self.last_day_lynched = None;
            self.event_log
                .push(format!("第 {} 天：投票平票，无人放逐。", self.day));
            self.recap_log.push(RecapEvent::DayLynch { day: self.day, target: None });
            return Ok(None);
        }
        let lynched = top[0];
        self.players[lynched].alive = false;
        self.last_day_lynched = Some(lynched);
        self.deaths.push(DeathEvent {
            day: self.day,
            night: false,
            player_idx: lynched,
            cause: DeathCause::Lynch,
        });
        self.recap_log.push(RecapEvent::Death {
            day: self.day,
            night: false,
            player: lynched,
            cause: DeathCause::Lynch,
        });
        self.event_log.push(format!(
            "第 {} 天：{} 被投票放逐。",
            self.day, self.players[lynched].name
        ));
        self.recap_log.push(RecapEvent::DayLynch {
            day: self.day,
            target: Some(lynched),
        });
        Ok(Some(lynched))
    }

    /// 投票结束后调用：先放逐者遗言 → 猎人/狼王开枪 → 警徽流转，最后下一夜。
    pub fn advance_after_vote(&mut self) -> Result<()> {
        if let Some(lynched) = self.last_day_lynched {
            // 准备 pending 技能（不立即切阶段）
            let role = self.players[lynched].role;
            if role == Some(Role::Hunter) || role == Some(Role::WolfKing) {
                self.pending_hunter = Some(lynched);
                self.pending_hunter_post_stage = Some(Stage::DayVote);
            }
            if Some(lynched) == self.sheriff_idx {
                self.pending_badge = Some(lynched);
                self.pending_badge_post_stage = Some(Stage::DayVote);
            }
            // 放逐者遗言（一定有，放逐不是被毒）
            self.last_words_queue = vec![lynched];
            self.last_words_idx = 0;
            self.last_words_speeches.clear();
            self.last_words_public_msg = None;
            self.last_words_private_msg = None;
            self.last_words_post_stage = Some(Stage::DayVote);
            self.stage = Stage::LastWords;
            return Ok(());
        }
        self.advance_to_next_night_or_end();
        Ok(())
    }

    /// 猎人开枪：target 可为 None（不开枪）。
    pub fn hunter_shoot(
        &mut self,
        hunter_open_id: &str,
        target_open_id: Option<&str>,
    ) -> Result<Option<usize>> {
        if self.stage != Stage::HunterShoot {
            return Err(anyhow!("当前不是猎人开枪阶段"));
        }
        let h_idx = self
            .find_player(hunter_open_id)
            .ok_or_else(|| anyhow!("你不在桌上"))?;
        if Some(h_idx) != self.pending_hunter {
            return Err(anyhow!("不是你开枪"));
        }
        let mut shot_idx = None;
        if let Some(id) = target_open_id {
            let t = self
                .find_player(id)
                .ok_or_else(|| anyhow!("目标玩家不存在"))?;
            if !self.players[t].alive {
                return Err(anyhow!("不能枪杀已死亡的玩家"));
            }
            if t == h_idx {
                return Err(anyhow!("不能枪杀自己"));
            }
            self.players[t].alive = false;
            let night = matches!(self.pending_hunter_post_stage, Some(Stage::DayReveal));
            self.deaths.push(DeathEvent {
                day: self.day,
                night,
                player_idx: t,
                cause: DeathCause::HunterShot,
            });
            self.recap_log.push(RecapEvent::Death {
                day: self.day,
                night,
                player: t,
                cause: DeathCause::HunterShot,
            });
            // 公告中性化：不标记 猎人 / 狼王 角色（狼王惯例伪装成猎人），
            // 与公开广播卡片保持一致。
            self.event_log.push(format!(
                "{} 临死前开枪带走 {}",
                self.players[h_idx].name, self.players[t].name
            ));
            shot_idx = Some(t);
        } else {
            self.event_log
                .push(format!("{} 选择不开枪", self.players[h_idx].name));
        }
        self.recap_log.push(RecapEvent::HunterShot {
            day: self.day,
            shooter: h_idx,
            target: shot_idx,
        });

        // 把夜里的死亡加到 last_night_deaths（用于白天广播）
        let post = self.pending_hunter_post_stage.take();
        self.pending_hunter = None;
        self.pending_hunter_ai_decision = None;
        if matches!(post, Some(Stage::DayReveal)) {
            if let Some(t) = shot_idx {
                self.last_night_deaths.push(t);
            }
        }

        // 警长链：如果猎人本人或被猎杀者是警长，触发警徽流转
        let sheriff_dead = self
            .sheriff_idx
            .filter(|s| !self.players[*s].alive);
        if let Some(s_idx) = sheriff_dead {
            self.pending_badge = Some(s_idx);
            self.pending_badge_post_stage = post;
            self.stage = Stage::BadgePass;
        } else {
            self.continue_after_special_stage(post);
        }
        Ok(shot_idx)
    }

    /// 公共助手：进入下一夜，或者如果胜负已定就 Ended。
    fn advance_to_next_night_or_end(&mut self) {
        if let Some(_winner) = self.victory() {
            self.stage = Stage::Ended;
            return;
        }
        self.day += 1;
        // 有存活守卫 → 进 GuardPick；否则直接 WolvesPick
        let guard_alive = self
            .role_idx(Role::Guard)
            .map(|i| self.players[i].alive)
            .unwrap_or(false);
        self.stage = if guard_alive {
            Stage::GuardPick
        } else {
            Stage::WolvesPick
        };
        self.wolf_kill_votes.clear();
        self.wolf_chat.clear();
        self.wolf_ready.clear();
        self.wolf_night_msgs.clear();
        self.seer_check_target = None;
        self.witch_save_choice = None;
        self.witch_poison_target = None;
        self.guard_target = None;
        self.night_victim = None;
        self.witch_acted = false;
        self.last_night_deaths.clear();
        self.last_day_lynched = None;
        self.day_speech_order.clear();
        self.day_speech_idx = 0;
        self.day_speeches.clear();
        self.day_speech_public_msg = None;
        self.day_speech_private_msg = None;
        self.day_votes.clear();
        self.event_log
            .push(format!("第 {} 夜开始。", self.day));
    }

    /// 胜负检查。
    ///
    /// **屠城规则 + 警长例外**：
    /// - 全部狼死 → 好人胜
    /// - 狼数 > 好人数 → 狼胜（绝对多数）
    /// - 狼数 == 好人数：
    ///   - 警长存活且是好人 → 不算狼胜（1.5x 票权能压回，游戏继续）
    ///   - 否则 → 狼胜
    pub fn victory(&self) -> Option<Winner> {
        let wolves = self.alive_wolf_count();
        let good = self.alive_good_count();
        if wolves == 0 {
            return Some(Winner::Good);
        }
        if wolves > good {
            return Some(Winner::Wolves);
        }
        if wolves == good {
            // 警长还在好人手上 → 1.5x 票权使好人在投票中占优
            // 例如 1:1 时好人警长（3 权）vs 狼（2 权）→ 狼会被投出
            let sheriff_protects = self
                .sheriff_idx
                .filter(|s| self.players[*s].alive)
                .map(|s| !self.is_wolf(s))
                .unwrap_or(false);
            if !sheriff_protects {
                return Some(Winner::Wolves);
            }
        }
        None
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Winner {
    Good,
    Wolves,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn add(g: &mut WolfGame, id: &str, name: &str) {
        g.add_player(id.into(), name.into()).unwrap();
    }

    /// 9 人板锁定：P0/1/2 狼，P3 预言家，P4 女巫，P5 猎人，P6/7/8 村民。
    fn lock_roles_9(g: &mut WolfGame) {
        g.players[0].role = Some(Role::Werewolf);
        g.players[1].role = Some(Role::Werewolf);
        g.players[2].role = Some(Role::Werewolf);
        g.players[3].role = Some(Role::Seer);
        g.players[4].role = Some(Role::Witch);
        g.players[5].role = Some(Role::Hunter);
        g.players[6].role = Some(Role::Villager);
        g.players[7].role = Some(Role::Villager);
        g.players[8].role = Some(Role::Villager);
    }

    /// 12 人板锁定：P0-3 狼，P4 预言家，P5 女巫，P6 猎人，P7 守卫，P8-11 村民。
    fn lock_roles_12(g: &mut WolfGame) {
        g.players[0].role = Some(Role::Werewolf);
        g.players[1].role = Some(Role::Werewolf);
        g.players[2].role = Some(Role::Werewolf);
        g.players[3].role = Some(Role::Werewolf);
        g.players[4].role = Some(Role::Seer);
        g.players[5].role = Some(Role::Witch);
        g.players[6].role = Some(Role::Hunter);
        g.players[7].role = Some(Role::Guard);
        g.players[8].role = Some(Role::Villager);
        g.players[9].role = Some(Role::Villager);
        g.players[10].role = Some(Role::Villager);
        g.players[11].role = Some(Role::Villager);
    }

    fn make_n(n: usize) -> WolfGame {
        let mut g = WolfGame::new("c".into());
        for i in 0..n {
            add(&mut g, &format!("p{i}"), &format!("P{i}"));
        }
        g.start_game().unwrap();
        g
    }

    #[test]
    fn role_dist_9_to_12() {
        for n in 9..=12 {
            let r = role_distribution(n).unwrap();
            assert_eq!(r.len(), n);
            // 用 is_wolf 算总狼数（含狼王）
            let total_wolves = r.iter().filter(|x| x.is_wolf()).count();
            let expected_total = if n == 12 { 4 } else { 3 };
            assert_eq!(total_wolves, expected_total, "{n} 人板狼总数错误");
            // 10+ 板必须有狼王
            if n >= 10 {
                assert!(
                    r.iter().any(|x| matches!(x, Role::WolfKing)),
                    "{n} 人板应当有狼王"
                );
            }
            // 9 人板没狼王
            if n == 9 {
                assert!(!r.iter().any(|x| matches!(x, Role::WolfKing)));
            }
            assert!(r.iter().any(|x| matches!(x, Role::Seer)));
            assert!(r.iter().any(|x| matches!(x, Role::Witch)));
            assert!(r.iter().any(|x| matches!(x, Role::Hunter)));
            if n >= 10 {
                assert!(r.iter().any(|x| matches!(x, Role::Guard)), "{n} 人板缺守卫");
            }
        }
    }

    #[test]
    fn wolfking_lynched_can_shoot() {
        let mut g = make_n(10);
        g.players[0].role = Some(Role::Werewolf);
        g.players[1].role = Some(Role::Werewolf);
        g.players[2].role = Some(Role::WolfKing); // 狼王
        g.players[3].role = Some(Role::Seer);
        g.players[4].role = Some(Role::Witch);
        g.players[5].role = Some(Role::Hunter);
        g.players[6].role = Some(Role::Guard);
        g.players[7].role = Some(Role::Villager);
        g.players[8].role = Some(Role::Villager);
        g.players[9].role = Some(Role::Villager);

        // 强行进入投票阶段，把狼王投死
        g.stage = Stage::DayVote;
        g.last_day_lynched = Some(2);
        g.players[2].alive = false;
        g.deaths.push(DeathEvent {
            day: 1,
            night: false,
            player_idx: 2,
            cause: DeathCause::Lynch,
        });

        g.advance_after_vote().unwrap();
        // 放逐者先说遗言
        assert_eq!(g.stage, Stage::LastWords);
        assert_eq!(g.last_words_queue, vec![2]);
        g.submit_last_words("p2", "".into()).unwrap();
        g.finish_last_words().unwrap();
        // 遗言完毕 → 进入 HunterShoot
        assert_eq!(g.stage, Stage::HunterShoot);
        assert_eq!(g.pending_hunter, Some(2));

        // 狼王开枪打死 P3 (预言家)
        let shot = g.hunter_shoot("p2", Some("p3")).unwrap();
        assert_eq!(shot, Some(3));
        assert!(!g.players[3].alive);
    }

    #[test]
    fn is_wolf_includes_wolfking() {
        assert!(Role::WolfKing.is_wolf());
        assert!(Role::Werewolf.is_wolf());
        assert!(!Role::Seer.is_wolf());
    }

    #[test]
    fn recap_log_captures_night_actions() {
        let mut g = make_n(9);
        lock_roles_9(&mut g);

        g.wolf_pick("p0", "p8").unwrap();
        g.wolf_pick("p1", "p8").unwrap();
        g.wolf_pick("p2", "p8").unwrap();
        g.advance_after_wolves().unwrap();
        g.seer_check("p3", "p0").unwrap();
        g.witch_act("p4", false, None).unwrap();

        // 应当至少包含：WolfFinalTarget(P8), SeerCheck(P0=狼), Witch(skip), Death(P8 wolf-kill)
        assert!(matches!(
            g.recap_log.iter().find(|e| matches!(e, RecapEvent::WolfFinalTarget { target: Some(8), .. })),
            Some(_)
        ));
        assert!(matches!(
            g.recap_log.iter().find(|e| matches!(e, RecapEvent::SeerCheck { target: 0, is_wolf: true, .. })),
            Some(_)
        ));
        assert!(matches!(
            g.recap_log.iter().find(|e| matches!(e, RecapEvent::Witch { save: false, poison: None, .. })),
            Some(_)
        ));
        assert!(matches!(
            g.recap_log.iter().find(|e| matches!(e, RecapEvent::Death { player: 8, cause: DeathCause::WolfKill, .. })),
            Some(_)
        ));
    }

    #[test]
    fn night_deaths_have_no_last_words() {
        // 夜里死的人（无论狼刀还是毒杀）都不发表遗言。
        let mut g = make_n(9);
        lock_roles_9(&mut g);
        // 狼空刀（投自己人也行，简化用同一个目标但跳过狼刀）
        g.wolf_pick("p0", "p8").unwrap();
        g.wolf_pick("p1", "p8").unwrap();
        g.wolf_pick("p2", "p8").unwrap();
        g.advance_after_wolves().unwrap();
        g.seer_check("p3", "p0").unwrap();
        // 女巫救 P8（防狼刀）+ 不可同时毒。所以分两次测：
        // 这里只能用毒或救其中之一。让女巫不救 → P8 死于狼刀（有遗言）
        // 然后下一夜女巫再毒人 → 那时被毒者无遗言
        g.witch_act("p4", true, None).unwrap();
        // P8 被救，没死 → 没有 last_words
        assert_eq!(g.stage, Stage::DayReveal);
        assert!(g.last_night_deaths.is_empty());

        // 进白天，全员沉默，全员弃权 → 无放逐 → 进第二夜
        g.enter_day_discuss().unwrap();
        while !g.all_day_speeches_done() {
            let cur = g.current_day_speaker().unwrap();
            let oid = g.players[cur].open_id.clone();
            g.submit_day_speech(&oid, "".into()).unwrap();
        }
        g.enter_day_vote().unwrap();
        for i in g
            .alive_indices()
            .iter()
            .map(|x| g.players[*x].open_id.clone())
            .collect::<Vec<_>>()
        {
            g.cast_vote(&i, None).unwrap();
        }
        g.resolve_lynch().unwrap();
        g.advance_after_vote().unwrap();

        // 第二夜：狼空刀（P0 选自己已死的目标也不行；让狼刀某个非神）
        // 简化：跳过狼刀阶段，直接女巫毒人测试
        assert_eq!(g.stage, Stage::WolvesPick);
        g.wolf_pick("p0", "p8").unwrap();
        g.wolf_pick("p1", "p8").unwrap();
        g.wolf_pick("p2", "p8").unwrap();
        g.advance_after_wolves().unwrap();
        g.seer_check("p3", "p1").unwrap();
        // 女巫毒 P7（村民）。狼刀 P8 也死。两人都不进 last_words——夜里没有遗言阶段。
        g.witch_act("p4", false, Some("p7")).unwrap();
        assert_eq!(g.stage, Stage::DayReveal);
        assert!(g.last_words_queue.is_empty());
        assert!(!g.players[8].alive); // P8 死于狼刀
        assert!(!g.players[7].alive); // P7 死于毒
    }

    #[test]
    fn sheriff_picks_direction_then_speaks_last() {
        let mut g = make_n(10);
        g.players[0].role = Some(Role::Werewolf);
        g.players[1].role = Some(Role::Werewolf);
        g.players[2].role = Some(Role::WolfKing);
        g.players[3].role = Some(Role::Seer);
        g.players[4].role = Some(Role::Witch);
        g.players[5].role = Some(Role::Hunter);
        g.players[6].role = Some(Role::Guard);
        g.players[7].role = Some(Role::Villager);
        g.players[8].role = Some(Role::Villager);
        g.players[9].role = Some(Role::Villager);
        g.sheriff_idx = Some(3);
        g.stage = Stage::DayReveal;
        g.day = 2;

        g.enter_day_discuss().unwrap();
        // 警长存活 → 先选方向
        assert_eq!(g.stage, Stage::SheriffPickDirection);

        // 警长选警上（顺时针）
        g.pick_sheriff_direction("p3", true).unwrap();
        assert_eq!(g.stage, Stage::DaySpeech);
        // 顺序：4,5,6,7,8,9,0,1,2,3 → 警长末位归票
        assert_eq!(g.day_speech_order[0], 4);
        assert_eq!(g.day_speech_order.last(), Some(&3));

        // 没轮到不能插话
        let err = g
            .submit_day_speech("p3", "我先说".into())
            .unwrap_err();
        assert!(err.to_string().contains("还没轮到"));
    }

    #[test]
    fn sheriff_picks_警下_reverses_order() {
        let mut g = make_n(10);
        g.players[0].role = Some(Role::Werewolf);
        g.players[1].role = Some(Role::Werewolf);
        g.players[2].role = Some(Role::WolfKing);
        g.players[3].role = Some(Role::Seer);
        g.players[4].role = Some(Role::Witch);
        g.players[5].role = Some(Role::Hunter);
        g.players[6].role = Some(Role::Guard);
        g.players[7].role = Some(Role::Villager);
        g.players[8].role = Some(Role::Villager);
        g.players[9].role = Some(Role::Villager);
        g.sheriff_idx = Some(3);
        g.stage = Stage::DayReveal;
        g.day = 2;
        g.enter_day_discuss().unwrap();
        // 选警下（逆时针）
        g.pick_sheriff_direction("p3", false).unwrap();
        // 顺序：2,1,0,9,8,7,6,5,4,3
        assert_eq!(g.day_speech_order[0], 2);
        assert_eq!(g.day_speech_order[1], 1);
        assert_eq!(g.day_speech_order.last(), Some(&3));
    }

    #[test]
    fn multi_candidate_sheriff_goes_to_speech_then_vote() {
        let mut g = make_n(10);
        g.players[0].role = Some(Role::Werewolf);
        g.players[1].role = Some(Role::Werewolf);
        g.players[2].role = Some(Role::WolfKing);
        g.players[3].role = Some(Role::Seer);
        g.players[4].role = Some(Role::Witch);
        g.players[5].role = Some(Role::Hunter);
        g.players[6].role = Some(Role::Guard);
        g.players[7].role = Some(Role::Villager);
        g.players[8].role = Some(Role::Villager);
        g.players[9].role = Some(Role::Villager);
        g.stage = Stage::SheriffNominate;

        // P0 + P3 都上警
        for i in 0..10 {
            let id = format!("p{i}");
            let run = i == 3 || i == 0;
            g.nominate_sheriff(&id, run).unwrap();
        }
        g.finish_sheriff_nominate().unwrap();
        assert_eq!(g.stage, Stage::SheriffSpeech);
        assert_eq!(g.sheriff_speech_order, vec![0, 3]);

        g.submit_sheriff_speech("p0", "我是预言家".into()).unwrap();
        g.submit_sheriff_speech("p3", "他悍跳，我才是真预言家".into())
            .unwrap();
        assert!(g.all_sheriff_speeches_done());

        g.finish_sheriff_speeches().unwrap();
        // 候选人发完 → 直接进警长投票（没有警下发言环节）
        assert_eq!(g.stage, Stage::SheriffVote);
    }

    #[test]
    fn role_dist_rejects_out_of_range() {
        assert!(role_distribution(8).is_err());
        assert!(role_distribution(13).is_err());
        assert!(role_distribution(4).is_err());
    }

    #[test]
    fn sheriff_election_only_for_10_plus() {
        assert!(!has_sheriff_election(9));
        assert!(has_sheriff_election(10));
        assert!(has_sheriff_election(11));
        assert!(has_sheriff_election(12));
    }

    #[test]
    fn start_rejects_too_few_players() {
        let mut g = WolfGame::new("c".into());
        for i in 0..8 {
            add(&mut g, &format!("p{i}"), &format!("P{i}"));
        }
        let err = g.start_game().unwrap_err();
        assert!(err.to_string().contains("9-12"));
    }

    #[test]
    fn nine_player_starts_at_wolves_pick_no_guard() {
        let g = make_n(9);
        // 9 人没守卫，直接进狼人阶段
        assert_eq!(g.stage, Stage::WolvesPick);
        assert_eq!(g.day, 1);
        assert!(!g.sheriff_enabled);
    }

    #[test]
    fn twelve_player_starts_at_guard_pick_with_sheriff() {
        let g = make_n(12);
        assert_eq!(g.stage, Stage::GuardPick);
        assert!(g.sheriff_enabled);
    }

    #[test]
    fn full_round_9p_lynch_a_wolf() {
        let mut g = make_n(9);
        lock_roles_9(&mut g);

        g.wolf_pick("p0", "p8").unwrap();
        g.wolf_pick("p1", "p8").unwrap();
        g.wolf_pick("p2", "p8").unwrap();
        g.advance_after_wolves().unwrap();
        assert_eq!(g.stage, Stage::SeerPick);

        let is_wolf = g.seer_check("p3", "p0").unwrap();
        assert!(is_wolf);
        assert_eq!(g.stage, Stage::WitchAct);

        g.witch_act("p4", false, None).unwrap();
        // 夜里死的人不说遗言，直接 DayReveal
        assert_eq!(g.stage, Stage::DayReveal);
        assert_eq!(g.last_night_deaths, vec![8]);

        // 9 人不上警 → 直接进发言（无警长 → 直接 DaySpeech）
        g.enter_day_discuss().unwrap();
        assert_eq!(g.stage, Stage::DaySpeech);
        while !g.all_day_speeches_done() {
            let speaker_idx = g.current_day_speaker().unwrap();
            let oid = g.players[speaker_idx].open_id.clone();
            g.submit_day_speech(&oid, "".into()).unwrap();
        }
        g.enter_day_vote().unwrap();

        let alive: Vec<String> = g
            .alive_indices()
            .iter()
            .map(|i| g.players[*i].open_id.clone())
            .collect();
        for id in &alive {
            let target = if id == "p0" { "p1" } else { "p0" };
            g.cast_vote(id, Some(target)).unwrap();
        }
        let lynched = g.resolve_lynch().unwrap();
        assert_eq!(lynched, Some(0));

        g.advance_after_vote().unwrap();
        // 放逐者遗言
        assert_eq!(g.stage, Stage::LastWords);
        g.submit_last_words("p0", "".into()).unwrap();
        g.finish_last_words().unwrap();
        // 没猎人 / 警长 → 直接进下一夜
        assert_eq!(g.stage, Stage::WolvesPick);
        assert_eq!(g.day, 2);
    }

    #[test]
    fn witch_save_blocks_kill() {
        let mut g = make_n(9);
        lock_roles_9(&mut g);

        g.wolf_pick("p0", "p6").unwrap();
        g.wolf_pick("p1", "p6").unwrap();
        g.wolf_pick("p2", "p6").unwrap();
        g.advance_after_wolves().unwrap();
        g.seer_check("p3", "p0").unwrap();
        g.witch_act("p4", true, None).unwrap();
        assert_eq!(g.stage, Stage::DayReveal);
        assert!(g.last_night_deaths.is_empty(), "P6 should be saved");
        assert!(g.witch_save_used);
    }

    #[test]
    fn hunter_shot_after_wolf_kill() {
        let mut g = make_n(9);
        lock_roles_9(&mut g);

        g.wolf_pick("p0", "p5").unwrap();
        g.wolf_pick("p1", "p5").unwrap();
        g.wolf_pick("p2", "p5").unwrap();
        g.advance_after_wolves().unwrap();
        g.seer_check("p3", "p0").unwrap();
        g.witch_act("p4", false, None).unwrap();
        // 夜里死的人不说遗言；猎人直接进 HunterShoot 阶段
        // （由 dying_hunter_combined 一次性吐出『一句话+开枪目标』）
        assert_eq!(g.stage, Stage::HunterShoot);

        let shot = g.hunter_shoot("p5", Some("p0")).unwrap();
        assert_eq!(shot, Some(0));
        assert!(!g.players[0].alive);
        assert_eq!(g.stage, Stage::DayReveal);
    }

    #[test]
    fn witch_cannot_save_and_poison_same_night() {
        let mut g = make_n(9);
        lock_roles_9(&mut g);

        g.wolf_pick("p0", "p8").unwrap();
        g.wolf_pick("p1", "p8").unwrap();
        g.wolf_pick("p2", "p8").unwrap();
        g.advance_after_wolves().unwrap();
        g.seer_check("p3", "p0").unwrap();
        let r = g.witch_act("p4", true, Some("p1"));
        assert!(r.is_err(), "save+poison should reject");
    }

    #[test]
    fn guard_blocks_wolf_kill() {
        let mut g = make_n(12);
        lock_roles_12(&mut g);
        // P7 是守卫，守 P8 (村民)
        g.guard_pick("p7", "p8").unwrap();
        assert_eq!(g.stage, Stage::WolvesPick);

        // 三狼刀 P8
        g.wolf_pick("p0", "p8").unwrap();
        g.wolf_pick("p1", "p8").unwrap();
        g.wolf_pick("p2", "p8").unwrap();
        g.advance_after_wolves().unwrap();
        g.seer_check("p4", "p0").unwrap();
        g.witch_act("p5", false, None).unwrap();
        assert_eq!(g.stage, Stage::DayReveal);
        assert!(g.last_night_deaths.is_empty(), "守卫应当挡住狼刀");
        assert!(g.players[8].alive);
    }

    #[test]
    fn double_protect_kills_target() {
        // 同守同救：守卫守 P8 + 女巫救 P8 = P8 依然死亡
        let mut g = make_n(12);
        lock_roles_12(&mut g);
        g.guard_pick("p7", "p8").unwrap();
        g.wolf_pick("p0", "p8").unwrap();
        g.wolf_pick("p1", "p8").unwrap();
        g.wolf_pick("p2", "p8").unwrap();
        g.advance_after_wolves().unwrap();
        g.seer_check("p4", "p0").unwrap();
        g.witch_act("p5", true, None).unwrap();
        // 死了 P8 → 夜里不说遗言，直接 DayReveal
        assert_eq!(g.stage, Stage::DayReveal);
        assert_eq!(g.last_night_deaths, vec![8], "同守同救应当死亡");
    }

    #[test]
    fn guard_cannot_repeat_target() {
        let mut g = make_n(12);
        lock_roles_12(&mut g);
        g.guard_pick("p7", "p8").unwrap();
        // 跑完一夜进第二夜
        g.wolf_pick("p0", "p8").unwrap();
        g.wolf_pick("p1", "p8").unwrap();
        g.wolf_pick("p2", "p8").unwrap();
        g.advance_after_wolves().unwrap();
        g.seer_check("p4", "p0").unwrap();
        g.witch_act("p5", false, None).unwrap();
        // 一直推到下一夜
        g.enter_day_discuss().unwrap();
        // 12 人启用上警，跳过流程：让所有人不上警
        if g.stage == Stage::SheriffNominate {
            for id in g
                .alive_indices()
                .iter()
                .map(|i| g.players[*i].open_id.clone())
                .collect::<Vec<_>>()
            {
                g.nominate_sheriff(&id, false).unwrap();
            }
            g.finish_sheriff_nominate().unwrap();
        }
        // 全员沉默跳过白天发言
        while !g.all_day_speeches_done() {
            let speaker_idx = g.current_day_speaker().unwrap();
            let oid = g.players[speaker_idx].open_id.clone();
            g.submit_day_speech(&oid, "".into()).unwrap();
        }
        g.enter_day_vote().unwrap();
        // 全员弃权 → 没人放逐
        for id in g
            .alive_indices()
            .iter()
            .map(|i| g.players[*i].open_id.clone())
            .collect::<Vec<_>>()
        {
            g.cast_vote(&id, None).unwrap();
        }
        g.resolve_lynch().unwrap();
        g.advance_after_vote().unwrap();
        // 进入第二夜守卫阶段
        assert_eq!(g.stage, Stage::GuardPick);
        // 不能继续守 P8
        let r = g.guard_pick("p7", "p8");
        assert!(r.is_err(), "连续两晚守同一人应被拒");
    }

    #[test]
    fn sheriff_election_single_candidate_auto_elected() {
        let mut g = make_n(10);
        // 10 人板：3 狼 + 预女猎守 + 3 民。开局先经历 GuardPick (默认守自己) → 狼 → 预 → 女
        g.players[0].role = Some(Role::Werewolf);
        g.players[1].role = Some(Role::Werewolf);
        g.players[2].role = Some(Role::Werewolf);
        g.players[3].role = Some(Role::Seer);
        g.players[4].role = Some(Role::Witch);
        g.players[5].role = Some(Role::Hunter);
        g.players[6].role = Some(Role::Guard);
        g.players[7].role = Some(Role::Villager);
        g.players[8].role = Some(Role::Villager);
        g.players[9].role = Some(Role::Villager);

        g.guard_pick("p6", "p6").unwrap();
        g.wolf_pick("p0", "p9").unwrap();
        g.wolf_pick("p1", "p9").unwrap();
        g.wolf_pick("p2", "p9").unwrap();
        g.advance_after_wolves().unwrap();
        g.seer_check("p3", "p0").unwrap();
        g.witch_act("p4", false, None).unwrap();
        // P9 死于狼刀，夜里不说遗言，直接进 DayReveal
        assert_eq!(g.stage, Stage::DayReveal);

        // 进入上警阶段
        g.enter_day_discuss().unwrap();
        assert_eq!(g.stage, Stage::SheriffNominate);

        // 全员决定：只有 P3 上警
        for id in g
            .alive_indices()
            .iter()
            .map(|i| g.players[*i].open_id.clone())
            .collect::<Vec<_>>()
        {
            let run = id == "p3";
            g.nominate_sheriff(&id, run).unwrap();
        }
        g.finish_sheriff_nominate().unwrap();
        // 唯一候选 → 自动当选 → 让警长选方向
        assert_eq!(g.sheriff_idx, Some(3));
        assert_eq!(g.stage, Stage::SheriffPickDirection);
    }

    #[test]
    fn sheriff_15x_vote_weight() {
        let mut g = make_n(10);
        g.players[0].role = Some(Role::Werewolf);
        g.players[1].role = Some(Role::Werewolf);
        g.players[2].role = Some(Role::Werewolf);
        g.players[3].role = Some(Role::Seer);
        g.players[4].role = Some(Role::Witch);
        g.players[5].role = Some(Role::Hunter);
        g.players[6].role = Some(Role::Guard);
        g.players[7].role = Some(Role::Villager);
        g.players[8].role = Some(Role::Villager);
        g.players[9].role = Some(Role::Villager);
        g.sheriff_idx = Some(3); // 直接设警长

        // 强行进入投票阶段
        g.stage = Stage::DayVote;
        // 5 人投 P0，4 人（含警长）投 P1：
        // 普通 5 票 = 10 权重投 P0
        // 警长 1 票 (3 权重) + 普通 3 票 (6 权重) = 9 权重投 P1
        // → P0 得票 10，P1 得票 9，P0 被放逐
        for i in [4, 5, 6, 7, 8] {
            let id = format!("p{i}");
            g.cast_vote(&id, Some("p0")).unwrap();
        }
        for i in [3, 9, 1, 2] {
            let id = format!("p{i}");
            // P1/P2 自己不能投自己 → 让他们投 P0 也可以，但这里 they vote P1 to test weight
            // 实际上 P1/P2 投自己会失败，让他们 abstain
            if i == 1 || i == 2 {
                g.cast_vote(&id, None).unwrap();
            } else {
                g.cast_vote(&id, Some("p1")).unwrap();
            }
        }
        let lynched = g.resolve_lynch().unwrap();
        assert_eq!(lynched, Some(0));
    }

    #[test]
    fn sheriff_dies_triggers_badge_pass() {
        let mut g = make_n(10);
        g.players[0].role = Some(Role::Werewolf);
        g.players[1].role = Some(Role::Werewolf);
        g.players[2].role = Some(Role::Werewolf);
        g.players[3].role = Some(Role::Seer);
        g.players[4].role = Some(Role::Witch);
        g.players[5].role = Some(Role::Hunter);
        g.players[6].role = Some(Role::Guard);
        g.players[7].role = Some(Role::Villager);
        g.players[8].role = Some(Role::Villager);
        g.players[9].role = Some(Role::Villager);
        g.sheriff_idx = Some(3);

        // 守卫不守警长，让狼刀杀掉警长
        g.guard_pick("p6", "p6").unwrap();
        g.wolf_pick("p0", "p3").unwrap();
        g.wolf_pick("p1", "p3").unwrap();
        g.wolf_pick("p2", "p3").unwrap();
        g.advance_after_wolves().unwrap();
        // 警长（P3 预言家）已死，不能再查验。这里手动跳过
        // 实际 advance_after_wolves 会进 SeerPick，但 P3 已死 → 直接到 WitchAct
        // 简化：手动测警徽流转
        g.stage = Stage::WitchAct;
        g.night_victim = Some(3);
        g.witch_act("p4", false, None).unwrap();
        // 警长死于夜里，不说遗言；pending_badge 走 BadgePass
        assert_eq!(g.stage, Stage::BadgePass);
        assert_eq!(g.pending_badge, Some(3));

        // 撕毁警徽
        let new_holder = g.transfer_badge("p3", None).unwrap();
        assert_eq!(new_holder, None);
        assert_eq!(g.sheriff_idx, None);
        assert_eq!(g.stage, Stage::DayReveal);
    }

    #[test]
    fn victory_when_all_wolves_dead() {
        let mut g = make_n(9);
        lock_roles_9(&mut g);
        g.players[0].alive = false;
        g.players[1].alive = false;
        g.players[2].alive = false;
        assert_eq!(g.victory(), Some(Winner::Good));
    }

    #[test]
    fn victory_when_wolves_outnumber() {
        // 2 狼 vs 1 好人 (绝对多数) → 狼胜，无关警长
        let mut g = make_n(9);
        lock_roles_9(&mut g);
        g.players[2].alive = false;
        for i in 3..=7 {
            g.players[i].alive = false;
        }
        assert_eq!(g.alive_wolf_count(), 2);
        assert_eq!(g.alive_good_count(), 1);
        assert_eq!(g.victory(), Some(Winner::Wolves));
    }

    #[test]
    fn victory_at_parity_no_sheriff_wolves_win() {
        // 1 狼 vs 1 好人，无警长 → 狼胜（屠城，1.5x 不存在）
        let mut g = make_n(9);
        lock_roles_9(&mut g);
        // 杀掉 P1 P2（其他 2 狼）+ P3-7（5 个好人），剩 P0 狼 + P8 好人
        g.players[1].alive = false;
        g.players[2].alive = false;
        for i in 3..=7 {
            g.players[i].alive = false;
        }
        assert_eq!(g.alive_wolf_count(), 1);
        assert_eq!(g.alive_good_count(), 1);
        assert!(g.sheriff_idx.is_none());
        assert_eq!(g.victory(), Some(Winner::Wolves));
    }

    #[test]
    fn victory_at_parity_with_good_sheriff_continues() {
        // 1 狼 vs 1 好人，警长是好人 → 不算狼胜（1.5x 票权能压回）
        let mut g = make_n(9);
        lock_roles_9(&mut g);
        g.players[1].alive = false;
        g.players[2].alive = false;
        for i in 3..=7 {
            g.players[i].alive = false;
        }
        // P8 (好人村民) 是警长
        g.sheriff_idx = Some(8);
        assert_eq!(g.alive_wolf_count(), 1);
        assert_eq!(g.alive_good_count(), 1);
        assert_eq!(g.victory(), None, "好人警长在场时 1:1 不算狼胜");
    }

    #[test]
    fn victory_at_parity_with_dead_sheriff_wolves_win() {
        // 1 狼 vs 1 好人，警长已死 → 狼胜
        let mut g = make_n(9);
        lock_roles_9(&mut g);
        g.players[1].alive = false;
        g.players[2].alive = false;
        for i in 3..=7 {
            g.players[i].alive = false;
        }
        // P3 曾是警长，但已死
        g.sheriff_idx = Some(3);
        assert_eq!(g.victory(), Some(Winner::Wolves));
    }

    #[test]
    fn victory_at_parity_with_wolf_sheriff_wolves_win() {
        // 1 狼 vs 1 好人，警长在狼身上 → 狼胜（狼也享受 1.5x，更乱）
        let mut g = make_n(9);
        lock_roles_9(&mut g);
        g.players[1].alive = false;
        g.players[2].alive = false;
        for i in 3..=7 {
            g.players[i].alive = false;
        }
        g.sheriff_idx = Some(0); // P0 是狼且是警长
        assert_eq!(g.victory(), Some(Winner::Wolves));
    }
}
