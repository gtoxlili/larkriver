use crate::config::Config;
use crate::feishu::cards::*;
use crate::feishu::events::{CardAction, InboundMessage, MemberAdded, Mention};
use crate::feishu::Client as FeishuClient;
use crate::game::*;
use crate::poker::category_name;

use anyhow::{anyhow, Result};
use parking_lot::Mutex;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{info, warn};

pub struct Bot {
    pub client: Arc<FeishuClient>,
    cfg: Config,
    games: Mutex<HashMap<String, Game>>, // chat_id → game
    bot_open_id: Mutex<Option<String>>,
    /// LRU-ish dedup cache for callback `event_id`. Feishu sometimes delivers
    /// the same callback twice (retries, schema-version mirroring) — without
    /// this, the second delivery hits a state that's already advanced and the
    /// user sees a phantom "无法执行" / "还没轮到你" toast on top of the real one.
    seen_events: Mutex<HashMap<String, Instant>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Command {
    Join,
    Leave,
    Start,
    State,
    Chips,
    Reset,
    Help,
}

impl Bot {
    pub fn new(client: Arc<FeishuClient>, cfg: Config) -> Arc<Self> {
        Arc::new(Self {
            client,
            cfg,
            games: Mutex::new(HashMap::new()),
            bot_open_id: Mutex::new(None),
            seen_events: Mutex::new(HashMap::new()),
        })
    }

    /// Returns true if `event_id` has been seen in the last ~2 minutes —
    /// callers should drop the event in that case. Empty `event_id`
    /// (legacy payloads without one) is never deduped.
    fn is_duplicate_event(&self, event_id: &str) -> bool {
        if event_id.is_empty() {
            return false;
        }
        let mut seen = self.seen_events.lock();
        // Cheap GC: drop entries older than 120s on every insert.
        seen.retain(|_, t| t.elapsed() < Duration::from_secs(120));
        if seen.contains_key(event_id) {
            true
        } else {
            seen.insert(event_id.to_string(), Instant::now());
            false
        }
    }

    pub fn cfg(&self) -> &Config {
        &self.cfg
    }

    /// Set once on startup so we can identify mentions of ourselves.
    pub fn set_bot_open_id(&self, open_id: String) {
        *self.bot_open_id.lock() = Some(open_id);
    }

    fn bot_open_id_clone(&self) -> Option<String> {
        self.bot_open_id.lock().clone()
    }

    pub async fn handle_message(self: Arc<Self>, msg: InboundMessage) -> Result<()> {
        if self.is_duplicate_event(&msg.event_id) {
            return Ok(());
        }
        if msg.message_type != "text" {
            return Ok(());
        }
        if let Some(allowed) = &self.cfg.allowed_chat_id {
            if &msg.chat_id != allowed && msg.chat_type != "p2p" {
                return Ok(());
            }
        }

        let bot_oid = self.bot_open_id_clone().unwrap_or_default();
        let cmd = parse_command(&msg.text, &msg.mentions, &bot_oid, msg.chat_type == "p2p");

        let Some(cmd) = cmd else {
            return Ok(());
        };

        info!(?cmd, chat = %msg.chat_id, sender = %msg.sender_open_id, "command");
        if let Err(e) = self.dispatch_command(cmd, &msg).await {
            // Error feedback goes only to the user who sent the command — no need
            // to clutter the group with a public reply.
            let c = card(header("⚠️", "red"), vec![div_md(&format!("{e}"))]);
            let _ = self.send_user_only(&msg, &c).await;
        }
        Ok(())
    }

    async fn dispatch_command(&self, cmd: Command, msg: &InboundMessage) -> Result<()> {
        match cmd {
            Command::Help => self.send_help(msg).await,
            Command::Join => self.cmd_join(msg).await,
            Command::Leave => self.cmd_leave(msg).await,
            Command::Start => self.cmd_start(msg).await,
            Command::State => self.cmd_state(msg).await,
            Command::Chips => self.cmd_chips(msg).await,
            Command::Reset => self.cmd_reset(msg).await,
        }
    }

    /// Send a card visible only to `msg.sender_open_id`. In a group chat that's
    /// an ephemeral message (others can't see it); in a 1-on-1 chat with the
    /// bot it falls back to a regular message (the chat is already private).
    async fn send_user_only(&self, msg: &InboundMessage, card: &Value) -> Result<String> {
        if msg.chat_type == "p2p" {
            self.client
                .send_message("chat_id", &msg.chat_id, "interactive", card)
                .await
        } else {
            self.client
                .send_ephemeral_card(&msg.chat_id, &msg.sender_open_id, card)
                .await
        }
    }

    async fn send_help(&self, msg: &InboundMessage) -> Result<()> {
        let c = card(
            header("德州扑克 帮助", "blue"),
            vec![
                div_md(
                    "**操作方式**：通过卡片按钮，或在群里 @机器人 + 关键词\n\n\
                     • `join` 加入下一局\n\
                     • `leave` 离开\n\
                     • `start` 开局 (≥2 名玩家)\n\
                     • `state` 当前状态\n\
                     • `chips` 各玩家筹码\n\
                     • `reset` 重置牌桌\n\n\
                     游戏内行动均为卡片按钮：弃牌 / 跟注 / 加注 / 全押。",
                ),
                note_md("初始筹码 1000 · 小盲 5 / 大盲 10 · 手牌以**仅本人可见**的群消息发出"),
            ],
        );
        self.send_user_only(msg, &c).await?;
        Ok(())
    }

    async fn cmd_join(&self, msg: &InboundMessage) -> Result<()> {
        let name = self
            .client
            .user_name(&msg.sender_open_id)
            .await
            .unwrap_or_else(|_| "玩家".into());
        self.do_join(&msg.chat_id, &msg.sender_open_id, &name).await
    }

    async fn cmd_leave(&self, msg: &InboundMessage) -> Result<()> {
        self.do_leave(&msg.chat_id, &msg.sender_open_id).await
    }

    async fn cmd_reset(&self, msg: &InboundMessage) -> Result<()> {
        self.do_reset(&msg.chat_id).await
    }

    async fn do_join(&self, chat_id: &str, open_id: &str, name: &str) -> Result<()> {
        {
            let mut games = self.games.lock();
            let game = games
                .entry(chat_id.to_string())
                .or_insert_with(|| Game::new(chat_id.to_string()));
            game.add_player(open_id.to_string(), name.to_string())?;
        }
        self.refresh_lobby(chat_id).await
    }

    async fn do_leave(&self, chat_id: &str, open_id: &str) -> Result<()> {
        {
            let mut games = self.games.lock();
            let game = games
                .get_mut(chat_id)
                .ok_or_else(|| anyhow!("当前没有牌局"))?;
            game.remove_player(open_id)?;
        }
        self.refresh_lobby(chat_id).await
    }

