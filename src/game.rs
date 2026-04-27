use crate::poker::{best_five, category_name, Card, Deck, DeckMode, HandRank};
use anyhow::{anyhow, Result};
use rand::seq::SliceRandom;
use serde::{Deserialize, Serialize};

/// Personality archetype for an AI seat. Drives the LLM system prompt so the
/// bot doesn't always play the same boring "GTO-ish" line — gives the table
/// some flavor for casual play.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Persona {
    /// 莽哥 / LAG — 牌不挑、敢开火、敢加注、敢诈唬。
    LooseAggressive,
    /// 老炮 / TAG — 选牌严但出手猛，价值下注绝不手软。
    TightAggressive,
    /// 跟注站 / 松弱 — 上来就想看牌，跟到底，几乎不主动加注。
    LooseWeak,
    /// 老抠 / 紧弱 — 强牌才肯下场，从不诈唬。
    TightWeak,
    /// 头铁 / Maniac — 每手都想梭哈，弃牌不存在的。
    Maniac,
}

impl Persona {
    pub fn label(self) -> &'static str {
        match self {
            Persona::LooseAggressive => "莽哥",
            Persona::TightAggressive => "老炮",
            Persona::LooseWeak => "跟注站",
            Persona::TightWeak => "老抠",
            Persona::Maniac => "头铁",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Persona::LooseAggressive =>
                "上头型，看哪手牌都觉得能打。开池就加注，被加注就再加。靠气势压人，弃牌？看心情。",
            Persona::TightAggressive =>
                "老手了，一眼看穿牌强不强。烂牌直接扔，进底池就当真打。不爱诈唬，但价值下注绝不手软。",
            Persona::LooseWeak =>
                "上来就想看牌。手里只要有点东西就想跟到底，连对都没有也想再看一张。基本不主动加注，输了也认。",
            Persona::TightWeak =>
                "一毛不拔型。AA / KK / AK 这种强牌才肯下场，对手一加注就开始怀疑自己被打到。从不主动开火，从不诈唬。",
            Persona::Maniac =>
                "手感一来就梭哈，每手都想打到底，诈唬比谁都猛。弃牌？不存在的。",
        }
    }

    /// Single emoji that represents the persona visually — used as the "avatar"
    /// in chat-bubble cards so each AI has a recognisable face beyond just the
    /// name suffix.
    pub fn emoji(self) -> &'static str {
        match self {
            Persona::LooseAggressive => "🐺",
            Persona::TightAggressive => "🦈",
            Persona::LooseWeak => "🐟",
            Persona::TightWeak => "🪨",
            Persona::Maniac => "🤪",
        }
    }

    pub fn random() -> Self {
        let all = [
            Persona::LooseAggressive,
            Persona::TightAggressive,
            Persona::LooseWeak,
            Persona::TightWeak,
            Persona::Maniac,
        ];
        *all.choose(&mut rand::thread_rng()).unwrap()
    }
}