    async fn do_reset(&self, chat_id: &str) -> Result<()> {
        {
            self.games.lock().remove(chat_id);
        }
        // After reset, post a fresh lobby card to invite players in.
        let _ = self.refresh_lobby(chat_id).await;
        let c = card(
            header("♻️ 牌桌已重置", "wathet"),
            vec![div_md("点击下方大厅卡片的 **加入** 按钮重新开始。")],
        );
        self.client
            .send_message("chat_id", chat_id, "interactive", &c)
            .await?;
        Ok(())
    }

    /// Post or update the persistent lobby card in `chat_id`. Idempotent — safe to call
    /// after every state change. If the card for this chat doesn't exist yet, posts a
    /// new one and stores its message_id for future in-place updates.
    async fn refresh_lobby(&self, chat_id: &str) -> Result<()> {
        let (card_value, existing_msg_id) = {
            let games = self.games.lock();
            let Some(game) = games.get(chat_id) else { return Ok(()); };
            let snap = snapshot(game);
            (build_lobby_card(&snap), game.lobby_msg_id.clone())
        };

        if let Some(msg_id) = existing_msg_id {
            if self.client.update_card(&msg_id, &card_value).await.is_ok() {
                return Ok(());
            }
            // fall through to posting a new card if update failed (card was deleted, etc.)
        }
        let new_id = self
            .client
            .send_message("chat_id", chat_id, "interactive", &card_value)
            .await?;
        if let Some(g) = self.games.lock().get_mut(chat_id) {
            g.lobby_msg_id = Some(new_id);
        }
        Ok(())
    }

    async fn cmd_chips(&self, msg: &InboundMessage) -> Result<()> {
        let body = {
            let games = self.games.lock();
            let game = games
                .get(&msg.chat_id)
                .ok_or_else(|| anyhow!("当前没有牌局"))?;
            if game.players.is_empty() {
                return Err(anyhow!("还没有玩家加入"));
            }
            game.players
                .iter()
                .map(|p| format!("• {} — **{}** 筹码", at(&p.open_id), p.chips))
                .collect::<Vec<_>>()
                .join("\n")
        };
        let c = card(header("筹码", "blue"), vec![div_md(&body)]);
        self.send_user_only(msg, &c).await?;
        Ok(())
    }

    async fn cmd_state(&self, msg: &InboundMessage) -> Result<()> {
        let snap = {
            let games = self.games.lock();
            games
                .get(&msg.chat_id)
                .map(|g| snapshot_for(g, Some(msg.sender_open_id.as_str())))
        }
        .ok_or_else(|| anyhow!("当前没有牌局"))?;
        // If the requester happens to be the current actor, hand them buttons too.
        let am_actor = snap.current_open_id.as_deref() == Some(msg.sender_open_id.as_str());
        let c = build_state_card(&snap, am_actor);
        self.send_user_only(msg, &c).await?;
        Ok(())
    }

    async fn cmd_start(&self, msg: &InboundMessage) -> Result<()> {
        self.do_start(&msg.chat_id).await
    }

    async fn do_start(&self, chat_id: &str) -> Result<()> {
        let (snap, hole_cards) = {
            let mut games = self.games.lock();
            let game = games
                .get_mut(chat_id)
                .ok_or_else(|| anyhow!("当前没有牌局, 先 join"))?;
            game.start_hand()?;
            let snap = snapshot(game);
            let hole: Vec<(String, String, Vec<crate::poker::Card>)> = game
                .players
                .iter()
                .filter(|p| !p.sat_out)
                .map(|p| (p.open_id.clone(), p.name.clone(), p.hole.clone()))
                .collect();
            (snap, hole)
        };

        // Update the lobby card to "in progress" state (no buttons).
        let _ = self.refresh_lobby(chat_id).await;

        // Send each player their hole cards as ephemeral group messages —
        // visible only to that user, no DM required.
        for (open_id, _name, hole) in hole_cards {
            let c = card(
                header_with_subtitle(
                    "🂠 你的手牌",
                    &format!("第 {} 局", snap.hand_count),
                    "purple",
                ),
                vec![
                    cards_row(&hole),
                    note("仅你可见 · 群里其他人看不到"),
                ],
            );
            if let Err(e) = self
                .client
                .send_ephemeral_card(chat_id, &open_id, &c)
                .await
            {
                warn!(?e, %open_id, "failed to deliver ephemeral hole cards");
            }
        }

        // Public hand-start announcement so non-actors know the hand began,
        // who's first, and what the blinds are. No buttons here.
        let _ = self
            .client
            .send_message(
                "chat_id",
                chat_id,
                "interactive",
                &build_hand_start_card(&snap),
            )
            .await;

        // Ephemeral state+buttons sent only to the first actor.
        self.post_actor_prompt(chat_id).await?;
        Ok(())
    }

    /// New user(s) added to the group → send each one an ephemeral welcome card
    /// with a [加入下一局] button.
    pub async fn handle_member_added(self: Arc<Self>, evt: MemberAdded) -> Result<()> {
        if self.is_duplicate_event(&evt.event_id) {
            return Ok(());
        }
        if let Some(allowed) = &self.cfg.allowed_chat_id {
            if &evt.chat_id != allowed {
                return Ok(());
            }
        }
        for user in evt.users {
            let c = build_welcome_card(&evt.chat_id, &user.name);
            if let Err(e) = self
                .client
                .send_ephemeral_card(&evt.chat_id, &user.open_id, &c)
                .await
            {
                warn!(?e, open_id = %user.open_id, "failed to send welcome ephemeral");
            }
        }
        Ok(())
    }

    pub async fn handle_card_action(self: Arc<Self>, action: CardAction) -> Result<Value> {
        if self.is_duplicate_event(&action.event_id) {
            return Ok(json!({}));
        }
        let Some(action_id) = action
            .value
            .get("action")
            .and_then(|v| v.as_str())
            .map(String::from)
        else {
            return Ok(json!({}));
        };
        let chat_id = action
            .value
            .get("chat_id")
            .and_then(|v| v.as_str())
            .map(String::from)
            .unwrap_or(action.open_chat_id.clone());

        // Lobby buttons: anyone in the chat can click. Spawn the work and
        // return immediately so the webhook responds inside Feishu's timeout.
        if matches!(
            action_id.as_str(),
            "join_lobby" | "leave_lobby" | "start_lobby"
        ) {
            let bot = self.clone();
            let aid = action_id.clone();
            let oid = action.open_id.clone();
            let cid = chat_id.clone();
            tokio::spawn(async move {
                let res = match aid.as_str() {
                    "join_lobby" => {
                        let name = bot
                            .client
                            .user_name(&oid)
                            .await
                            .unwrap_or_else(|_| "玩家".into());
                        bot.do_join(&cid, &oid, &name).await
                    }
                    "leave_lobby" => bot.do_leave(&cid, &oid).await,
                    "start_lobby" => bot.do_start(&cid).await,
                    _ => Ok(()),
                };
                if let Err(e) = res {
                    let c = card(
                        header("⚠️ 无法执行", "red"),
                        vec![div_md(&format!("{e}"))],
                    );
                    let _ = bot.client.send_ephemeral_card(&cid, &oid, &c).await;
                }
            });
            return Ok(json!({}));
        }

        // Game-action buttons (fold/check/call/raise/allin) need stale-click +
        // actor-authorization checks before mutating state.
        let actor_id = action
            .value
            .get("actor")
            .and_then(|v| v.as_str())
            .map(String::from)
            .unwrap_or_default();
        let hand = action.value.get("hand").and_then(|v| v.as_u64()).unwrap_or(0);

        // Stale-click guards. Run *before* the actor check so re-clicking an
        // old ephemeral card surfaces a clear "this card is stale" toast
        // rather than "现在不是你的回合", which reads as authorization failure.
        {
            let games = self.games.lock();
            if let Some(g) = games.get(&chat_id) {
                if hand != 0 && hand as u32 != g.hand_count {
                    return Ok(toast("这是上一局的按钮"));
                }
                if !actor_id.is_empty() {
                    match g.stage {
                        Stage::PreFlop | Stage::Flop | Stage::Turn | Stage::River => {
                            if let Some(current) = g.current_player_open_id() {
                                if current != actor_id {
                                    return Ok(toast(
                                        "这张卡片已失效 · 最新行动卡在下方",
                                    ));
                                }
                            }
                        }
                        Stage::Lobby | Stage::Ended | Stage::Showdown => {
                            return Ok(toast(
                                "本局已结束 · 点大厅卡 [开局] 开始新一局",
                            ));
                        }
                    }
                }
            }
        }

        // Authorization: someone other than the rendered actor clicked a
        // currently-valid card.
        if !actor_id.is_empty() && action.open_id != actor_id {
            return Ok(toast("现在不是你的回合"));
        }

        let player_action = match action_id.as_str() {
            "fold" => PlayerAction::Fold,
            "check" => PlayerAction::Check,
            "call" => PlayerAction::Call,
            "allin" => PlayerAction::AllIn,
            "raise" => {
                let to = action
                    .value
                    .get("to")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                if to == 0 {
                    return Ok(toast("加注金额无效"));
                }
                PlayerAction::RaiseTo(to)
            }
            "raise_custom" => {
                // Form-submit button — value carries the action id, the typed
                // amount lives in form_value.raise_to (a string).
                let raw = action
                    .form_value
                    .get("raise_to")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim();
                let to: u64 = match raw.parse() {
                    Ok(n) if n > 0 => n,
                    _ => return Ok(toast("请输入有效的加注金额")),
                };
                PlayerAction::RaiseTo(to)
            }
            _ => return Ok(json!({})),
        };

        // Apply action synchronously (fast); collect outcome+snapshot.
        let result = {
            let mut games = self.games.lock();
            let g = games
                .get_mut(&chat_id)
                .ok_or_else(|| anyhow!("game missing"))?;
            match g.act(&action.open_id, player_action) {
                Ok(outcome) => Ok((outcome, snapshot(g))),
                Err(e) => Err(e),
            }
        };

        let (outcome, snap) = match result {
            Ok(v) => v,
            Err(e) => return Ok(toast(&format!("{e}"))),
        };

        // Spawn the message-posting so we respond fast.
        let bot = self.clone();
        let cid = chat_id.clone();
        tokio::spawn(async move {
            bot.post_action_outcome(&cid, snap, outcome).await;
        });
        Ok(json!({}))
    }

    async fn post_action_outcome(&self, chat_id: &str, snap: GameSnapshot, outcome: ActOutcome) {
        // Public announcement: who did what + post-action state + next actor.
        // Carries enough info that non-actors can follow without seeing the
        // ephemeral state card.
        let _ = self
            .client
            .send_message(
                "chat_id",
                chat_id,
                "interactive",
                &build_action_announcement(&snap, &outcome.log),
            )
            .await;

        if let Some((stage, cards)) = &outcome.stage_cards {
            let _ = self.post_stage_card(chat_id, *stage, cards, &snap).await;
        }
        for (stage, cards) in &outcome.extra_stages {
            let _ = self.post_stage_card(chat_id, *stage, cards, &snap).await;
        }

        if let Some(summary) = outcome.summary {
            let _ = self.post_summary(chat_id, &snap, &summary).await;
            // Hand ended. The previous lobby card is now buried under action /
            // stage / summary cards, so drop its id and post a fresh one so the
            // next-hand buttons are visible at the bottom of the chat.
            if let Some(g) = self.games.lock().get_mut(chat_id) {
                g.lobby_msg_id = None;
            }
            let _ = self.refresh_lobby(chat_id).await;
        } else if outcome.next_actor_open_id.is_some() {
            // Buttons go only to the player whose turn it is.
            let _ = self.post_actor_prompt(chat_id).await;
        }
    }

    /// Send the action card (full state + buttons) only to the player whose
    /// turn it is, as an ephemeral group message. The snapshot used here is
    /// rebuilt under the lock so it carries the actor's hole cards — the
    /// caller's snapshot might not have them.
    async fn post_actor_prompt(&self, chat_id: &str) -> Result<()> {
        let payload = {
            let games = self.games.lock();
            let Some(g) = games.get(chat_id) else { return Ok(()); };
            let Some(actor_id) = g.current_player_open_id().map(String::from) else {
                return Ok(());
            };
            let snap = snapshot_for(g, Some(&actor_id));
            Some((build_state_card(&snap, true), actor_id))
        };
        if let Some((card, actor_id)) = payload {
            self.client
                .send_ephemeral_card(chat_id, &actor_id, &card)
                .await?;
        }
        Ok(())
    }

    async fn post_stage_card(
        &self,
        chat_id: &str,
        stage: Stage,
        new_cards: &[crate::poker::Card],
        snap: &GameSnapshot,
    ) -> Result<()> {
        let template = match stage {
            Stage::Flop => "indigo",
            Stage::Turn => "violet",
            Stage::River => "carmine",
            _ => "blue",
        };
        let mut elements = vec![
            markdown("**新增公共牌**"),
            cards_row(new_cards),
        ];
        if snap.community.len() > new_cards.len() {
            elements.push(markdown("**全部公共牌**"));
            elements.push(cards_row(&snap.community));
        }
        let c = card(
            header_with_subtitle(
                &format!("🂠 {}", stage.label()),
                &format!("底池 {}", snap.pot),
                template,
            ),
            elements,
        );
        self.client
            .send_message("chat_id", chat_id, "interactive", &c)
            .await?;
        Ok(())
    }