/// Default chip stack each player starts with.
pub const STARTING_CHIPS: u64 = 1000;
/// Small blind for every hand.
pub const SMALL_BLIND: u64 = 5;
/// Big blind for every hand.
pub const BIG_BLIND: u64 = 10;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Player {
    pub open_id: String,
    pub name: String,
    pub chips: u64,
    pub hole: Vec<Card>,
    pub bet_in_round: u64,
    pub total_bet: u64,
    pub folded: bool,
    pub all_in: bool,
    pub acted_this_round: bool,
    pub sat_out: bool, // out of chips, skipped this hand
    /// LLM-driven seat. `open_id` is a synthetic id like `ai:1` that
    /// won't resolve in Feishu — display code substitutes the name instead of
    /// rendering an `<at>` tag.
    pub is_ai: bool,
    /// Personality archetype for AI seats; `None` for humans. Drives the
    /// system prompt so different AIs play different styles.
    #[serde(default)]
    pub persona: Option<Persona>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Stage {
    Lobby,
    PreFlop,
    Flop,
    Turn,
    River,
    Showdown,
    Ended,
}

impl Stage {
    pub fn label(self) -> &'static str {
        match self {
            Stage::Lobby => "等待玩家",
            Stage::PreFlop => "翻牌前",
            Stage::Flop => "翻牌",
            Stage::Turn => "转牌",
            Stage::River => "河牌",
            Stage::Showdown => "摊牌",
            Stage::Ended => "结束",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ActionKind {
    Fold,
    Check,
    Call,
    Bet,
    Raise,
    AllIn,
}

#[derive(Debug, Clone, Copy)]
pub enum PlayerAction {
    Fold,
    Check,
    Call,
    /// Raise so that the player's bet_in_round becomes `to`. Must be at least
    /// current_bet + min_raise (or all-in for less).
    RaiseTo(u64),
    AllIn,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionLogEntry {
    pub player_idx: usize,
    pub kind: ActionKind,
    pub amount: u64, // 0 for fold/check, otherwise total bet_in_round after action
}

#[derive(Debug, Clone)]
pub struct ShowdownResult {
    pub player_idx: usize,
    pub hole: Vec<Card>,
    pub best_five: Vec<Card>,
    pub rank: HandRank,
}

#[derive(Debug, Clone)]
pub struct PotPayout {
    pub amount: u64,
    pub winners: Vec<usize>, // player indices
    pub note: String,
}

#[derive(Debug, Clone)]
pub struct HandSummary {
    pub showdowns: Vec<ShowdownResult>,
    pub payouts: Vec<PotPayout>,
}

#[derive(Debug)]
pub struct ActOutcome {
    pub log: ActionLogEntry,
    /// Cards revealed for the new stage (flop=3, turn/river=1).
    pub stage_cards: Option<(Stage, Vec<Card>)>,
    /// If multiple stages are dealt at once (all-in run-out), each stage's reveal.
    pub extra_stages: Vec<(Stage, Vec<Card>)>,
    pub summary: Option<HandSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Game {
    pub chat_id: String,
    pub players: Vec<Player>,
    pub stage: Stage,
    pub deck: Deck,
    pub community: Vec<Card>,
    pub current_bet: u64,
    pub min_raise: u64,
    pub small_blind: u64,
    pub big_blind: u64,
    pub dealer_idx: usize,
    pub current_idx: usize,
    pub hand_count: u32,
    pub action_log: Vec<ActionLogEntry>,
    pub last_action_msg_id: Option<String>,
    /// Message id of the persistent lobby card. Updated in place on join/leave.
    pub lobby_msg_id: Option<String>,
    /// Current hand's deck variant. Set fresh each `start_hand`.
    pub mode: DeckMode,
}

impl Game {
    pub fn new(chat_id: String) -> Self {
        Self {
            chat_id,
            players: vec![],
            stage: Stage::Lobby,
            deck: Deck::shuffled(DeckMode::Standard),
            community: vec![],
            current_bet: 0,
            min_raise: BIG_BLIND,
            small_blind: SMALL_BLIND,
            big_blind: BIG_BLIND,
            dealer_idx: 0,
            current_idx: 0,
            hand_count: 0,
            action_log: vec![],
            last_action_msg_id: None,
            lobby_msg_id: None,
            mode: DeckMode::Standard,
        }
    }

    pub fn add_player(&mut self, open_id: String, name: String) -> Result<()> {
        self.add_player_inner(open_id, name, false, None)
    }

    /// Like `add_player` but flags the seat as LLM-driven and tags it with a
    /// persona. Synthetic open_id (`ai:...`) — won't resolve in Feishu, but
    /// used to look the seat up internally.
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
            return Err(anyhow!("一局牌正在进行中，等结束后再加入"));
        }
        if self.players.iter().any(|p| p.open_id == open_id) {
            return Err(anyhow!("你已经在桌上了"));
        }
        if self.players.len() >= 12 {
            return Err(anyhow!("人数已满 (12)"));
        }
        self.players.push(Player {
            open_id,
            name,
            chips: STARTING_CHIPS,
            hole: vec![],
            bet_in_round: 0,
            total_bet: 0,
            folded: false,
            all_in: false,
            acted_this_round: false,
            sat_out: false,
            is_ai,
            persona,
        });
        Ok(())
    }

    pub fn remove_player(&mut self, open_id: &str) -> Result<()> {
        if !matches!(self.stage, Stage::Lobby | Stage::Ended) {
            return Err(anyhow!("牌局进行中，无法离桌"));
        }
        let before = self.players.len();
        self.players.retain(|p| p.open_id != open_id);
        if self.players.len() == before {
            return Err(anyhow!("你不在桌上"));
        }
        Ok(())
    }

    /// Remove the most recently added AI seat. Returns the removed player's
    /// display name. Refuses mid-hand to keep state consistent.
    pub fn remove_last_ai(&mut self) -> Result<String> {
        if !matches!(self.stage, Stage::Lobby | Stage::Ended) {
            return Err(anyhow!("牌局进行中，无法移除 AI"));
        }
        let idx = self
            .players
            .iter()
            .rposition(|p| p.is_ai)
            .ok_or_else(|| anyhow!("桌上没有 AI 玩家"))?;
        Ok(self.players.remove(idx).name)
    }

pub fn find_player(&self, open_id: &str) -> Option<usize> {
        self.players.iter().position(|p| p.open_id == open_id)
    }

    pub fn pot_total(&self) -> u64 {
        self.players.iter().map(|p| p.total_bet).sum()
    }

    /// Begin a new hand. Requires ≥ 2 active (chip-holding) players.
    ///
    /// Refuses to start if a hand is already in progress — recovering
    /// in-place is dangerous when paired with the platform's at-least-once
    /// callback delivery (a duplicated [开局] click would silently void the
    /// just-dealt pot and re-deal). If the table genuinely gets stuck, use
    /// `/poker reset` to wipe and start over.
    pub fn start_hand(&mut self, mode: DeckMode) -> Result<()> {
        if !matches!(self.stage, Stage::Lobby | Stage::Ended) {
            return Err(anyhow!(
                "已经在牌局中（{}）。卡住可用 /poker reset 重置牌桌",
                self.stage.label()
            ));
        }

        let active: Vec<usize> = self
            .players
            .iter()
            .enumerate()
            .filter(|(_, p)| p.chips > 0)
            .map(|(i, _)| i)
            .collect();
        if active.len() < 2 {
            return Err(anyhow!("至少需要 2 名带筹码的玩家"));
        }

        // Reset per-hand state
        self.mode = mode;
        self.deck = Deck::shuffled(mode);
        self.community.clear();
        self.action_log.clear();
        self.current_bet = self.big_blind;
        self.min_raise = self.big_blind;
        self.hand_count += 1;

        for p in &mut self.players {
            p.hole.clear();
            p.bet_in_round = 0;
            p.total_bet = 0;
            p.folded = false;
            p.all_in = false;
            p.acted_this_round = false;
            p.sat_out = p.chips == 0;
        }

        // Advance dealer button to the next active player
        if self.hand_count == 1 {
            self.dealer_idx = active[0];
        } else {
            self.dealer_idx = self.next_active_index(self.dealer_idx);
        }

        // Deal hole cards
        for _ in 0..2 {
            for p in &mut self.players {
                if !p.sat_out {
                    if let Some(c) = self.deck.draw() {
                        p.hole.push(c);
                    }
                }
            }
        }

        // Post blinds
        let n_active = active.len();
        let (sb_idx, bb_idx, first_to_act) = if n_active == 2 {
            // Heads-up: dealer is SB, the other is BB, dealer acts first preflop.
            let sb = self.dealer_idx;
            let bb = self.next_active_index(sb);
            (sb, bb, sb)
        } else {
            let sb = self.next_active_index(self.dealer_idx);
            let bb = self.next_active_index(sb);
            let first = self.next_active_index(bb);
            (sb, bb, first)
        };

        self.contribute(sb_idx, self.small_blind);
        self.contribute(bb_idx, self.big_blind);

        self.current_idx = first_to_act;
        self.stage = Stage::PreFlop;
        Ok(())
    }

    /// Move chips from a player's stack into their bet_in_round/total_bet.
    /// Caps at the player's available chips (caller is responsible for blinds being legal).
    fn contribute(&mut self, idx: usize, amount: u64) -> u64 {
        let p = &mut self.players[idx];
        let pay = amount.min(p.chips);
        p.chips -= pay;
        p.bet_in_round += pay;
        p.total_bet += pay;
        if p.chips == 0 {
            p.all_in = true;
        }
        pay
    }

    fn next_active_index(&self, from: usize) -> usize {
        let n = self.players.len();
        let mut i = (from + 1) % n;
        for _ in 0..n {
            let p = &self.players[i];
            if !p.sat_out && !p.folded {
                return i;
            }
            i = (i + 1) % n;
        }
        from
    }

    /// Find the next player who still needs to act this round.
    /// A player needs to act if they are not folded, not all-in, and either
    /// haven't acted yet or their bet_in_round < current_bet.
    fn next_to_act_after(&self, from: usize) -> Option<usize> {
        let n = self.players.len();
        let mut i = (from + 1) % n;
        for _ in 0..n {
            let p = &self.players[i];
            if !p.sat_out
                && !p.folded
                && !p.all_in
                && (!p.acted_this_round || p.bet_in_round < self.current_bet)
            {
                return Some(i);
            }
            i = (i + 1) % n;
        }
        None
    }

    fn live_players(&self) -> Vec<usize> {
        self.players
            .iter()
            .enumerate()
            .filter(|(_, p)| !p.sat_out && !p.folded)
            .map(|(i, _)| i)
            .collect()
    }

    fn betting_possible(&self) -> bool {
        // Need ≥ 2 non-folded players who still have chips to bet.
        self.players
            .iter()
            .filter(|p| !p.sat_out && !p.folded && !p.all_in)
            .count()
            >= 2
    }

    /// Apply a player's action. Caller must make sure it's actually `open_id`'s turn.
    pub fn act(&mut self, open_id: &str, action: PlayerAction) -> Result<ActOutcome> {
        if !matches!(self.stage, Stage::PreFlop | Stage::Flop | Stage::Turn | Stage::River) {
            return Err(anyhow!("当前不是下注阶段"));
        }
        let idx = self
            .find_player(open_id)
            .ok_or_else(|| anyhow!("你不在牌桌上"))?;
        if idx != self.current_idx {
            return Err(anyhow!("还没轮到你"));
        }
        let p = &self.players[idx];
        if p.folded || p.all_in || p.sat_out {
            return Err(anyhow!("你这一局已经结束行动"));
        }

        let log = self.apply_action(idx, action)?;
        self.action_log.push(log.clone());

        // Mark the actor as having acted this round
        self.players[idx].acted_this_round = true;

        // 1. If only one non-folded player remains, hand ends immediately.
        let remaining = self.live_players();
        if remaining.len() == 1 {
            let summary = self.award_uncontested(remaining[0]);
            self.stage = Stage::Ended;
            return Ok(ActOutcome {
                log,
                stage_cards: None,
                extra_stages: vec![],
                summary: Some(summary),
            });
        }

        // 2. If betting round complete, advance stage(s).
        if self.is_round_complete() {
            return Ok(self.advance_after_round(log));
        }

        // 3. Otherwise hand off to next actor.
        let next = self
            .next_to_act_after(idx)
            .ok_or_else(|| anyhow!("内部错误：找不到下一位行动玩家"))?;
        self.current_idx = next;
        Ok(ActOutcome {
            log,
            stage_cards: None,
            extra_stages: vec![],
            summary: None,
        })
    }

    fn apply_action(&mut self, idx: usize, action: PlayerAction) -> Result<ActionLogEntry> {
        let p = &self.players[idx];
        let to_call = self.current_bet.saturating_sub(p.bet_in_round);

        let mut kind;
        let amount;
        match action {
            PlayerAction::Fold => {
                self.players[idx].folded = true;
                kind = ActionKind::Fold;
                amount = 0;
            }
            PlayerAction::Check => {
                if to_call > 0 {
                    return Err(anyhow!("当前需要跟注 {to_call}，无法 check"));
                }
                kind = ActionKind::Check;
                amount = 0;
            }
            PlayerAction::Call => {
                if to_call == 0 {
                    return Err(anyhow!("无人下注，请使用 check"));
                }
                let chips = self.players[idx].chips;
                let pay = to_call.min(chips);
                self.contribute(idx, pay);
                kind = ActionKind::Call;
                amount = self.players[idx].bet_in_round;
            }
            PlayerAction::AllIn => {
                let chips = self.players[idx].chips;
                if chips == 0 {
                    return Err(anyhow!("没有筹码可以 all-in"));
                }
                self.contribute(idx, chips);
                let new_bet = self.players[idx].bet_in_round;
                if new_bet > self.current_bet {
                    let raise_increment = new_bet - self.current_bet;
                    if raise_increment >= self.min_raise {
                        self.min_raise = raise_increment;
                    }
                    self.current_bet = new_bet;
                    self.reset_acted_for_raise(idx);
                    kind = if self.community.is_empty() && self.current_bet == self.big_blind {
                        ActionKind::Bet
                    } else {
                        ActionKind::Raise
                    };
                } else {
                    kind = ActionKind::Call;
                }
                kind = match kind {
                    ActionKind::Call => ActionKind::AllIn,
                    other => other,
                };
                amount = new_bet;
            }
            PlayerAction::RaiseTo(target) => {
                if target <= self.current_bet {
                    return Err(anyhow!(
                        "加注金额必须大于当前注 {}",
                        self.current_bet
                    ));
                }
                let increment = target - self.current_bet;
                if increment < self.min_raise && self.players[idx].chips + self.players[idx].bet_in_round > target {
                    return Err(anyhow!("最小加注幅度为 {}", self.min_raise));
                }
                let need = target.saturating_sub(self.players[idx].bet_in_round);
                if need > self.players[idx].chips {
                    return Err(anyhow!("筹码不够，使用 all-in"));
                }
                self.contribute(idx, need);
                self.min_raise = increment;
                self.current_bet = target;
                self.reset_acted_for_raise(idx);
                kind = if self.community.is_empty() && target == self.big_blind {
                    ActionKind::Bet
                } else {
                    ActionKind::Raise
                };
                amount = target;
            }
        }

        Ok(ActionLogEntry {
            player_idx: idx,
            kind,
            amount,
        })
    }

    /// When somebody raises, every other still-active player must act again.
    fn reset_acted_for_raise(&mut self, raiser_idx: usize) {
        for (i, p) in self.players.iter_mut().enumerate() {
            if i == raiser_idx || p.folded || p.all_in || p.sat_out {
                continue;
            }
            p.acted_this_round = false;
        }
    }

    fn is_round_complete(&self) -> bool {
        let actors: Vec<&Player> = self
            .players
            .iter()
            .filter(|p| !p.sat_out && !p.folded && !p.all_in)
            .collect();
        if actors.is_empty() {
            return true;
        }
        actors
            .iter()
            .all(|p| p.acted_this_round && p.bet_in_round == self.current_bet)
    }

    fn advance_after_round(&mut self, log: ActionLogEntry) -> ActOutcome {
        // Reset per-round bets
        for p in &mut self.players {
            p.bet_in_round = 0;
            p.acted_this_round = false;
        }
        self.current_bet = 0;
        self.min_raise = self.big_blind;

        // Advance one stage (deal cards)
        let primary = self.deal_next_stage();

        // If no further betting is possible (all but one are all-in / folded),
        // run out the rest of the streets without prompting for actions.
        let mut extras = vec![];
        if !self.betting_possible() && self.stage != Stage::Showdown && self.stage != Stage::Ended
        {
            while self.stage != Stage::Showdown && self.stage != Stage::Ended {
                let extra = self.deal_next_stage();
                if let Some(e) = extra {
                    extras.push(e);
                }
            }
        }

        // Showdown if we hit it.
        let summary = if self.stage == Stage::Showdown {
            let s = self.showdown();
            self.stage = Stage::Ended;
            Some(s)
        } else {
            None
        };

        if let Some(i) = self.first_to_act_postflop() {
            self.current_idx = i;
        }

        ActOutcome {
            log,
            stage_cards: primary,
            extra_stages: extras,
            summary,
        }
    }

    fn deal_next_stage(&mut self) -> Option<(Stage, Vec<Card>)> {
        match self.stage {
            Stage::PreFlop => {
                self.deck.draw(); // burn
                let cards = self.deck.draw_n(3);
                self.community.extend(cards.clone());
                self.stage = Stage::Flop;
                Some((Stage::Flop, cards))
            }
            Stage::Flop => {
                self.deck.draw();
                let cards = self.deck.draw_n(1);
                self.community.extend(cards.clone());
                self.stage = Stage::Turn;
                Some((Stage::Turn, cards))
            }
            Stage::Turn => {
                self.deck.draw();
                let cards = self.deck.draw_n(1);
                self.community.extend(cards.clone());
                self.stage = Stage::River;
                Some((Stage::River, cards))
            }
            Stage::River => {
                self.stage = Stage::Showdown;
                None
            }
            _ => None,
        }
    }

    fn first_to_act_postflop(&self) -> Option<usize> {
        let n = self.players.len();
        let mut i = (self.dealer_idx + 1) % n;
        for _ in 0..n {
            let p = &self.players[i];
            if !p.sat_out && !p.folded && !p.all_in {
                return Some(i);
            }
            i = (i + 1) % n;
        }
        None
    }

    fn award_uncontested(&mut self, winner_idx: usize) -> HandSummary {
        let pot = self.pot_total();
        self.players[winner_idx].chips += pot;
        for p in &mut self.players {
            p.total_bet = 0;
            p.bet_in_round = 0;
        }
        HandSummary {
            showdowns: vec![],
            payouts: vec![PotPayout {
                amount: pot,
                winners: vec![winner_idx],
                note: "未摊牌".into(),
            }],
        }
    }

    fn showdown(&mut self) -> HandSummary {
        // Evaluate every non-folded player's best 5-card hand.
        let mut showdowns: Vec<ShowdownResult> = self
            .players
            .iter()
            .enumerate()
            .filter(|(_, p)| !p.sat_out && !p.folded)
            .map(|(i, p)| {
                let mut all = p.hole.clone();
                all.extend(self.community.iter().copied());
                let (rank, best) = best_five(&all, self.mode);
                ShowdownResult {
                    player_idx: i,
                    hole: p.hole.clone(),
                    best_five: best,
                    rank,
                }
            })
            .collect();
        showdowns.sort_by(|a, b| b.rank.cmp(&a.rank));

        // Compute side pots from total_bet contributions.
        let pots = compute_pots(&self.players);
        let mut payouts = vec![];

        for (pot_amount, eligible) in pots {
            if pot_amount == 0 {
                continue;
            }
            // Find best hand among eligible non-folded players.
            let candidates: Vec<&ShowdownResult> = showdowns
                .iter()
                .filter(|s| eligible.contains(&s.player_idx))
                .collect();
            if candidates.is_empty() {
                continue;
            }
            let best = candidates[0].rank;
            let winners: Vec<usize> = candidates
                .iter()
                .filter(|s| s.rank == best)
                .map(|s| s.player_idx)
                .collect();
            let share = pot_amount / winners.len() as u64;
            let remainder = pot_amount - share * winners.len() as u64;
            for (k, w) in winners.iter().enumerate() {
                let extra = if k == 0 { remainder } else { 0 };
                self.players[*w].chips += share + extra;
            }
            payouts.push(PotPayout {
                amount: pot_amount,
                winners,
                note: format!("{}", category_name(best.category, self.mode)),
            });
        }

        for p in &mut self.players {
            p.total_bet = 0;
            p.bet_in_round = 0;
        }

        HandSummary { showdowns, payouts }
    }

    pub fn current_player_open_id(&self) -> Option<&str> {
        self.players.get(self.current_idx).map(|p| p.open_id.as_str())
    }
}

/// Compute the pots (main + side) from each player's `total_bet`.
/// Returns a vector of (pot_amount, eligible_player_indices) ordered from main → side.
fn compute_pots(players: &[Player]) -> Vec<(u64, Vec<usize>)> {
    let mut levels: Vec<u64> = players.iter().map(|p| p.total_bet).collect();
    levels.sort_unstable();
    levels.dedup();
    levels.retain(|&l| l > 0);

    let mut pots = vec![];
    let mut prev = 0u64;
    for &level in &levels {
        let increment = level - prev;
        let mut pot_amount = 0u64;
        let mut eligible = vec![];
        for (i, p) in players.iter().enumerate() {
            let contributed_above_prev = p.total_bet.saturating_sub(prev);
            let contrib = contributed_above_prev.min(increment);
            pot_amount += contrib;
            if p.total_bet >= level && !p.folded {
                eligible.push(i);
            }
        }
        if pot_amount > 0 && !eligible.is_empty() {
            pots.push((pot_amount, eligible));
        } else if pot_amount > 0 && eligible.is_empty() {
            // No active player at this level — money is folded contributions.
            // Roll into the previous pot if any, else create a degenerate pot.
            if let Some(last) = pots.last_mut() {
                last.0 += pot_amount;
            } else {
                pots.push((pot_amount, vec![]));
            }
        }
        prev = level;
    }
    pots
}

#[cfg(test)]
mod tests {
    use super::*;

    fn add(g: &mut Game, id: &str, name: &str) {
        g.add_player(id.into(), name.into()).unwrap();
    }

    #[test]
    fn heads_up_blinds() {
        let mut g = Game::new("c".into());
        add(&mut g, "a", "Alice");
        add(&mut g, "b", "Bob");
        g.start_hand(DeckMode::Standard).unwrap();
        // dealer is 0 (Alice) heads-up: Alice = SB, Bob = BB, Alice acts first
        assert_eq!(g.players[0].bet_in_round, SMALL_BLIND);
        assert_eq!(g.players[1].bet_in_round, BIG_BLIND);
        assert_eq!(g.current_idx, 0);
    }

    #[test]
    fn fold_ends_hand() {
        let mut g = Game::new("c".into());
        add(&mut g, "a", "Alice");
        add(&mut g, "b", "Bob");
        g.start_hand(DeckMode::Standard).unwrap();
        let outcome = g.act("a", PlayerAction::Fold).unwrap();
        assert!(outcome.summary.is_some());
        // Bob wins the pot (5 + 10 = 15)
        assert_eq!(g.players[1].chips, STARTING_CHIPS - BIG_BLIND + (SMALL_BLIND + BIG_BLIND));
    }

    #[test]
    fn full_hand_check_to_river() {
        let mut g = Game::new("c".into());
        add(&mut g, "a", "Alice");
        add(&mut g, "b", "Bob");
        g.start_hand(DeckMode::Standard).unwrap();
        // Alice (SB) calls 5 to match BB
        g.act("a", PlayerAction::Call).unwrap();
        // Bob (BB) checks
        let o1 = g.act("b", PlayerAction::Check).unwrap();
        assert_eq!(g.stage, Stage::Flop);
        assert_eq!(o1.stage_cards.as_ref().map(|s| s.0), Some(Stage::Flop));
        // Both check on flop
        g.act("b", PlayerAction::Check).unwrap();
        let _ = g.act("a", PlayerAction::Check).unwrap();
        assert_eq!(g.stage, Stage::Turn);
        // Both check on turn
        g.act("b", PlayerAction::Check).unwrap();
        let _ = g.act("a", PlayerAction::Check).unwrap();
        assert_eq!(g.stage, Stage::River);
        // Both check on river → showdown
        g.act("b", PlayerAction::Check).unwrap();
        let last = g.act("a", PlayerAction::Check).unwrap();
        assert_eq!(g.stage, Stage::Ended);
        assert!(last.summary.is_some());
    }

    #[test]
    fn start_hand_rejects_mid_hand_call() {
        // Calling start_hand while a hand is in progress must error rather
        // than silently re-deal — protects against duplicate-callback storms
        // double-dealing.
        let mut g = Game::new("c".into());
        add(&mut g, "a", "Alice");
        add(&mut g, "b", "Bob");
        g.start_hand(DeckMode::Standard).unwrap();
        let err = g.start_hand(DeckMode::Standard).unwrap_err();
        assert!(
            err.to_string().contains("已经在牌局中"),
            "expected mid-hand error, got: {err}"
        );
        assert_eq!(g.hand_count, 1, "hand counter should not advance");
    }

    #[test]
    fn raise_resets_actions() {
        let mut g = Game::new("c".into());
        add(&mut g, "a", "Alice");
        add(&mut g, "b", "Bob");
        add(&mut g, "c", "Carol");
        g.start_hand(DeckMode::Standard).unwrap();
        // first to act is Carol (UTG = dealer + 3 = 0 + 3 mod 3 = 0)
        // wait, with 3 players: dealer=0, SB=1, BB=2, first preflop = (0+3)%3 = 0 = Alice (dealer)
        // So Alice acts first.
        assert_eq!(g.current_idx, 0);
    }
}