    async fn post_summary(
        &self,
        chat_id: &str,
        snap: &GameSnapshot,
        summary: &HandSummary,
    ) -> Result<()> {
        // (Display strings switched to at-mentions below — Feishu renders the
        // user's display name from `<at id=...>` without the bot needing the
        // contact:user.base:readonly scope.)
        let mut elements = vec![];

        if !summary.showdowns.is_empty() {
            elements.push(markdown("**公共牌**"));
            elements.push(cards_row(&snap.community));
            elements.push(hr());

            for s in &summary.showdowns {
                let p = &snap.players[s.player_idx];
                elements.push(markdown(&format!(
                    "{} · {}",
                    at(&p.open_id),
                    category_name(s.rank.category)
                )));
                elements.push(markdown("手牌"));
                elements.push(cards_row(&s.hole));
                elements.push(markdown("最佳五张"));
                elements.push(cards_row(&s.best_five));
                elements.push(hr());
            }
        }

        for (k, payout) in summary.payouts.iter().enumerate() {
            let pot_label = if k == 0 {
                "💰 主池".to_string()
            } else {
                format!("💰 边池 #{k}")
            };
            let winner_names = payout
                .winners
                .iter()
                .map(|i| at(&snap.players[*i].open_id))
                .collect::<Vec<_>>()
                .join("、");
            let line = if payout.winners.is_empty() {
                format!("{} **{}** 筹码 → 无人领取", pot_label, payout.amount)
            } else {
                format!(
                    "{} **{}** 筹码 → {} ({})",
                    pot_label, payout.amount, winner_names, payout.note
                )
            };
            elements.push(markdown(&line));
        }

        let chips_line = snap
            .players
            .iter()
            .map(|p| format!("{}: {}", at(&p.open_id), p.chips))
            .collect::<Vec<_>>()
            .join(" · ");
        elements.push(note(&format!(
            "筹码 — {chips_line}\n点大厅卡片 **开局** 进入下一局"
        )));

        let c = card(
            header_with_subtitle(
                "🏆 牌局结束",
                &format!("第 {} 局", snap.hand_count),
                "turquoise",
            ),
            elements,
        );
        self.client
            .send_message("chat_id", chat_id, "interactive", &c)
            .await?;
        Ok(())
    }

    /// One-off helper: send a mock of every card type to `chat_id` so the
    /// shapes can be eyeballed in a real client. `recipient_open_id` receives
    /// the ephemeral ones (welcome / hole / actor prompt / help / chips / error).
    pub async fn send_all_mocks(
        &self,
        chat_id: &str,
        recipient_open_id: &str,
    ) -> Result<()> {
        use crate::poker::{best_five, Card, Rank, Suit};

        let cli = &self.client;
        let alice = recipient_open_id.to_string();
        // Feishu's card validator rejects unknown open_ids inside <at> tags or
        // person_list. Use the bot's own open_id as the "second player" so
        // every reference is valid; the at-mention will render as the bot's
        // display name, which is fine for a visual layout check.
        let bob = self
            .bot_open_id_clone()
            .unwrap_or_else(|| alice.clone());

        let send_label = |idx: u32, name: &str| -> Value {
            card(
                header(&format!("🧪 Mock #{}", idx), "grey"),
                vec![markdown(&format!("**{name}**"))],
            )
        };

        let mk = |id: &str, name: &str, chips: u64, bet: u64| PlayerSnapshot {
            open_id: id.into(),
            name: name.into(),
            chips,
            bet_in_round: bet,
            folded: false,
            all_in: false,
            sat_out: false,
        };
        let c = |r: u8, s: Suit| Card { rank: Rank(r), suit: s };

        let mut idx = 0u32;
        let mut step =
            |_n: &str| -> u32 { idx += 1; idx };

        // --------- 1. help (ephemeral) ---------
        let n = step("help");
        cli.send_message("chat_id", chat_id, "interactive",
            &send_label(n, "/poker help (仅请求者可见)")).await?;
        let help_card = card(
            header("德州扑克 帮助", "blue"),
            vec![
                markdown(
                    "**操作方式**：通过卡片按钮，或在群里 @机器人 + 关键词\n\n\
                     • `join` 加入下一局\n\
                     • `leave` 离开\n\
                     • `start` 开局 (≥2 名玩家)\n\
                     • `state` 当前状态\n\
                     • `chips` 各玩家筹码\n\
                     • `reset` 重置牌桌\n\n\
                     游戏内行动均为卡片按钮：弃牌 / 跟注 / 加注 / 全押。",
                ),
                note("初始筹码 1000 · 小盲 5 / 大盲 10 · 手牌以**仅本人可见**的群消息发出"),
            ],
        );
        cli.send_ephemeral_card(chat_id, &alice, &help_card).await?;

        // --------- 2. welcome (ephemeral) ---------
        let n = step("welcome");
        cli.send_message("chat_id", chat_id, "interactive",
            &send_label(n, "新成员入群 - 欢迎卡 (仅他可见)")).await?;
        cli.send_ephemeral_card(chat_id, &alice,
            &build_welcome_card(chat_id, "Alice")).await?;

        // --------- 3. lobby empty ---------
        let n = step("lobby empty");
        cli.send_message("chat_id", chat_id, "interactive",
            &send_label(n, "大厅 - 空桌")).await?;
        let snap_empty = GameSnapshot {
            chat_id: chat_id.into(),
            stage: Stage::Lobby,
            hand_count: 0,
            community: vec![],
            pot: 0, current_bet: 0, min_raise: 10, big_blind: 10,
            dealer_idx: 0, current_open_id: None, players: vec![],
            viewer_hole: vec![],
        };
        cli.send_message("chat_id", chat_id, "interactive",
            &build_lobby_card(&snap_empty)).await?;

        // --------- 4. lobby with 2 waiting ---------
        let n = step("lobby waiting");
        cli.send_message("chat_id", chat_id, "interactive",
            &send_label(n, "大厅 - 两位玩家就座 (可点开局)")).await?;
        let snap_lobby2 = GameSnapshot {
            chat_id: chat_id.into(),
            stage: Stage::Lobby,
            hand_count: 0,
            community: vec![],
            pot: 0, current_bet: 0, min_raise: 10, big_blind: 10,
            dealer_idx: 0, current_open_id: None,
            players: vec![mk(&alice, "Alice", 1000, 0), mk(&bob, "Bob", 1000, 0)],
            viewer_hole: vec![],
        };
        cli.send_message("chat_id", chat_id, "interactive",
            &build_lobby_card(&snap_lobby2)).await?;

        // --------- 5. lobby in-progress ---------
        let n = step("lobby in-progress");
        cli.send_message("chat_id", chat_id, "interactive",
            &send_label(n, "大厅 - 牌局进行中 (无按钮)")).await?;
        let snap_lobby_inprog = GameSnapshot {
            chat_id: chat_id.into(),
            stage: Stage::Flop,
            hand_count: 1,
            community: vec![c(10, Suit::Hearts), c(11, Suit::Hearts), c(7, Suit::Clubs)],
            pot: 60, current_bet: 0, min_raise: 10, big_blind: 10,
            dealer_idx: 0, current_open_id: Some(alice.clone()),
            players: vec![mk(&alice, "Alice", 970, 0), mk(&bob, "Bob", 970, 0)],
            viewer_hole: vec![],
        };
        cli.send_message("chat_id", chat_id, "interactive",
            &build_lobby_card(&snap_lobby_inprog)).await?;

        // --------- 6. hand start ---------
        let n = step("hand start");
        cli.send_message("chat_id", chat_id, "interactive",
            &send_label(n, "牌局开始公告")).await?;
        let snap_start = GameSnapshot {
            chat_id: chat_id.into(),
            stage: Stage::PreFlop,
            hand_count: 1,
            community: vec![],
            pot: 15, current_bet: 10, min_raise: 10, big_blind: 10,
            dealer_idx: 0, current_open_id: Some(alice.clone()),
            players: vec![
                mk(&alice, "Alice", 995, 5),  // SB
                mk(&bob, "Bob", 990, 10),     // BB
            ],
            viewer_hole: vec![],
        };
        cli.send_message("chat_id", chat_id, "interactive",
            &build_hand_start_card(&snap_start)).await?;

        // --------- 7. hole cards (ephemeral) ---------
        let n = step("hole cards");
        cli.send_message("chat_id", chat_id, "interactive",
            &send_label(n, "手牌 (私人临时消息，仅你可见)")).await?;
        let hole = vec![c(14, Suit::Spades), c(13, Suit::Spades)];
        let hole_card = card(
            header_with_subtitle("🂠 你的手牌", "第 1 局", "purple"),
            vec![cards_row(&hole), note("仅你可见 · 群里其他人看不到")],
        );
        cli.send_ephemeral_card(chat_id, &alice, &hole_card).await?;

        // Build mid-flop snapshot used by several mocks below
        let snap_mid_flop = GameSnapshot {
            chat_id: chat_id.into(),
            stage: Stage::Flop,
            hand_count: 1,
            community: vec![c(10, Suit::Hearts), c(11, Suit::Hearts), c(7, Suit::Clubs)],
            pot: 60, current_bet: 20, min_raise: 20, big_blind: 10,
            dealer_idx: 0, current_open_id: Some(alice.clone()),
            players: vec![
                mk(&alice, "Alice", 970, 0),
                mk(&bob, "Bob", 950, 20),
            ],
            // Hole cards for the actor (Alice) so the mock action card shows
            // the "你的手牌" row.
            viewer_hole: vec![c(14, Suit::Spades), c(13, Suit::Spades)],
        };

        // --------- 8. state - public ---------
        let n = step("state public");
        cli.send_message("chat_id", chat_id, "interactive",
            &send_label(n, "状态卡 - 公开版 (无按钮)")).await?;
        cli.send_message("chat_id", chat_id, "interactive",
            &build_state_card(&snap_mid_flop, false)).await?;

        // --------- 9. state - private with form + buttons ---------
        let n = step("state actor");
        cli.send_message("chat_id", chat_id, "interactive",
            &send_label(n, "行动卡 - 私人版 (form + input + 按钮)")).await?;
        cli.send_ephemeral_card(chat_id, &alice,
            &build_state_card(&snap_mid_flop, true)).await?;

        // --------- 10. action announcement ---------
        let n = step("action announce");
        cli.send_message("chat_id", chat_id, "interactive",
            &send_label(n, "行动公告 - 加注后 (公开)")).await?;
        let log = ActionLogEntry { player_idx: 0, kind: ActionKind::Raise, amount: 60 };
        let mut snap_after = snap_mid_flop.clone();
        snap_after.players[0].bet_in_round = 60;
        snap_after.players[0].chips = 910;
        snap_after.pot = 120;
        snap_after.current_bet = 60;
        snap_after.current_open_id = Some(bob.clone());
        cli.send_message("chat_id", chat_id, "interactive",
            &build_action_announcement(&snap_after, &log)).await?;

        // --------- 11/12/13. stage cards ---------
        let n = step("flop");
        cli.send_message("chat_id", chat_id, "interactive",
            &send_label(n, "翻牌 (Flop) - 公共牌方块")).await?;
        let flop = vec![c(10, Suit::Hearts), c(11, Suit::Hearts), c(7, Suit::Clubs)];
        self.post_stage_card(chat_id, Stage::Flop, &flop, &snap_mid_flop).await?;

        let n = step("turn");
        cli.send_message("chat_id", chat_id, "interactive",
            &send_label(n, "转牌 (Turn)")).await?;
        let mut snap_turn = snap_mid_flop.clone();
        snap_turn.stage = Stage::Turn;
        snap_turn.community.push(c(13, Suit::Hearts));
        let turn = vec![c(13, Suit::Hearts)];
        self.post_stage_card(chat_id, Stage::Turn, &turn, &snap_turn).await?;

        let n = step("river");
        cli.send_message("chat_id", chat_id, "interactive",
            &send_label(n, "河牌 (River)")).await?;
        let mut snap_river = snap_turn.clone();
        snap_river.stage = Stage::River;
        snap_river.community.push(c(2, Suit::Diamonds));
        let river = vec![c(2, Suit::Diamonds)];
        self.post_stage_card(chat_id, Stage::River, &river, &snap_river).await?;

        // --------- 14. summary (showdown) ---------
        let n = step("summary");
        cli.send_message("chat_id", chat_id, "interactive",
            &send_label(n, "摊牌总结")).await?;
        let alice_hole = vec![c(14, Suit::Spades), c(13, Suit::Spades)];
        let bob_hole = vec![c(12, Suit::Hearts), c(12, Suit::Diamonds)];
        let community = snap_river.community.clone();
        let mut alice7 = alice_hole.clone();
        alice7.extend(community.iter().copied());
        let (alice_rank, alice_best) = best_five(&alice7);
        let mut bob7 = bob_hole.clone();
        bob7.extend(community.iter().copied());
        let (bob_rank, bob_best) = best_five(&bob7);
        let winner_idx = if alice_rank > bob_rank { 0 } else { 1 };
        let winning_rank = if alice_rank > bob_rank { alice_rank } else { bob_rank };
        let summary = HandSummary {
            showdowns: vec![
                ShowdownResult { player_idx: 0, hole: alice_hole, best_five: alice_best, rank: alice_rank },
                ShowdownResult { player_idx: 1, hole: bob_hole, best_five: bob_best, rank: bob_rank },
            ],
            payouts: vec![PotPayout {
                amount: 120,
                winners: vec![winner_idx],
                note: crate::poker::category_name(winning_rank.category).to_string(),
            }],
        };
        let mut snap_show = snap_river.clone();
        snap_show.stage = Stage::Showdown;
        snap_show.pot = 120;
        snap_show.players[0].chips = 910;
        snap_show.players[1].chips = 910;
        if winner_idx == 0 { snap_show.players[0].chips += 120; }
        else { snap_show.players[1].chips += 120; }
        self.post_summary(chat_id, &snap_show, &summary).await?;

        // --------- 15. chips (ephemeral) ---------
        let n = step("chips");
        cli.send_message("chat_id", chat_id, "interactive",
            &send_label(n, "/poker chips (仅请求者可见)")).await?;
        let chips_body = format!(
            "• {} (Alice) — **970** 筹码\n• {} (Bob) — **1030** 筹码",
            at(&alice), at(&bob),
        );
        let chips_card = card(header("筹码", "blue"), vec![markdown(&chips_body)]);
        cli.send_ephemeral_card(chat_id, &alice, &chips_card).await?;

        // --------- 16. reset ---------
        let n = step("reset");
        cli.send_message("chat_id", chat_id, "interactive",
            &send_label(n, "/poker reset 通知 (公开)")).await?;
        let reset_card = card(
            header("♻️ 牌桌已重置", "wathet"),
            vec![markdown("点击下方大厅卡片的 **加入** 按钮重新开始。")],
        );
        cli.send_message("chat_id", chat_id, "interactive", &reset_card).await?;

        // --------- 17. error feedback ---------
        let n = step("error");
        cli.send_message("chat_id", chat_id, "interactive",
            &send_label(n, "错误反馈 (仅报错者可见)")).await?;
        let err_card = card(
            header("⚠️", "red"),
            vec![markdown("当前没有牌局")],
        );
        cli.send_ephemeral_card(chat_id, &alice, &err_card).await?;

        Ok(())
    }
}

/// Toast-only response for the card endpoint.
/// Visual representation of one playing card as a coloured tile.
/// ♥/♦ get a red background with white text; ♠/♣ get grey background with
/// the default near-black text — both readable, clearly suit-coded.
fn card_tile(c: crate::poker::Card) -> Value {
    use crate::poker::Suit;
    let label = format!("**{}{}**", c.rank.label(), c.suit.symbol());
    let (bg, body) = match c.suit {
        Suit::Hearts | Suit::Diamonds => ("red", heading_md_colored(&label, "white")),
        _ => ("grey", heading_md(&label)),
    };
    tile_column(vec![body], 1, bg)
}

/// Lay out a row of card tiles. Returns a markdown placeholder for the empty case.
fn cards_row(cards: &[crate::poker::Card]) -> Value {
    if cards.is_empty() {
        return markdown("—");
    }
    let cols: Vec<Value> = cards.iter().map(|c| card_tile(*c)).collect();
    column_set(cols)
}

fn toast(msg: &str) -> Value {
    json!({
        "toast": {
            "type": "warning",
            "content": msg,
        }
    })
}

/// Snapshot of a game's public state (no hidden cards) for rendering.
#[derive(Debug, Clone)]
pub struct GameSnapshot {
    pub chat_id: String,
    pub stage: Stage,
    pub hand_count: u32,
    pub community: Vec<crate::poker::Card>,
    pub pot: u64,
    pub current_bet: u64,
    pub min_raise: u64,
    pub big_blind: u64,
    pub dealer_idx: usize,
    pub current_open_id: Option<String>,
    pub players: Vec<PlayerSnapshot>,
    /// Hole cards of the user this snapshot is being rendered for. Empty
    /// unless the snapshot is being shown to a specific player (e.g. their
    /// own action prompt or a `/poker state` reply).
    pub viewer_hole: Vec<crate::poker::Card>,
}

#[derive(Debug, Clone)]
pub struct PlayerSnapshot {
    pub open_id: String,
    pub name: String,
    pub chips: u64,
    pub bet_in_round: u64,
    pub folded: bool,
    pub all_in: bool,
    pub sat_out: bool,
}

fn snapshot(g: &Game) -> GameSnapshot {
    snapshot_for(g, None)
}

/// Same as `snapshot` but also captures the named viewer's hole cards so
/// they can be rendered into an ephemeral card meant only for that user.
fn snapshot_for(g: &Game, viewer_open_id: Option<&str>) -> GameSnapshot {
    let viewer_hole = viewer_open_id
        .and_then(|id| g.players.iter().find(|p| p.open_id == id))
        .map(|p| p.hole.clone())
        .unwrap_or_default();
    GameSnapshot {
        chat_id: g.chat_id.clone(),
        stage: g.stage,
        hand_count: g.hand_count,
        community: g.community.clone(),
        pot: g.pot_total(),
        current_bet: g.current_bet,
        min_raise: g.min_raise,
        big_blind: g.big_blind,
        dealer_idx: g.dealer_idx,
        current_open_id: g.current_player_open_id().map(String::from),
        players: g
            .players
            .iter()
            .map(|p| PlayerSnapshot {
                open_id: p.open_id.clone(),
                name: p.name.clone(),
                chips: p.chips,
                bet_in_round: p.bet_in_round,
                folded: p.folded,
                all_in: p.all_in,
                sat_out: p.sat_out,
            })
            .collect(),
        viewer_hole,
    }
}

fn parse_command(
    text: &str,
    mentions: &[Mention],
    bot_open_id: &str,
    is_p2p: bool,
) -> Option<Command> {
    let mut cleaned = text.to_string();
    let mut mentioned_bot = is_p2p;
    for m in mentions {
        if !bot_open_id.is_empty() && m.open_id == bot_open_id {
            mentioned_bot = true;
        }
        cleaned = cleaned.replace(&m.key, "");
    }
    let body_str: String = if cleaned.trim_start().starts_with("/poker") {
        cleaned.trim_start().trim_start_matches("/poker").trim().to_string()
    } else if mentioned_bot {
        cleaned.trim().to_string()
    } else {
        return None;
    };
    let body = body_str.to_lowercase();
    let mut parts = body.split_whitespace();
    let cmd = parts.next()?;
    Some(match cmd {
        "join" | "加入" => Command::Join,
        "leave" | "离开" => Command::Leave,
        "start" | "begin" | "go" | "开始" => Command::Start,
        "state" | "status" | "状态" => Command::State,
        "chips" | "stack" | "筹码" => Command::Chips,
        "reset" | "重置" => Command::Reset,
        "help" | "帮助" | "?" => Command::Help,
        _ => return None,
    })
}

/// One-shot welcome shown only to a user who just joined the group. The button
/// reuses the same `join_lobby` action as the persistent lobby card, so the
/// click flows through the existing handler.
fn build_welcome_card(chat_id: &str, name: &str) -> Value {
    let v = json!({ "action": "join_lobby", "chat_id": chat_id });
    card(
        header_with_subtitle(
            "🎰 欢迎来到牌桌",
            "群里在玩德州扑克",
            "turquoise",
        ),
        vec![
            markdown(&format!(
                "👋 **{name}**，要不要加入下一局？\n\n初始 1000 筹码 · 小盲 5 / 大盲 10 · 手牌仅本人可见"
            )),
            button_row(vec![button("加入下一局", v, "primary_filled")]),
            note("这条消息仅你可见，不想玩可以直接忽略。"),
        ],
    )
}

/// The persistent "lobby" card: a single message in the chat that's updated in
/// place as players join/leave, and that flips to a "in-progress" view (with no
/// buttons) for the duration of a hand.
fn build_lobby_card(snap: &GameSnapshot) -> Value {
    let in_progress = matches!(
        snap.stage,
        Stage::PreFlop | Stage::Flop | Stage::Turn | Stage::River | Stage::Showdown
    );

    let subtitle = if in_progress {
        format!("第 {} 局 · {} · 底池 {}", snap.hand_count, snap.stage.label(), snap.pot)
    } else if snap.hand_count == 0 {
        format!("已就座 {} 人 · 等待开局", snap.players.len())
    } else {
        format!("已就座 {} 人 · 已进行 {} 局", snap.players.len(), snap.hand_count)
    };

    let mut elements: Vec<Value> = vec![];

    if snap.players.is_empty() {
        elements.push(markdown("🪑 牌桌空空如也，点击下方 **加入** 就座。"));
    } else {
        // Avatar row for joined players.
        let active_ids: Vec<String> = snap
            .players
            .iter()
            .filter(|p| !p.sat_out)
            .map(|p| p.open_id.clone())
            .collect();
        if !active_ids.is_empty() {
            elements.push(person_list(&active_ids));
        }

        // Detailed lines (chips + status markers) below the avatars.
        let lines: Vec<String> = snap
            .players
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let dealer = if i == snap.dealer_idx && snap.hand_count > 0 {
                    " 🅓"
                } else {
                    ""
                };
                let status = if p.sat_out {
                    " 💤"
                } else if p.folded {
                    " ✗"
                } else if p.all_in {
                    " ★"
                } else {
                    ""
                };
                format!(
                    "• {}{}{} — {} 筹码",
                    at(&p.open_id), dealer, status, p.chips
                )
            })
            .collect();
        elements.push(markdown(&lines.join("\n")));
    }

    if !in_progress {
        let v_base = json!({ "chat_id": snap.chat_id });
        let mut buttons = vec![button(
            "加入",
            merge(&v_base, &json!({ "action": "join_lobby" })),
            "primary",
        )];
        if !snap.players.is_empty() {
            buttons.push(button(
                "离开",
                merge(&v_base, &json!({ "action": "leave_lobby" })),
                "default",
            ));
        }
        let chip_holders = snap.players.iter().filter(|p| p.chips > 0).count();
        if chip_holders >= 2 {
            buttons.push(button(
                "开局",
                merge(&v_base, &json!({ "action": "start_lobby" })),
                "primary",
            ));
        }
        elements.push(actions(buttons));
        elements.push(note_md(
            "初始 1000 筹码 · 小盲 5 / 大盲 10 · 手牌仅本人可见",
        ));
    } else {
        elements.push(note_md("牌局进行中，结束后可点按钮加入下一局。"));
    }

    let template = if in_progress { "wathet" } else { "turquoise" };
    card(
        header_with_subtitle("🎰 德州扑克 · 大厅", &subtitle, template),
        elements,
    )
}

/// Render the full game state. If `include_buttons` is true the card also
/// shows the actor-specific call amount and the action buttons — only ever
/// rendered into an ephemeral message sent to the actor themselves.
fn build_state_card(snap: &GameSnapshot, include_buttons: bool) -> Value {
    let actor = snap.current_open_id.as_deref();

    let subtitle = format!(
        "第 {} 局 · {} · 底池 {} · 当前注 {}",
        snap.hand_count, snap.stage.label(), snap.pot, snap.current_bet
    );

    let mut elements: Vec<Value> = vec![];

    if !snap.community.is_empty() {
        elements.push(markdown("**公共牌**"));
        elements.push(cards_row(&snap.community));
    }

    let player_lines: Vec<String> = snap
        .players
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let marker = if Some(p.open_id.as_str()) == actor {
                "▶"
            } else if p.folded {
                "✗"
            } else if p.all_in {
                "★"
            } else if p.sat_out {
                "💤"
            } else {
                "•"
            };
            let dealer = if i == snap.dealer_idx { " 🅓" } else { "" };
            format!(
                "{} {}{} — {} 筹码 (本轮 {})",
                marker, at(&p.open_id), dealer, p.chips, p.bet_in_round
            )
        })
        .collect();
    elements.push(markdown(&player_lines.join("\n")));

    let in_progress = matches!(
        snap.stage,
        Stage::PreFlop | Stage::Flop | Stage::Turn | Stage::River
    );

    if include_buttons {
        if let Some(open_id) = actor {
            let p_idx = snap
                .players
                .iter()
                .position(|p| p.open_id == open_id)
                .unwrap_or(0);
            let p = &snap.players[p_idx];
            let to_call = snap.current_bet.saturating_sub(p.bet_in_round);
            elements.push(hr());

            // Always re-surface the actor's own hole cards on their action
            // card so they don't have to scroll up to the original ephemeral.
            if !snap.viewer_hole.is_empty() {
                elements.push(markdown("**你的手牌**"));
                elements.push(cards_row(&snap.viewer_hole));
            }

            elements.push(markdown(&format!(
                "🎯 **你的回合** · 剩余筹码 **{}** · 需要跟注 **{}**",
                p.chips, to_call
            )));

            // Quick actions: Fold + (Check or Call) + All-in
            elements.push(button_row(quick_action_buttons(snap, p_idx)));

            // Custom raise via input form, plus a row of raise presets
            if p.chips > to_call {
                elements.push(raise_form_block(snap, p_idx));
                let presets = raise_preset_buttons(snap, p_idx);
                if !presets.is_empty() {
                    elements.push(button_row(presets));
                }
            }

            elements.push(note("仅你可见 · 群里其他人看不到这些按钮"));
        }
    } else if in_progress {
        if let Some(open_id) = actor {
            elements.push(note(&format!("{} 行动中…", at(open_id))));
        }
    } else {
        elements.push(note("等待发牌或本局已结束。"));
    }

    card(
        header_with_subtitle("🎰 德州扑克", &subtitle, "wathet"),
        elements,
    )
}

/// Fold + (Check / Call) + All-in. The form-input below handles custom raises.
///
/// Visual hierarchy:
/// - Fold uses `danger_text` — red text, no border, low visual weight.
/// - Check/Call uses `primary_filled` — solid blue, the prominent main action.
/// - All-in uses `default` — grey outline, secondary.
fn quick_action_buttons(snap: &GameSnapshot, idx: usize) -> Vec<Value> {
    let p = &snap.players[idx];
    let to_call = snap.current_bet.saturating_sub(p.bet_in_round);
    let chips = p.chips;
    let v_base = json!({
        "chat_id": snap.chat_id,
        "hand": snap.hand_count,
        "actor": p.open_id,
    });

    let mut buttons = vec![button(
        "弃牌",
        merge(&v_base, &json!({"action": "fold"})),
        "danger_text",
    )];

    if to_call == 0 {
        buttons.push(button(
            "过牌",
            merge(&v_base, &json!({"action": "check"})),
            "primary_filled",
        ));
    } else if chips <= to_call {
        // Forced all-in (calling would put them all-in or short)
        buttons.push(button(
            &format!("全押 {}", chips + p.bet_in_round),
            merge(&v_base, &json!({"action": "allin"})),
            "primary_filled",
        ));
        return buttons;
    } else {
        buttons.push(button(
            &format!("跟注 {}", to_call),
            merge(&v_base, &json!({"action": "call"})),
            "primary_filled",
        ));
    }

    if chips > to_call {
        buttons.push(button(
            &format!("全押 {}", chips + p.bet_in_round),
            merge(&v_base, &json!({"action": "allin"})),
            "default",
        ));
    }
    buttons
}

/// Form container with `加注到 [____]` input + a `确认加注` submit button on the
/// same row. Submitting the form triggers a `card.action.trigger` callback whose
/// `event.action.form_value.raise_to` carries the typed amount.
fn raise_form_block(snap: &GameSnapshot, idx: usize) -> Value {
    let p = &snap.players[idx];
    let min_raise_to = (snap.current_bet + snap.min_raise).max(snap.big_blind);
    let max_to = p.chips + p.bet_in_round;
    let v = json!({
        "chat_id": snap.chat_id,
        "hand": snap.hand_count,
        "actor": p.open_id,
        "action": "raise_custom",
    });
    form(
        "raise_form",
        vec![column_set(vec![
            column(
                vec![input_field(
                    "raise_to",
                    &format!("{}-{}", min_raise_to, max_to),
                    &min_raise_to.to_string(),
                    "加注到",
                )],
                3,
            ),
            column(
                vec![submit_button("确认加注", v, "primary_filled")],
                1,
            ),
        ])],
    )
}

/// Up to 3 quick raise presets (min raise, half-pot, pot).
fn raise_preset_buttons(snap: &GameSnapshot, idx: usize) -> Vec<Value> {
    let p = &snap.players[idx];
    let min_raise_to = (snap.current_bet + snap.min_raise).max(snap.big_blind);
    let max_to = p.chips + p.bet_in_round;
    let v_base = json!({
        "chat_id": snap.chat_id,
        "hand": snap.hand_count,
        "actor": p.open_id,
    });
    raise_presets(snap.current_bet, min_raise_to, snap.pot, max_to)
        .into_iter()
        .take(3)
        .map(|to| {
            let label = if to >= max_to {
                format!("全押 {}", max_to)
            } else if to == min_raise_to {
                format!("最小 {}", to)
            } else {
                format!("加到 {}", to)
            };
            let action_name = if to >= max_to { "allin" } else { "raise" };
            let mut v = json!({"action": action_name});
            if action_name == "raise" {
                v["to"] = json!(to);
            }
            button(&label, merge(&v_base, &v), "default")
        })
        .collect()
}

/// One-shot public card posted at the start of a hand so non-actors immediately
/// know the hand began, who's in, and who's first to act.
fn build_hand_start_card(snap: &GameSnapshot) -> Value {
    let dealer_at = snap
        .players
        .get(snap.dealer_idx)
        .map(|p| at(&p.open_id))
        .unwrap_or_else(|| "?".to_string());
    // Players who actually posted blinds this hand are exactly the ones whose
    // bet_in_round is positive — derive the names from that, so we don't have
    // to recompute heads-up vs multi-way blind positions here.
    let blind_posters: Vec<String> = snap
        .players
        .iter()
        .filter(|p| p.bet_in_round > 0)
        .map(|p| format!("{} ({})", at(&p.open_id), p.bet_in_round))
        .collect();
    let mut body = format!("庄家 {}", dealer_at);
    if !blind_posters.is_empty() {
        body.push_str(&format!("\n盲注：{}", blind_posters.join(" · ")));
    }
    if let Some(actor_id) = &snap.current_open_id {
        body.push_str(&format!("\n↓ 首位 {}", at(actor_id)));
    }
    card(
        header_with_subtitle(
            &format!("🂠 第 {} 局开始", snap.hand_count),
            &format!("底池 {}", snap.pot),
            "turquoise",
        ),
        vec![
            markdown(&body),
            note("行动按钮以**仅当前玩家可见**的方式发出"),
        ],
    )
}

/// Public, post-action announcement. Carries enough state info that non-actors
/// can follow the hand without ever seeing the ephemeral state card.
fn build_action_announcement(snap: &GameSnapshot, log: &ActionLogEntry) -> Value {
    let p = &snap.players[log.player_idx];
    let action = match log.kind {
        ActionKind::Fold => "弃牌".to_string(),
        ActionKind::Check => "过牌".to_string(),
        ActionKind::Call => format!("跟注到 {}", log.amount),
        ActionKind::Bet => format!("下注 {}", log.amount),
        ActionKind::Raise => format!("加注到 {}", log.amount),
        ActionKind::AllIn => format!("全押 {}", log.amount),
    };
    let mut subtitle = format!("底池 {}", snap.pot);
    if !snap.community.is_empty() {
        subtitle.push_str(&format!(
            " · 公共 {}",
            snap.community
                .iter()
                .map(|c| c.label())
                .collect::<Vec<_>>()
                .join(" ")
        ));
    }

    let mut body = format!(
        "{} {} (筹码 {})",
        at(&p.open_id),
        action,
        p.chips
    );

    let in_progress = matches!(
        snap.stage,
        Stage::PreFlop | Stage::Flop | Stage::Turn | Stage::River
    );
    if in_progress {
        if let Some(actor_id) = &snap.current_open_id {
            if actor_id != &p.open_id {
                body.push_str(&format!("\n↓ 下一位 {}", at(actor_id)));
            }
        }
    }
    card(
        header_with_subtitle("🎴 行动", &subtitle, "wathet"),
        vec![markdown(&body)],
    )
}

fn raise_presets(current_bet: u64, min_raise_to: u64, pot: u64, max_to: u64) -> Vec<u64> {
    use std::collections::BTreeSet;
    let mut s: BTreeSet<u64> = BTreeSet::new();
    if min_raise_to <= max_to {
        s.insert(min_raise_to);
    }
    let half_pot = current_bet + (pot / 2).max(1);
    if half_pot >= min_raise_to && half_pot < max_to {
        s.insert(half_pot);
    }
    let pot_raise = current_bet + pot.max(1);
    if pot_raise >= min_raise_to && pot_raise < max_to {
        s.insert(pot_raise);
    }
    if max_to >= min_raise_to {
        s.insert(max_to);
    }
    s.into_iter().take(4).collect()
}

fn merge(a: &Value, b: &Value) -> Value {
    let mut out = a.clone();
    if let (Some(o), Some(bm)) = (out.as_object_mut(), b.as_object()) {
        for (k, v) in bm {
            o.insert(k.clone(), v.clone());
        }
    }
    out
}

