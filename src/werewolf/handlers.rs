//! 狼人杀的 bot 控制层 —— 命令派发、卡片回调、AI 推进循环。
//!
//! `impl Bot` 扩展，复用 bot.rs 的 client / store / llm / dedup。每个 chat
//! 同时最多一桌狼人杀，由 `bot.wolf_games` map 管理。
//!
//! 推进核心：`advance_wolf` 串行驱动 AI 完成所有当前阶段能立即推进的工作，
//! 直到必须等待人类点击为止 —— 复用德州扑克的 `advance_actor` 模式。

use crate::bot::{toast, Bot};
use crate::feishu::cards::{card, header, markdown};
use crate::feishu::events::{CardAction, InboundMessage};
#[allow(unused_imports)]
use crate::game::Persona;
use crate::werewolf::cards::*;
use crate::werewolf::game::*;
use crate::werewolf::llm as wolf_llm;
use crate::werewolf::llm::AttemptHistory;
use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::time::Duration;
use tracing::{info, warn};

/// 两种顺序发言的语义区分。
enum SpeechKind {
    SheriffMain,
    Day,
}

impl Bot {
    // ========================================================================
    // 持久化
    // ========================================================================

    pub(crate) fn persist_wolf_locked(&self, chat_id: &str, game: &WolfGame) {
        if let Err(e) = self.store.save_wolf(chat_id, game) {
            warn!(?e, %chat_id, "persist wolf game failed");
        }
    }

    // ========================================================================
    // 命令入口（来自 bot.rs 的 dispatch_command）
    // ========================================================================

    pub(crate) async fn send_wolf_help(&self, msg: &InboundMessage) -> Result<()> {
        let c = card(
            header("🐺 狼人杀 帮助", "purple"),
            vec![
                markdown(
                    "**操作方式**：先在统一大厅 [加入] 进房，然后点 [开始狼人杀] 按钮，\
                     或 `/wolf start`。\n\n\
                     • `wolf join` 加入房间（等同于 `join`）\n\
                     • `wolf leave` 离开（等同于 `leave`）\n\
                     • `wolf start` 开狼人杀（9-12 名玩家）\n\
                     • `wolf reset` 重置房间（等同于 `reset`）\n\n\
                     **板娘配比**：\n\
                     • 9 人：3 狼 / 预 / 女 / 猎 / 3 民（不上警）\n\
                     • 10 人：2 狼 + **狼王** / 预 / 女 / 猎 / **守** / 3 民\n\
                     • 11 人：2 狼 + 狼王 / 预 / 女 / 猎 / 守 / 4 民\n\
                     • 12 人：3 狼 + 狼王 / 预 / 女 / 猎 / 守 / 4 民\n\n\
                     **关键规则**：\n\
                     • 狼王（10+ 板）被投票 / 反向送葬时可开枪，被毒不能\n\
                     • 守卫每晚守一人（含自己），不可连守同一人；同守同救会死\n\
                     • 10+ 板第 1 天有上警阶段，警长 1.5 倍票权，死亡可移交 / 撕毁警徽\n\
                     • 猎人被狼刀 / 放逐可开枪，被毒不能\n\n\
                     胜负：屠城——全部狼死（含狼王） = 好人胜；存活狼数 ≥ 存活好人数 = 狼胜",
                ),
                crate::feishu::cards::note_md(
                    "身份卡 / 夜间技能 / 投票 全部以**仅本人可见**的群消息发出",
                ),
            ],
        );
        self.send_user_only(msg, &c).await?;
        Ok(())
    }

    // ========================================================================
    // 卡片回调入口（仅游戏内行动，大厅按钮统一走 bot.rs handle_card_action）
    // ========================================================================

    pub(crate) async fn handle_wolf_card_action(
        self: std::sync::Arc<Self>,
        action: CardAction,
        action_id: String,
    ) -> Result<Value> {
        let chat_id = action
            .value
            .get("chat_id")
            .and_then(|v| v.as_str())
            .map(String::from)
            .unwrap_or(action.open_chat_id.clone());

        // 游戏中按钮：检查 stale & actor 授权
        let actor_id = action
            .value
            .get("actor")
            .and_then(|v| v.as_str())
            .map(String::from)
            .unwrap_or_default();
        let game_count = action
            .value
            .get("game")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);

        // stale 检查
        {
            let games = self.wolf_games.lock();
            if let Some(g) = games.get(&chat_id) {
                if game_count != 0 && game_count as u32 != g.game_count {
                    return Ok(toast("这是上一局的按钮"));
                }
            }
        }

        if !actor_id.is_empty() && action.open_id != actor_id {
            return Ok(toast("这张卡片不是给你的"));
        }

        // 路由各类游戏内按钮
        let target_open_id = action
            .value
            .get("target")
            .and_then(|v| v.as_str())
            .map(String::from);

        match action_id.as_str() {
            "wolf_guard_pick" => {
                let Some(target) = target_open_id else {
                    return Ok(toast("目标无效"));
                };
                let res = {
                    let mut games = self.wolf_games.lock();
                    let Some(g) = games.get_mut(&chat_id) else {
                        return Ok(toast("游戏不存在"));
                    };
                    let r = g.guard_pick(&action.open_id, &target);
                    if r.is_ok() {
                        self.persist_wolf_locked(&chat_id, g);
                    }
                    r
                };
                if let Err(e) = res {
                    return Ok(toast(&format!("{e}")));
                }
                let bot = self.clone();
                let cid = chat_id.clone();
                tokio::spawn(async move {
                    bot.advance_wolf(&cid).await;
                });
                Ok(toast("已守护"))
            }
            "wolf_kill" => {
                let target = match target_open_id {
                    Some(t) => t,
                    None => return Ok(toast("目标无效")),
                };
                let res = self.apply_wolf_kill(&chat_id, &action.open_id, &target);
                if let Err(e) = res {
                    return Ok(toast(&format!("{e}")));
                }
                let bot = self.clone();
                let cid = chat_id.clone();
                tokio::spawn(async move {
                    bot.broadcast_wolf_night_update(&cid).await;
                    bot.advance_wolf(&cid).await;
                });
                Ok(toast("已选目标 · 改主意可再点"))
            }
            "wolf_chat_send" => {
                let raw = action
                    .form_value
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim();
                if raw.is_empty() {
                    return Ok(toast("发言为空"));
                }
                let res = {
                    let mut games = self.wolf_games.lock();
                    let Some(g) = games.get_mut(&chat_id) else {
                        return Ok(toast("游戏不存在"));
                    };
                    let r = g.wolf_say(&action.open_id, raw.to_string());
                    if r.is_ok() {
                        self.persist_wolf_locked(&chat_id, g);
                    }
                    r
                };
                if let Err(e) = res {
                    return Ok(toast(&format!("{e}")));
                }
                let bot = self.clone();
                let cid = chat_id.clone();
                tokio::spawn(async move {
                    bot.broadcast_wolf_night_update(&cid).await;
                });
                Ok(json!({}))
            }
            "wolf_ready" => {
                let res = {
                    let mut games = self.wolf_games.lock();
                    let Some(g) = games.get_mut(&chat_id) else {
                        return Ok(toast("游戏不存在"));
                    };
                    let r = g.wolf_mark_ready(&action.open_id);
                    if r.is_ok() {
                        self.persist_wolf_locked(&chat_id, g);
                    }
                    r
                };
                if let Err(e) = res {
                    return Ok(toast(&format!("{e}")));
                }
                let bot = self.clone();
                let cid = chat_id.clone();
                tokio::spawn(async move {
                    bot.broadcast_wolf_night_update(&cid).await;
                    bot.advance_wolf(&cid).await;
                });
                Ok(toast("已就绪 · 等队友"))
            }
            "wolf_sheriff_run" => self.sheriff_nominate_and_advance(&chat_id, &action.open_id, true).await,
            "wolf_sheriff_skip" => self.sheriff_nominate_and_advance(&chat_id, &action.open_id, false).await,
            "wolf_sheriff_speech_submit" => {
                let speech = action
                    .form_value
                    .get("speech")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                self.submit_speech_and_advance(&chat_id, &action.open_id, speech, true).await
            }
            "wolf_sheriff_speech_skip" => {
                self.submit_speech_and_advance(&chat_id, &action.open_id, String::new(), true)
                    .await
            }
            "wolf_day_speech_submit" => {
                let speech = action
                    .form_value
                    .get("speech")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                self.submit_speech_and_advance(&chat_id, &action.open_id, speech, false).await
            }
            "wolf_day_speech_skip" => {
                self.submit_speech_and_advance(&chat_id, &action.open_id, String::new(), false)
                    .await
            }
            "wolf_last_words_submit" => {
                let speech = action
                    .form_value
                    .get("speech")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                self.submit_last_words_and_advance(&chat_id, &action.open_id, speech).await
            }
            "wolf_last_words_skip" => {
                self.submit_last_words_and_advance(&chat_id, &action.open_id, String::new())
                    .await
            }
            "wolf_sheriff_dir_up" => {
                self.pick_direction_and_advance(&chat_id, &action.open_id, true).await
            }
            "wolf_sheriff_dir_down" => {
                self.pick_direction_and_advance(&chat_id, &action.open_id, false).await
            }
            "wolf_sheriff_vote" => {
                let Some(target) = target_open_id else {
                    return Ok(toast("目标无效"));
                };
                self.sheriff_vote_and_advance(&chat_id, &action.open_id, Some(target))
                    .await
            }
            "wolf_sheriff_vote_abstain" => {
                self.sheriff_vote_and_advance(&chat_id, &action.open_id, None)
                    .await
            }
            "wolf_badge_pass" => {
                let Some(target) = target_open_id else {
                    return Ok(toast("目标无效"));
                };
                self.badge_pass_and_advance(&chat_id, &action.open_id, Some(target))
                    .await
            }
            "wolf_badge_destroy" => {
                self.badge_pass_and_advance(&chat_id, &action.open_id, None)
                    .await
            }
            "wolf_seer_check" => {
                let target = match target_open_id {
                    Some(t) => t,
                    None => return Ok(toast("目标无效")),
                };
                let r = {
                    let mut games = self.wolf_games.lock();
                    let Some(g) = games.get_mut(&chat_id) else {
                        return Ok(toast("游戏不存在"));
                    };
                    let res = g.seer_check(&action.open_id, &target);
                    if res.is_ok() {
                        self.persist_wolf_locked(&chat_id, g);
                    }
                    res
                };
                match r {
                    Ok(is_wolf) => {
                        // 私下回执
                        let target_player = {
                            let games = self.wolf_games.lock();
                            games
                                .get(&chat_id)
                                .and_then(|g| g.find_player(&target).map(|i| g.players[i].clone()))
                        };
                        let game_for_card = {
                            let games = self.wolf_games.lock();
                            games.get(&chat_id).cloned()
                        };
                        if let (Some(tp), Some(g)) = (target_player, game_for_card) {
                            let c = build_seer_result_card(&g, &tp, is_wolf);
                            let _ = self
                                .client
                                .send_ephemeral_card(&chat_id, &action.open_id, &c)
                                .await;
                        }
                        let bot = self.clone();
                        let cid = chat_id.clone();
                        tokio::spawn(async move {
                            bot.advance_wolf(&cid).await;
                        });
                        Ok(json!({}))
                    }
                    Err(e) => Ok(toast(&format!("{e}"))),
                }
            }
            "wolf_witch_save" => self.witch_act_and_advance(&chat_id, &action.open_id, true, None).await,
            "wolf_witch_skip" => self.witch_act_and_advance(&chat_id, &action.open_id, false, None).await,
            "wolf_witch_poison" => {
                let Some(target) = target_open_id else {
                    return Ok(toast("目标无效"));
                };
                self.witch_act_and_advance(&chat_id, &action.open_id, false, Some(target))
                    .await
            }
            "wolf_vote" => {
                let target = match target_open_id {
                    Some(t) => t,
                    None => return Ok(toast("目标无效")),
                };
                let res = {
                    let mut games = self.wolf_games.lock();
                    let Some(g) = games.get_mut(&chat_id) else {
                        return Ok(toast("游戏不存在"));
                    };
                    let r = g.cast_vote(&action.open_id, Some(&target));
                    if r.is_ok() {
                        self.persist_wolf_locked(&chat_id, g);
                    }
                    r
                };
                if let Err(e) = res {
                    return Ok(toast(&format!("{e}")));
                }
                let bot = self.clone();
                let cid = chat_id.clone();
                tokio::spawn(async move {
                    bot.advance_wolf(&cid).await;
                });
                Ok(toast("已投票"))
            }
            "wolf_vote_abstain" => {
                let res = {
                    let mut games = self.wolf_games.lock();
                    let Some(g) = games.get_mut(&chat_id) else {
                        return Ok(toast("游戏不存在"));
                    };
                    let r = g.cast_vote(&action.open_id, None);
                    if r.is_ok() {
                        self.persist_wolf_locked(&chat_id, g);
                    }
                    r
                };
                if let Err(e) = res {
                    return Ok(toast(&format!("{e}")));
                }
                let bot = self.clone();
                let cid = chat_id.clone();
                tokio::spawn(async move {
                    bot.advance_wolf(&cid).await;
                });
                Ok(toast("已弃权"))
            }
            "wolf_hunter_shoot" => {
                let Some(target) = target_open_id else {
                    return Ok(toast("目标无效"));
                };
                self.hunter_shoot_and_advance(&chat_id, &action.open_id, Some(target))
                    .await
            }
            "wolf_hunter_skip" => {
                self.hunter_shoot_and_advance(&chat_id, &action.open_id, None)
                    .await
            }
            _ => Ok(json!({})),
        }
    }

    // ========================================================================
    // 各种 apply / advance 助手
    // ========================================================================

    fn apply_wolf_kill(
        &self,
        chat_id: &str,
        wolf_open_id: &str,
        target_open_id: &str,
    ) -> Result<()> {
        let mut games = self.wolf_games.lock();
        let g = games
            .get_mut(chat_id)
            .ok_or_else(|| anyhow!("游戏不存在"))?;
        g.wolf_pick(wolf_open_id, target_open_id)?;
        self.persist_wolf_locked(chat_id, g);
        Ok(())
    }

    async fn witch_act_and_advance(
        self: &std::sync::Arc<Self>,
        chat_id: &str,
        witch_open_id: &str,
        save: bool,
        poison_open_id: Option<String>,
    ) -> Result<Value> {
        let res = {
            let mut games = self.wolf_games.lock();
            let Some(g) = games.get_mut(chat_id) else {
                return Ok(toast("游戏不存在"));
            };
            let r = g.witch_act(witch_open_id, save, poison_open_id.as_deref());
            if r.is_ok() {
                self.persist_wolf_locked(chat_id, g);
            }
            r
        };
        if let Err(e) = res {
            return Ok(toast(&format!("{e}")));
        }
        let bot = self.clone();
        let cid = chat_id.to_string();
        tokio::spawn(async move {
            bot.advance_wolf(&cid).await;
        });
        Ok(toast("已确认"))
    }

    async fn submit_last_words_and_advance(
        self: &std::sync::Arc<Self>,
        chat_id: &str,
        speaker_open_id: &str,
        speech: String,
    ) -> Result<Value> {
        let res = {
            let mut games = self.wolf_games.lock();
            let Some(g) = games.get_mut(chat_id) else {
                return Ok(toast("游戏不存在"));
            };
            let r = g.submit_last_words(speaker_open_id, speech);
            if r.is_ok() {
                self.persist_wolf_locked(chat_id, g);
            }
            r
        };
        if let Err(e) = res {
            return Ok(toast(&format!("{e}")));
        }
        let bot = self.clone();
        let cid = chat_id.to_string();
        tokio::spawn(async move {
            bot.advance_wolf(&cid).await;
        });
        Ok(toast("遗言已记录"))
    }

    async fn pick_direction_and_advance(
        self: &std::sync::Arc<Self>,
        chat_id: &str,
        sheriff_open_id: &str,
        clockwise: bool,
    ) -> Result<Value> {
        let res = {
            let mut games = self.wolf_games.lock();
            let Some(g) = games.get_mut(chat_id) else {
                return Ok(toast("游戏不存在"));
            };
            let r = g.pick_sheriff_direction(sheriff_open_id, clockwise);
            if r.is_ok() {
                self.persist_wolf_locked(chat_id, g);
            }
            r
        };
        if let Err(e) = res {
            return Ok(toast(&format!("{e}")));
        }
        // 公告
        let game = {
            let games = self.wolf_games.lock();
            games.get(chat_id).cloned()
        };
        if let Some(g) = game {
            if let Some(s_idx) = g.sheriff_idx {
                let announce = build_sheriff_direction_announce(&g, &g.players[s_idx], clockwise);
                let _ = self
                    .client
                    .send_message("chat_id", chat_id, "interactive", &announce)
                    .await;
            }
        }
        let bot = self.clone();
        let cid = chat_id.to_string();
        tokio::spawn(async move {
            bot.advance_wolf(&cid).await;
        });
        Ok(toast(if clockwise { "已选警上" } else { "已选警下" }))
    }

    async fn submit_speech_and_advance(
        self: &std::sync::Arc<Self>,
        chat_id: &str,
        speaker_open_id: &str,
        speech: String,
        sheriff: bool,
    ) -> Result<Value> {
        let res = {
            let mut games = self.wolf_games.lock();
            let Some(g) = games.get_mut(chat_id) else {
                return Ok(toast("游戏不存在"));
            };
            let r = if sheriff {
                g.submit_sheriff_speech(speaker_open_id, speech)
            } else {
                g.submit_day_speech(speaker_open_id, speech)
            };
            if r.is_ok() {
                self.persist_wolf_locked(chat_id, g);
            }
            r
        };
        if let Err(e) = res {
            return Ok(toast(&format!("{e}")));
        }
        let bot = self.clone();
        let cid = chat_id.to_string();
        tokio::spawn(async move {
            bot.advance_wolf(&cid).await;
        });
        Ok(toast("发言已提交"))
    }

    async fn sheriff_nominate_and_advance(
        self: &std::sync::Arc<Self>,
        chat_id: &str,
        voter_open_id: &str,
        running: bool,
    ) -> Result<Value> {
        let res = {
            let mut games = self.wolf_games.lock();
            let Some(g) = games.get_mut(chat_id) else {
                return Ok(toast("游戏不存在"));
            };
            let r = g.nominate_sheriff(voter_open_id, running);
            if r.is_ok() {
                self.persist_wolf_locked(chat_id, g);
            }
            r
        };
        if let Err(e) = res {
            return Ok(toast(&format!("{e}")));
        }
        let bot = self.clone();
        let cid = chat_id.to_string();
        tokio::spawn(async move {
            bot.advance_wolf(&cid).await;
        });
        Ok(toast(if running { "已上警" } else { "未上警" }))
    }

    async fn sheriff_vote_and_advance(
        self: &std::sync::Arc<Self>,
        chat_id: &str,
        voter_open_id: &str,
        target_open_id: Option<String>,
    ) -> Result<Value> {
        let res = {
            let mut games = self.wolf_games.lock();
            let Some(g) = games.get_mut(chat_id) else {
                return Ok(toast("游戏不存在"));
            };
            let r = g.cast_sheriff_vote(voter_open_id, target_open_id.as_deref());
            if r.is_ok() {
                self.persist_wolf_locked(chat_id, g);
            }
            r
        };
        if let Err(e) = res {
            return Ok(toast(&format!("{e}")));
        }
        let bot = self.clone();
        let cid = chat_id.to_string();
        tokio::spawn(async move {
            bot.advance_wolf(&cid).await;
        });
        Ok(toast("已投票"))
    }

    async fn badge_pass_and_advance(
        self: &std::sync::Arc<Self>,
        chat_id: &str,
        sheriff_open_id: &str,
        target_open_id: Option<String>,
    ) -> Result<Value> {
        let res = {
            let mut games = self.wolf_games.lock();
            let Some(g) = games.get_mut(chat_id) else {
                return Ok(toast("游戏不存在"));
            };
            let r = g.transfer_badge(sheriff_open_id, target_open_id.as_deref());
            if r.is_ok() {
                self.persist_wolf_locked(chat_id, g);
            }
            r
        };
        let new_holder = match res {
            Ok(h) => h,
            Err(e) => return Ok(toast(&format!("{e}"))),
        };

        // 公告
        let game = {
            let games = self.wolf_games.lock();
            games.get(chat_id).cloned()
        };
        if let Some(g) = game {
            let s_idx = g.find_player(sheriff_open_id);
            if let Some(s) = s_idx {
                let new_p = new_holder.map(|t| g.players[t].clone());
                let announce = build_badge_announce_card(&g.players[s], new_p.as_ref());
                let _ = self
                    .client
                    .send_message("chat_id", chat_id, "interactive", &announce)
                    .await;
            }
        }

        let bot = self.clone();
        let cid = chat_id.to_string();
        tokio::spawn(async move {
            bot.advance_wolf(&cid).await;
        });
        Ok(json!({}))
    }

    async fn hunter_shoot_and_advance(
        self: &std::sync::Arc<Self>,
        chat_id: &str,
        hunter_open_id: &str,
        target_open_id: Option<String>,
    ) -> Result<Value> {
        let res = {
            let mut games = self.wolf_games.lock();
            let Some(g) = games.get_mut(chat_id) else {
                return Ok(toast("游戏不存在"));
            };
            let r = g.hunter_shoot(hunter_open_id, target_open_id.as_deref());
            if r.is_ok() {
                self.persist_wolf_locked(chat_id, g);
            }
            r
        };
        let shot = match res {
            Ok(s) => s,
            Err(e) => return Ok(toast(&format!("{e}"))),
        };

        // 公告
        let (g_clone, h_idx_opt) = {
            let games = self.wolf_games.lock();
            let g = games.get(chat_id).cloned();
            let idx = g
                .as_ref()
                .and_then(|gg| gg.find_player(hunter_open_id));
            (g, idx)
        };
        if let (Some(g), Some(h_idx)) = (g_clone, h_idx_opt) {
            let target_player = shot.map(|t| g.players[t].clone());
            let announce =
                build_hunter_announce_card(&g.players[h_idx], target_player.as_ref());
            let _ = self
                .client
                .send_message("chat_id", chat_id, "interactive", &announce)
                .await;
        }

        let bot = self.clone();
        let cid = chat_id.to_string();
        tokio::spawn(async move {
            bot.advance_wolf(&cid).await;
        });
        Ok(json!({}))
    }

    // ========================================================================
    // 推进循环（核心）
    // ========================================================================

    /// 串行驱动游戏前进——每个阶段处理 AI 自动行动 / 公告 / 状态切换，直到必须等待人类点击为止。
    /// 像德州扑克的 advance_actor 那样可重入：每次外部事件后都安全地再调用一次。
    pub(crate) async fn advance_wolf(&self, chat_id: &str) {
        // 防递归：上限若干次循环避免任何意外。
        for _ in 0..50 {
            let stage_now = {
                let games = self.wolf_games.lock();
                games.get(chat_id).map(|g| g.stage)
            };
            let Some(stage) = stage_now else { return };

            match stage {
                Stage::Lobby | Stage::Ended => return,

                Stage::GuardPick => {
                    let (g_idx, is_ai, g_oid) = {
                        let games = self.wolf_games.lock();
                        let Some(g) = games.get(chat_id) else { return };
                        let Some(idx) = g.role_idx(crate::werewolf::game::Role::Guard) else {
                            // 没守卫，理论上不会进这里
                            return;
                        };
                        let p = &g.players[idx];
                        // 守卫死了 → 跳到狼人阶段
                        if !p.alive {
                            (idx, true, p.open_id.clone())
                        } else {
                            (idx, p.is_ai, p.open_id.clone())
                        }
                    };
                    // 死亡守卫 → 跳过
                    let dead_guard = {
                        let games = self.wolf_games.lock();
                        games.get(chat_id).map(|g| !g.players[g_idx].alive).unwrap_or(true)
                    };
                    if dead_guard {
                        let mut games = self.wolf_games.lock();
                        if let Some(g) = games.get_mut(chat_id) {
                            g.stage = Stage::WolvesPick;
                            self.persist_wolf_locked(chat_id, g);
                        }
                        continue;
                    }
                    if !is_ai {
                        let game = {
                            let games = self.wolf_games.lock();
                            games.get(chat_id).cloned()
                        };
                        if let Some(g) = game {
                            let c = build_guard_night_card(&g, &g.players[g_idx]);
                            let _ = self
                                .client
                                .send_ephemeral_card(chat_id, &g_oid, &c)
                                .await;
                        }
                        return;
                    }
                    // AI 守卫：retry-with-feedback —— 每次失败把错误反馈给 AI 让它重选
                    let mut history: AttemptHistory = vec![];
                    let mut decided = false;
                    for _attempt in 0..3 {
                        let target_idx =
                            self.guard_ai_pick(chat_id, g_idx, &history).await;
                        let target_oid = {
                            let games = self.wolf_games.lock();
                            games.get(chat_id).and_then(|g| {
                                g.players.get(target_idx).map(|p| p.open_id.clone())
                            })
                        };
                        let target_oid = match target_oid {
                            Some(t) => t,
                            None => {
                                history.push((
                                    format!("{{\"target_idx\": {}}}", target_idx),
                                    format!("idx {} 越界，玩家不存在", target_idx),
                                ));
                                continue;
                            }
                        };
                        let result = {
                            let mut games = self.wolf_games.lock();
                            let Some(g) = games.get_mut(chat_id) else { return };
                            let r = g.guard_pick(&g_oid, &target_oid);
                            if r.is_ok() {
                                self.persist_wolf_locked(chat_id, g);
                            }
                            r
                        };
                        match result {
                            Ok(()) => { decided = true; break; }
                            Err(e) => {
                                history.push((
                                    format!("{{\"target_idx\": {}}}", target_idx),
                                    e.to_string(),
                                ));
                            }
                        }
                    }
                    if !decided {
                        // AI 反复给非法答案 → 直接挑一个合法候选兜底，否则跳过 stage
                        let mut games = self.wolf_games.lock();
                        let Some(g) = games.get_mut(chat_id) else { return };
                        let cands: Vec<String> = g
                            .alive_indices()
                            .into_iter()
                            .filter(|i| g.last_guard_target != Some(*i))
                            .map(|i| g.players[i].open_id.clone())
                            .collect();
                        let mut ok = false;
                        for c in &cands {
                            if g.guard_pick(&g_oid, c).is_ok() {
                                ok = true;
                                break;
                            }
                        }
                        if !ok {
                            warn!(%g_oid, "no legal guard target after AI retries, skipping");
                            g.stage = Stage::WolvesPick;
                        }
                        self.persist_wolf_locked(chat_id, g);
                    }
                    tokio::time::sleep(Duration::from_millis(400)).await;
                }

                Stage::SheriffNominate => {
                    // AI 决定是否上警 + 给人类发上警卡
                    let pending_ais: Vec<(usize, String)> = {
                        let games = self.wolf_games.lock();
                        let Some(g) = games.get(chat_id) else { return };
                        g.alive_indices()
                            .into_iter()
                            .filter(|i| {
                                g.players[*i].is_ai
                                    && !g.sheriff_nominations.iter().any(|(idx, _)| idx == i)
                            })
                            .map(|i| (i, g.players[i].open_id.clone()))
                            .collect()
                    };
                    for (idx, oid) in pending_ais {
                        let run = self.sheriff_run_ai(chat_id, idx).await;
                        {
                            let mut games = self.wolf_games.lock();
                            if let Some(g) = games.get_mut(chat_id) {
                                let _ = g.nominate_sheriff(&oid, run);
                                self.persist_wolf_locked(chat_id, g);
                            }
                        }
                        tokio::time::sleep(Duration::from_millis(250)).await;
                    }

                    let all_done = {
                        let games = self.wolf_games.lock();
                        games.get(chat_id).map(|g| g.all_alive_nominated()).unwrap_or(false)
                    };
                    if !all_done {
                        let humans_pending: Vec<String> = {
                            let games = self.wolf_games.lock();
                            let Some(g) = games.get(chat_id) else { return };
                            g.alive_indices()
                                .into_iter()
                                .filter(|i| {
                                    !g.players[*i].is_ai
                                        && !g.sheriff_nominations.iter().any(|(idx, _)| idx == i)
                                })
                                .map(|i| g.players[i].open_id.clone())
                                .collect()
                        };
                        for oid in humans_pending {
                            let game = {
                                let games = self.wolf_games.lock();
                                games.get(chat_id).cloned()
                            };
                            if let Some(g) = game {
                                if let Some(p_idx) = g.find_player(&oid) {
                                    let c = build_sheriff_nominate_card(&g, &g.players[p_idx]);
                                    let _ = self
                                        .client
                                        .send_ephemeral_card(chat_id, &oid, &c)
                                        .await;
                                }
                            }
                        }
                        return;
                    }

                    // 全员决定 → 公告候选人 + finish
                    let game = {
                        let games = self.wolf_games.lock();
                        games.get(chat_id).cloned()
                    };
                    if let Some(g) = game {
                        let c = build_sheriff_candidates_card(&g);
                        let _ = self
                            .client
                            .send_message("chat_id", chat_id, "interactive", &c)
                            .await;
                    }
                    {
                        let mut games = self.wolf_games.lock();
                        if let Some(g) = games.get_mut(chat_id) {
                            let _ = g.finish_sheriff_nominate();
                            self.persist_wolf_locked(chat_id, g);
                        }
                    }
                }

                Stage::SheriffVote => {
                    let pending_ais: Vec<(usize, String)> = {
                        let games = self.wolf_games.lock();
                        let Some(g) = games.get(chat_id) else { return };
                        let candidates = g.sheriff_candidates();
                        g.alive_indices()
                            .into_iter()
                            .filter(|i| {
                                g.players[*i].is_ai
                                    && !candidates.contains(i)
                                    && g.sheriff_votes.for_voter(*i).is_none()
                            })
                            .map(|i| (i, g.players[i].open_id.clone()))
                            .collect()
                    };
                    for (idx, oid) in pending_ais {
                        // retry-with-feedback：AI 投错就把错误反馈让它重选
                        let mut hist: AttemptHistory = vec![];
                        let mut decided = false;
                        for _ in 0..3 {
                            let target_opt = self.sheriff_vote_ai(chat_id, idx, &hist).await;
                            let target_oid = target_opt.and_then(|t| {
                                let games = self.wolf_games.lock();
                                games.get(chat_id).and_then(|g| {
                                    g.players.get(t).map(|p| p.open_id.clone())
                                })
                            });
                            let result = {
                                let mut games = self.wolf_games.lock();
                                let Some(g) = games.get_mut(chat_id) else { return };
                                let r = g.cast_sheriff_vote(&oid, target_oid.as_deref());
                                if r.is_ok() {
                                    self.persist_wolf_locked(chat_id, g);
                                }
                                r
                            };
                            match result {
                                Ok(()) => { decided = true; break; }
                                Err(e) => hist.push((
                                    format!(
                                        "{{\"target_idx\": {}}}",
                                        target_opt.map(|i| i as i64).unwrap_or(-1)
                                    ),
                                    e.to_string(),
                                )),
                            }
                        }
                        if !decided {
                            // 兜底：弃权
                            let mut games = self.wolf_games.lock();
                            if let Some(g) = games.get_mut(chat_id) {
                                let _ = g.cast_sheriff_vote(&oid, None);
                                self.persist_wolf_locked(chat_id, g);
                            }
                        }
                        tokio::time::sleep(Duration::from_millis(250)).await;
                    }

                    let all_done = {
                        let games = self.wolf_games.lock();
                        games
                            .get(chat_id)
                            .map(|g| g.all_sheriff_voters_cast())
                            .unwrap_or(false)
                    };
                    if !all_done {
                        // 给非候选人类发投票卡
                        let humans_pending: Vec<String> = {
                            let games = self.wolf_games.lock();
                            let Some(g) = games.get(chat_id) else { return };
                            let candidates = g.sheriff_candidates();
                            g.alive_indices()
                                .into_iter()
                                .filter(|i| {
                                    !g.players[*i].is_ai
                                        && !candidates.contains(i)
                                        && g.sheriff_votes.for_voter(*i).is_none()
                                })
                                .map(|i| g.players[i].open_id.clone())
                                .collect()
                        };
                        for oid in humans_pending {
                            let game = {
                                let games = self.wolf_games.lock();
                                games.get(chat_id).cloned()
                            };
                            if let Some(g) = game {
                                if let Some(p_idx) = g.find_player(&oid) {
                                    let c = build_sheriff_vote_card(&g, &g.players[p_idx]);
                                    let _ = self
                                        .client
                                        .send_ephemeral_card(chat_id, &oid, &c)
                                        .await;
                                }
                            }
                        }
                        return;
                    }

                    {
                        let mut games = self.wolf_games.lock();
                        if let Some(g) = games.get_mut(chat_id) {
                            let _ = g.resolve_sheriff_vote();
                            self.persist_wolf_locked(chat_id, g);
                        }
                    }
                }

                Stage::BadgePass => {
                    let (h_idx, is_ai, h_oid) = {
                        let games = self.wolf_games.lock();
                        let Some(g) = games.get(chat_id) else { return };
                        let Some(idx) = g.pending_badge else { return };
                        let p = &g.players[idx];
                        (idx, p.is_ai, p.open_id.clone())
                    };
                    if !is_ai {
                        let game = {
                            let games = self.wolf_games.lock();
                            games.get(chat_id).cloned()
                        };
                        if let Some(g) = game {
                            let c = build_badge_pass_card(&g, &g.players[h_idx]);
                            let _ = self
                                .client
                                .send_ephemeral_card(chat_id, &h_oid, &c)
                                .await;
                        }
                        return;
                    }
                    // AI 警长 retry-with-feedback
                    let mut hist: AttemptHistory = vec![];
                    let mut new_holder: Option<usize> = None;
                    let mut decided = false;
                    for _ in 0..3 {
                        let target_opt = self.badge_pass_ai(chat_id, h_idx, &hist).await;
                        let target_oid = target_opt.and_then(|t| {
                            let games = self.wolf_games.lock();
                            games.get(chat_id).and_then(|g| {
                                g.players.get(t).map(|p| p.open_id.clone())
                            })
                        });
                        let r = {
                            let mut games = self.wolf_games.lock();
                            let Some(g) = games.get_mut(chat_id) else { return };
                            let r = g.transfer_badge(&h_oid, target_oid.as_deref());
                            if r.is_ok() {
                                self.persist_wolf_locked(chat_id, g);
                            }
                            r
                        };
                        match r {
                            Ok(holder) => {
                                new_holder = holder;
                                decided = true;
                                break;
                            }
                            Err(e) => hist.push((
                                format!(
                                    "{{\"target_idx\": {}}}",
                                    target_opt.map(|i| i as i64).unwrap_or(-1)
                                ),
                                e.to_string(),
                            )),
                        }
                    }
                    if !decided {
                        // 兜底：撕毁警徽
                        let mut games = self.wolf_games.lock();
                        if let Some(g) = games.get_mut(chat_id) {
                            let r = g.transfer_badge(&h_oid, None);
                            if r.is_ok() {
                                self.persist_wolf_locked(chat_id, g);
                            }
                            new_holder = r.ok().flatten();
                        }
                    }
                    let game = {
                        let games = self.wolf_games.lock();
                        games.get(chat_id).cloned()
                    };
                    if let Some(g) = game {
                        let new_p = new_holder.map(|t| g.players[t].clone());
                        let announce =
                            build_badge_announce_card(&g.players[h_idx], new_p.as_ref());
                        let _ = self
                            .client
                            .send_message("chat_id", chat_id, "interactive", &announce)
                            .await;
                    }
                    tokio::time::sleep(Duration::from_millis(400)).await;
                }

                Stage::WolvesPick => {
                    // 判断模式：全 AI 走快速通道，混合 / 全人类走聊天通道
                    let (all_ai, has_humans) = {
                        let games = self.wolf_games.lock();
                        let Some(g) = games.get(chat_id) else { return };
                        let alive = g.alive_wolves();
                        let any_human = alive.iter().any(|w| !g.players[*w].is_ai);
                        (!any_human, any_human)
                    };

                    if all_ai {
                        // 全 AI 快速通道：顺序决策、不发卡、不聊天
                        let pending_ai_wolves: Vec<(usize, String)> = {
                            let games = self.wolf_games.lock();
                            let Some(g) = games.get(chat_id) else { return };
                            g.alive_wolves()
                                .into_iter()
                                .filter(|w| {
                                    !g.wolf_kill_votes.iter().any(|(voter, _)| voter == w)
                                })
                                .map(|i| (i, g.players[i].open_id.clone()))
                                .collect()
                        };
                        for (idx, open_id) in pending_ai_wolves {
                            // retry-with-feedback：AI 选错就给反馈让它重选
                            let mut hist: AttemptHistory = vec![];
                            for _ in 0..3 {
                                let decision = self
                                    .wolf_ai_pick(chat_id, idx, false, &hist)
                                    .await;
                                let target_open_id = {
                                    let games = self.wolf_games.lock();
                                    games.get(chat_id).and_then(|g| {
                                        g.players.get(decision.target_idx).map(|p| p.open_id.clone())
                                    })
                                };
                                let target_oid = match target_open_id {
                                    Some(t) => t,
                                    None => {
                                        hist.push((
                                            format!("{{\"target_idx\": {}}}", decision.target_idx),
                                            format!("idx {} 越界", decision.target_idx),
                                        ));
                                        continue;
                                    }
                                };
                                match self.apply_wolf_kill(chat_id, &open_id, &target_oid) {
                                    Ok(()) => break,
                                    Err(e) => hist.push((
                                        format!("{{\"target_idx\": {}}}", decision.target_idx),
                                        e.to_string(),
                                    )),
                                }
                            }
                            // 3 次都失败就跳过这只狼（这只狼无效投票）
                            tokio::time::sleep(Duration::from_millis(300)).await;
                        }
                        // 直接 advance（无需"我决定了"）
                        let mut games = self.wolf_games.lock();
                        if let Some(g) = games.get_mut(chat_id) {
                            if let Err(e) = g.advance_after_wolves() {
                                warn!(?e, "advance_after_wolves failed");
                                return;
                            }
                            self.persist_wolf_locked(chat_id, g);
                        }
                        continue;
                    }

                    // 混合 / 全人类：先发卡给人类，AI 之后顺序行动
                    if has_humans {
                        let humans: Vec<(usize, String)> = {
                            let games = self.wolf_games.lock();
                            let Some(g) = games.get(chat_id) else { return };
                            g.alive_wolves()
                                .into_iter()
                                .filter(|w| !g.players[*w].is_ai)
                                .map(|i| (i, g.players[i].open_id.clone()))
                                .collect()
                        };
                        for (wolf_idx, oid) in humans {
                            self.send_or_update_wolf_night_card(chat_id, wolf_idx, &oid).await;
                        }
                    }

                    // AI 顺序决策（每只 AI 提交目标 + 可选发言）
                    let pending_ai_wolves: Vec<(usize, String)> = {
                        let games = self.wolf_games.lock();
                        let Some(g) = games.get(chat_id) else { return };
                        g.alive_wolves()
                            .into_iter()
                            .filter(|w| {
                                g.players[*w].is_ai
                                    && !g.is_wolf_ready(*w)
                            })
                            .map(|i| (i, g.players[i].open_id.clone()))
                            .collect()
                    };
                    for (idx, open_id) in pending_ai_wolves {
                        let mut hist: AttemptHistory = vec![];
                        let mut chat_msg: Option<String> = None;
                        let mut voted = false;
                        for _ in 0..3 {
                            let decision = self
                                .wolf_ai_pick(chat_id, idx, true, &hist)
                                .await;
                            let target_open_id = {
                                let games = self.wolf_games.lock();
                                games.get(chat_id).and_then(|g| {
                                    g.players.get(decision.target_idx).map(|p| p.open_id.clone())
                                })
                            };
                            let target_oid = match target_open_id {
                                Some(t) => t,
                                None => {
                                    hist.push((
                                        format!("{{\"target_idx\": {}}}", decision.target_idx),
                                        format!("idx {} 越界", decision.target_idx),
                                    ));
                                    continue;
                                }
                            };
                            match self.apply_wolf_kill(chat_id, &open_id, &target_oid) {
                                Ok(()) => {
                                    chat_msg = decision.chat;
                                    voted = true;
                                    break;
                                }
                                Err(e) => hist.push((
                                    format!("{{\"target_idx\": {}}}", decision.target_idx),
                                    e.to_string(),
                                )),
                            }
                        }
                        if !voted {
                            // 兜底：随便挑一个合法目标投，避免这只 AI 永远不就绪
                            let fallback = {
                                let games = self.wolf_games.lock();
                                games.get(chat_id).and_then(|g| {
                                    g.players
                                        .iter()
                                        .enumerate()
                                        .find(|(i, p)| p.alive && !g.is_wolf(*i))
                                        .map(|(_, p)| p.open_id.clone())
                                })
                            };
                            if let Some(t) = fallback {
                                let _ = self.apply_wolf_kill(chat_id, &open_id, &t);
                            }
                        }
                        {
                            let mut games = self.wolf_games.lock();
                            if let Some(g) = games.get_mut(chat_id) {
                                if let Some(msg) = chat_msg {
                                    let _ = g.wolf_say(&open_id, msg);
                                }
                                let _ = g.wolf_mark_ready(&open_id);
                                self.persist_wolf_locked(chat_id, g);
                            }
                        }
                        self.broadcast_wolf_night_update(chat_id).await;
                        tokio::time::sleep(Duration::from_millis(700)).await;
                    }

                    // 检查是否所有存活狼都就绪
                    let all_ready = {
                        let games = self.wolf_games.lock();
                        games.get(chat_id)
                            .map(|g| g.all_alive_wolves_ready())
                            .unwrap_or(false)
                    };
                    if !all_ready {
                        // 等待人类点 [我决定了] —— 卡片已发，return
                        return;
                    }

                    // 全员就绪 → 推进
                    {
                        let mut games = self.wolf_games.lock();
                        let Some(g) = games.get_mut(chat_id) else { return };
                        if let Err(e) = g.advance_after_wolves() {
                            warn!(?e, "advance_after_wolves failed");
                            return;
                        }
                        self.persist_wolf_locked(chat_id, g);
                    }
                }

                Stage::SeerPick => {
                    let (seer_idx, is_ai, seer_oid) = {
                        let games = self.wolf_games.lock();
                        let Some(g) = games.get(chat_id) else { return };
                        let Some(idx) = g.role_idx(Role::Seer) else {
                            // 没预言家，理论上不会进这里——直接结算
                            warn!("SeerPick stage but no seer found");
                            return;
                        };
                        let p = &g.players[idx];
                        (idx, p.is_ai, p.open_id.clone())
                    };

                    if !is_ai {
                        // 给预言家发夜间卡
                        let game = {
                            let games = self.wolf_games.lock();
                            games.get(chat_id).cloned()
                        };
                        if let Some(g) = game {
                            let c = build_seer_night_card(&g, &g.players[seer_idx]);
                            let _ = self
                                .client
                                .send_ephemeral_card(chat_id, &seer_oid, &c)
                                .await;
                        }
                        return;
                    }

                    // AI 预言家 retry-with-feedback
                    let mut hist: AttemptHistory = vec![];
                    let mut decided = false;
                    for _ in 0..3 {
                        let target_idx =
                            self.seer_ai_pick(chat_id, seer_idx, &hist).await;
                        let target_oid = {
                            let games = self.wolf_games.lock();
                            games.get(chat_id).and_then(|g| {
                                g.players.get(target_idx).map(|p| p.open_id.clone())
                            })
                        };
                        let target_oid = match target_oid {
                            Some(t) => t,
                            None => {
                                hist.push((
                                    format!("{{\"target_idx\": {}}}", target_idx),
                                    format!("idx {} 越界", target_idx),
                                ));
                                continue;
                            }
                        };
                        let r = {
                            let mut games = self.wolf_games.lock();
                            let Some(g) = games.get_mut(chat_id) else { return };
                            let r = g.seer_check(&seer_oid, &target_oid);
                            if r.is_ok() {
                                self.persist_wolf_locked(chat_id, g);
                            }
                            r
                        };
                        match r {
                            Ok(_) => { decided = true; break; }
                            Err(e) => hist.push((
                                format!("{{\"target_idx\": {}}}", target_idx),
                                e.to_string(),
                            )),
                        }
                    }
                    if !decided {
                        // 兜底：找一个非自己 alive 玩家查
                        let fallback = {
                            let games = self.wolf_games.lock();
                            games.get(chat_id).and_then(|g| {
                                g.players
                                    .iter()
                                    .enumerate()
                                    .find(|(i, p)| p.alive && *i != seer_idx)
                                    .map(|(_, p)| p.open_id.clone())
                            })
                        };
                        if let Some(t) = fallback {
                            let mut games = self.wolf_games.lock();
                            if let Some(g) = games.get_mut(chat_id) {
                                let _ = g.seer_check(&seer_oid, &t);
                                self.persist_wolf_locked(chat_id, g);
                            }
                        }
                    }
                    tokio::time::sleep(Duration::from_millis(400)).await;
                }

                Stage::WitchAct => {
                    let (witch_idx, is_ai, witch_oid) = {
                        let games = self.wolf_games.lock();
                        let Some(g) = games.get(chat_id) else { return };
                        let Some(idx) = g.role_idx(Role::Witch) else {
                            warn!("WitchAct stage but no witch found");
                            return;
                        };
                        let p = &g.players[idx];
                        (idx, p.is_ai, p.open_id.clone())
                    };

                    if !is_ai {
                        let game = {
                            let games = self.wolf_games.lock();
                            games.get(chat_id).cloned()
                        };
                        if let Some(g) = game {
                            let c = build_witch_night_card(&g, &g.players[witch_idx]);
                            let _ = self
                                .client
                                .send_ephemeral_card(chat_id, &witch_oid, &c)
                                .await;
                        }
                        return;
                    }

                    // AI 女巫 retry-with-feedback
                    let mut hist: AttemptHistory = vec![];
                    let mut decided = false;
                    for _ in 0..3 {
                        let decision =
                            self.witch_ai_decide(chat_id, witch_idx, &hist).await;
                        let r = {
                            let mut games = self.wolf_games.lock();
                            let Some(g) = games.get_mut(chat_id) else { return };
                            let r = match &decision {
                                wolf_llm::WitchDecision::Save => {
                                    g.witch_act(&witch_oid, true, None)
                                }
                                wolf_llm::WitchDecision::Poison(idx) => {
                                    let target_oid = g.players[*idx].open_id.clone();
                                    g.witch_act(&witch_oid, false, Some(&target_oid))
                                }
                                wolf_llm::WitchDecision::Skip => {
                                    g.witch_act(&witch_oid, false, None)
                                }
                            };
                            if r.is_ok() {
                                self.persist_wolf_locked(chat_id, g);
                            }
                            r
                        };
                        match r {
                            Ok(()) => { decided = true; break; }
                            Err(e) => {
                                let answer_repr = match &decision {
                                    wolf_llm::WitchDecision::Save => {
                                        "{\"action\": \"save\"}".to_string()
                                    }
                                    wolf_llm::WitchDecision::Poison(idx) => format!(
                                        "{{\"action\": \"poison\", \"poison_target_idx\": {}}}",
                                        idx
                                    ),
                                    wolf_llm::WitchDecision::Skip => {
                                        "{\"action\": \"skip\"}".to_string()
                                    }
                                };
                                hist.push((answer_repr, e.to_string()));
                            }
                        }
                    }
                    if !decided {
                        // 兜底：跳过
                        let mut games = self.wolf_games.lock();
                        if let Some(g) = games.get_mut(chat_id) {
                            let _ = g.witch_act(&witch_oid, false, None);
                            self.persist_wolf_locked(chat_id, g);
                        }
                    }
                    tokio::time::sleep(Duration::from_millis(400)).await;
                }

                Stage::DayReveal => {
                    // 公开广播昨夜死讯
                    let game = {
                        let games = self.wolf_games.lock();
                        games.get(chat_id).cloned()
                    };
                    if let Some(g) = game {
                        let c = build_day_reveal_card(&g);
                        let _ = self
                            .client
                            .send_message("chat_id", chat_id, "interactive", &c)
                            .await;
                    }

                    // 推进到讨论阶段
                    {
                        let mut games = self.wolf_games.lock();
                        let Some(g) = games.get_mut(chat_id) else { return };
                        if let Err(e) = g.enter_day_discuss() {
                            warn!(?e, "enter_day_discuss failed");
                            return;
                        }
                        self.persist_wolf_locked(chat_id, g);
                    }

                    // 胜负检查（在 enter_day_discuss 内部已检测；如果已 Ended 进 next iter）
                }

                Stage::SheriffPickDirection => {
                    let (s_idx, is_ai, s_oid) = {
                        let games = self.wolf_games.lock();
                        let Some(g) = games.get(chat_id) else { return };
                        let Some(idx) = g.sheriff_idx else { return };
                        let p = &g.players[idx];
                        (idx, p.is_ai, p.open_id.clone())
                    };
                    if is_ai {
                        let clockwise = self.sheriff_direction_ai(chat_id, s_idx).await;
                        {
                            let mut games = self.wolf_games.lock();
                            if let Some(g) = games.get_mut(chat_id) {
                                let _ = g.pick_sheriff_direction(&s_oid, clockwise);
                                self.persist_wolf_locked(chat_id, g);
                            }
                        }
                        // 公告
                        let game = {
                            let games = self.wolf_games.lock();
                            games.get(chat_id).cloned()
                        };
                        if let Some(g) = game {
                            let announce = build_sheriff_direction_announce(
                                &g,
                                &g.players[s_idx],
                                clockwise,
                            );
                            let _ = self
                                .client
                                .send_message("chat_id", chat_id, "interactive", &announce)
                                .await;
                        }
                        tokio::time::sleep(Duration::from_millis(400)).await;
                    } else {
                        // 人类警长 → 私发选择卡
                        let game = {
                            let games = self.wolf_games.lock();
                            games.get(chat_id).cloned()
                        };
                        if let Some(g) = game {
                            let c = build_sheriff_direction_card(&g, &g.players[s_idx]);
                            let _ = self
                                .client
                                .send_ephemeral_card(chat_id, &s_oid, &c)
                                .await;
                        }
                        return;
                    }
                }

                Stage::LastWords => {
                    self.refresh_last_words_public(chat_id).await;
                    let current: Option<(usize, String, bool)> = {
                        let games = self.wolf_games.lock();
                        let Some(g) = games.get(chat_id) else { return };
                        g.current_last_words_speaker().map(|i| {
                            let p = &g.players[i];
                            (i, p.open_id.clone(), p.is_ai)
                        })
                    };
                    let Some((spk_idx, spk_oid, is_ai)) = current else {
                        // 全员说完
                        let mut games = self.wolf_games.lock();
                        if let Some(g) = games.get_mut(chat_id) {
                            if let Err(e) = g.finish_last_words() {
                                warn!(?e, "finish_last_words failed, force-routing");
                                // 兜底：手动按 finish_last_words 的逻辑切 stage
                                if g.pending_hunter.is_some() {
                                    g.stage = Stage::HunterShoot;
                                } else if g.pending_badge.is_some() {
                                    g.stage = Stage::BadgePass;
                                } else {
                                    let post = g.last_words_post_stage.take();
                                    match post {
                                        Some(Stage::DayReveal) => g.stage = Stage::DayReveal,
                                        _ => {
                                            // 进下一夜：直接调 advance_to_next_night_or_end 等价物
                                            g.stage = Stage::DayReveal; // 安全兜底
                                        }
                                    }
                                }
                            }
                            self.persist_wolf_locked(chat_id, g);
                        }
                        continue;
                    };
                    if is_ai {
                        // 当前说遗言的人正好是要开枪的猎人/狼王 → 走 combined：
                        // 同一次 LLM 同时产出遗言 + 开枪决策，避免言行不一。
                        let is_dying_shooter = {
                            let games = self.wolf_games.lock();
                            games
                                .get(chat_id)
                                .map(|g| g.pending_hunter == Some(spk_idx))
                                .unwrap_or(false)
                        };
                        if is_dying_shooter {
                            let decision = self.dying_shooter_combined_ai(chat_id, spk_idx).await;
                            {
                                let mut games = self.wolf_games.lock();
                                if let Some(g) = games.get_mut(chat_id) {
                                    if let Err(e) = g.submit_last_words(&spk_oid, decision.speech) {
                                        warn!(?e, %spk_oid, "submit_last_words failed (combined), force-advancing");
                                        g.last_words_idx += 1;
                                    }
                                    // 把开枪目标存到游戏状态，HunterShoot 阶段直接用，不再调 LLM
                                    g.pending_hunter_ai_decision = Some(decision.target);
                                    self.persist_wolf_locked(chat_id, g);
                                }
                            }
                        } else {
                            let text = self.last_words_ai(chat_id, spk_idx).await;
                            {
                                let mut games = self.wolf_games.lock();
                                if let Some(g) = games.get_mut(chat_id) {
                                    if let Err(e) = g.submit_last_words(&spk_oid, text) {
                                        warn!(?e, %spk_oid, "submit_last_words failed, force-advancing");
                                        g.last_words_idx += 1;
                                    }
                                    self.persist_wolf_locked(chat_id, g);
                                }
                            }
                        }
                        tokio::time::sleep(Duration::from_millis(700)).await;
                    } else {
                        self.send_or_update_last_words_private(chat_id, spk_idx, &spk_oid)
                            .await;
                        return;
                    }
                }

                Stage::SheriffSpeech => {
                    self.refresh_speech_public(chat_id, /* sheriff = */ true).await;

                    let current: Option<(usize, String, bool)> = {
                        let games = self.wolf_games.lock();
                        let Some(g) = games.get(chat_id) else { return };
                        g.current_sheriff_speaker().map(|i| {
                            let p = &g.players[i];
                            (i, p.open_id.clone(), p.is_ai)
                        })
                    };
                    let Some((spk_idx, spk_oid, is_ai)) = current else {
                        // 全部说完 → 进警长投票
                        {
                            let mut games = self.wolf_games.lock();
                            if let Some(g) = games.get_mut(chat_id) {
                                if let Err(e) = g.finish_sheriff_speeches() {
                                    warn!(?e, "finish_sheriff_speeches failed, force-advancing to SheriffVote");
                                    g.stage = Stage::SheriffVote;
                                    g.sheriff_votes.clear();
                                }
                                self.persist_wolf_locked(chat_id, g);
                            }
                        }
                        continue;
                    };

                    if is_ai {
                        let text = self.sheriff_speech_ai(chat_id, spk_idx).await;
                        {
                            let mut games = self.wolf_games.lock();
                            if let Some(g) = games.get_mut(chat_id) {
                                if let Err(e) = g.submit_sheriff_speech(&spk_oid, text) {
                                    warn!(?e, %spk_oid, "submit_sheriff_speech failed, force-advancing");
                                    g.sheriff_speech_idx += 1;
                                }
                                self.persist_wolf_locked(chat_id, g);
                            }
                        }
                        tokio::time::sleep(Duration::from_millis(700)).await;
                    } else {
                        self.send_or_update_speech_private(
                            chat_id,
                            spk_idx,
                            &spk_oid,
                            /* sheriff = */ true,
                        )
                        .await;
                        return;
                    }
                }

                Stage::DaySpeech => {
                    self.refresh_speech_public(chat_id, /* sheriff = */ false).await;

                    let current: Option<(usize, String, bool)> = {
                        let games = self.wolf_games.lock();
                        let Some(g) = games.get(chat_id) else { return };
                        g.current_day_speaker().map(|i| {
                            let p = &g.players[i];
                            (i, p.open_id.clone(), p.is_ai)
                        })
                    };
                    let Some((spk_idx, spk_oid, is_ai)) = current else {
                        // 全员发完 → 进白天投票
                        {
                            let mut games = self.wolf_games.lock();
                            if let Some(g) = games.get_mut(chat_id) {
                                // enter_day_vote 失败时强行设 stage 防止死循环
                                if let Err(e) = g.enter_day_vote() {
                                    warn!(?e, "enter_day_vote failed, force-setting stage");
                                    g.stage = Stage::DayVote;
                                    g.day_votes.clear();
                                }
                                self.persist_wolf_locked(chat_id, g);
                            }
                        }
                        let c = card(
                            header("🗳️ 投票开始", "blue"),
                            vec![markdown(
                                "发言结束。每位存活玩家请通过私密卡投票。",
                            )],
                        );
                        let _ = self
                            .client
                            .send_message("chat_id", chat_id, "interactive", &c)
                            .await;
                        continue;
                    };

                    if is_ai {
                        let text = self.day_speech_ai(chat_id, spk_idx).await;
                        {
                            let mut games = self.wolf_games.lock();
                            if let Some(g) = games.get_mut(chat_id) {
                                // submit 失败强行推进 idx 防止死循环
                                if let Err(e) = g.submit_day_speech(&spk_oid, text) {
                                    warn!(
                                        ?e,
                                        %spk_oid,
                                        spk_idx,
                                        idx = g.day_speech_idx,
                                        order_len = g.day_speech_order.len(),
                                        "submit_day_speech failed, force-advancing idx"
                                    );
                                    g.day_speech_idx += 1;
                                }
                                self.persist_wolf_locked(chat_id, g);
                            }
                        }
                        tokio::time::sleep(Duration::from_millis(700)).await;
                    } else {
                        self.send_or_update_speech_private(
                            chat_id,
                            spk_idx,
                            &spk_oid,
                            /* sheriff = */ false,
                        )
                        .await;
                        return;
                    }
                }

                Stage::DayVote => {
                    // 1. AI 投票
                    let pending_ais: Vec<(usize, String)> = {
                        let games = self.wolf_games.lock();
                        let Some(g) = games.get(chat_id) else { return };
                        g.alive_indices()
                            .into_iter()
                            .filter(|i| {
                                g.players[*i].is_ai && g.day_votes.for_voter(*i).is_none()
                            })
                            .map(|i| (i, g.players[i].open_id.clone()))
                            .collect()
                    };
                    for (idx, oid) in pending_ais {
                        // 投票 retry-with-feedback；不再带 quip
                        let mut hist: AttemptHistory = vec![];
                        let mut decided = false;
                        for _ in 0..3 {
                            let decision = self.vote_ai_pick(chat_id, idx, &hist).await;
                            let target_oid = {
                                let games = self.wolf_games.lock();
                                let Some(g) = games.get(chat_id) else { return };
                                decision
                                    .target_idx
                                    .and_then(|t| g.players.get(t))
                                    .map(|p| p.open_id.clone())
                            };
                            let r = {
                                let mut games = self.wolf_games.lock();
                                let Some(g) = games.get_mut(chat_id) else { return };
                                let r = g.cast_vote(&oid, target_oid.as_deref());
                                if r.is_ok() {
                                    self.persist_wolf_locked(chat_id, g);
                                }
                                r
                            };
                            match r {
                                Ok(()) => { decided = true; break; }
                                Err(e) => hist.push((
                                    format!(
                                        "{{\"target_idx\": {}}}",
                                        decision.target_idx.map(|i| i as i64).unwrap_or(-1)
                                    ),
                                    e.to_string(),
                                )),
                            }
                        }
                        if !decided {
                            // 兜底：弃权
                            let mut games = self.wolf_games.lock();
                            if let Some(g) = games.get_mut(chat_id) {
                                let _ = g.cast_vote(&oid, None);
                                self.persist_wolf_locked(chat_id, g);
                            }
                        }
                        tokio::time::sleep(Duration::from_millis(300)).await;
                    }

                    // 2. 是否所有人都投了？
                    let all_voted = {
                        let games = self.wolf_games.lock();
                        games.get(chat_id).map(|g| g.all_alive_voted()).unwrap_or(false)
                    };
                    if !all_voted {
                        // 给还没投的真人发投票卡。已经发过的（day_vote_msgs 里
                        // 有 msg_id）走 update_card 复用原卡，没发过的才 send。
                        // 不这样做的话每次有人投完触发 advance_wolf 都会给剩下
                        // 的人重发一张，3 真人桌剩最后那个会收 N 张卡刷屏。
                        let humans_pending: Vec<(String, Option<String>)> = {
                            let games = self.wolf_games.lock();
                            let Some(g) = games.get(chat_id) else { return };
                            g.alive_indices()
                                .into_iter()
                                .filter(|i| {
                                    !g.players[*i].is_ai
                                        && g.day_votes.for_voter(*i).is_none()
                                })
                                .map(|i| {
                                    let oid = g.players[i].open_id.clone();
                                    let existing = g.day_vote_msg(&oid).map(String::from);
                                    (oid, existing)
                                })
                                .collect()
                        };
                        for (oid, existing) in humans_pending {
                            // 投票卡是静态的（候选不变，看不到别人怎么投）—— 发过一次
                            // 就别再发了。advance_wolf 每次有人投完都会进这条路径，
                            // 重发会刷屏。
                            if existing.is_some() {
                                continue;
                            }
                            let card = {
                                let games = self.wolf_games.lock();
                                let Some(g) = games.get(chat_id) else { return };
                                let Some(p_idx) = g.find_player(&oid) else { continue };
                                build_vote_card(g, &g.players[p_idx])
                            };
                            if let Ok(new_id) = self
                                .client
                                .send_ephemeral_card(chat_id, &oid, &card)
                                .await
                            {
                                let mut games = self.wolf_games.lock();
                                if let Some(g) = games.get_mut(chat_id) {
                                    g.set_day_vote_msg(&oid, new_id);
                                    self.persist_wolf_locked(chat_id, g);
                                }
                            }
                        }
                        return;
                    }

                    // 3. 全员投完，结算
                    let _ = {
                        let mut games = self.wolf_games.lock();
                        let Some(g) = games.get_mut(chat_id) else { return };
                        let r = g.resolve_lynch();
                        if r.is_ok() {
                            self.persist_wolf_locked(chat_id, g);
                        }
                        r
                    };

                    // 4. 广播投票结果
                    let game = {
                        let games = self.wolf_games.lock();
                        games.get(chat_id).cloned()
                    };
                    if let Some(g) = game {
                        let c = build_vote_tally_card(&g);
                        let _ = self
                            .client
                            .send_message("chat_id", chat_id, "interactive", &c)
                            .await;
                    }

                    // 5. 推进
                    {
                        let mut games = self.wolf_games.lock();
                        let Some(g) = games.get_mut(chat_id) else { return };
                        if let Err(e) = g.advance_after_vote() {
                            warn!(?e, "advance_after_vote failed");
                            return;
                        }
                        self.persist_wolf_locked(chat_id, g);
                    }
                }

                Stage::HunterShoot => {
                    let (h_idx, is_ai, h_oid) = {
                        let games = self.wolf_games.lock();
                        let Some(g) = games.get(chat_id) else { return };
                        let Some(idx) = g.pending_hunter else { return };
                        let p = &g.players[idx];
                        (idx, p.is_ai, p.open_id.clone())
                    };

                    if !is_ai {
                        let game = {
                            let games = self.wolf_games.lock();
                            games.get(chat_id).cloned()
                        };
                        if let Some(g) = game {
                            let c = build_hunter_card(&g, &g.players[h_idx]);
                            let _ = self
                                .client
                                .send_ephemeral_card(chat_id, &h_oid, &c)
                                .await;
                        }
                        return;
                    }

                    // 优先使用遗言阶段已经决策好的目标 —— 一次 LLM 调用搞定遗言+开枪
                    let pre_decided = {
                        let games = self.wolf_games.lock();
                        games.get(chat_id).and_then(|g| g.pending_hunter_ai_decision)
                    };
                    let mut shot_idx: Option<usize> = None;
                    let mut decided = false;

                    if let Some(target_opt) = pre_decided {
                        let target_oid = target_opt.and_then(|t| {
                            let games = self.wolf_games.lock();
                            games.get(chat_id).and_then(|g| {
                                g.players.get(t).map(|p| p.open_id.clone())
                            })
                        });
                        let r = {
                            let mut games = self.wolf_games.lock();
                            let Some(g) = games.get_mut(chat_id) else { return };
                            let r = g.hunter_shoot(&h_oid, target_oid.as_deref());
                            if r.is_ok() {
                                self.persist_wolf_locked(chat_id, g);
                            }
                            r
                        };
                        if let Ok(s) = r {
                            shot_idx = s;
                            decided = true;
                        }
                        // 如果预决目标因什么原因失败（如目标变化），落到下面 retry-with-feedback
                    }

                    // AI 猎人 retry-with-feedback（仅当预决不存在 / 失败时）
                    let mut hist: AttemptHistory = vec![];
                    if !decided {
                        for _ in 0..3 {
                            let target_opt = self.hunter_ai_pick(chat_id, h_idx, &hist).await;
                            let target_oid = target_opt.and_then(|t| {
                                let games = self.wolf_games.lock();
                                games.get(chat_id).and_then(|g| {
                                    g.players.get(t).map(|p| p.open_id.clone())
                                })
                            });
                            let r = {
                                let mut games = self.wolf_games.lock();
                                let Some(g) = games.get_mut(chat_id) else { return };
                                let r = g.hunter_shoot(&h_oid, target_oid.as_deref());
                                if r.is_ok() {
                                    self.persist_wolf_locked(chat_id, g);
                                }
                                r
                            };
                            match r {
                                Ok(s) => {
                                    shot_idx = s;
                                    decided = true;
                                    break;
                                }
                                Err(e) => hist.push((
                                    format!(
                                        "{{\"target_idx\": {}}}",
                                        target_opt.map(|i| i as i64).unwrap_or(-1)
                                    ),
                                    e.to_string(),
                                )),
                            }
                        }
                    }
                    if !decided {
                        // 兜底：不开枪
                        let mut games = self.wolf_games.lock();
                        if let Some(g) = games.get_mut(chat_id) {
                            let r = g.hunter_shoot(&h_oid, None);
                            if r.is_ok() {
                                self.persist_wolf_locked(chat_id, g);
                            }
                            shot_idx = r.ok().flatten();
                        }
                    }
                    let game = {
                        let games = self.wolf_games.lock();
                        games.get(chat_id).cloned()
                    };
                    if let Some(g) = game {
                        let target_player = shot_idx.map(|t| g.players[t].clone());
                        let announce = build_hunter_announce_card(
                            &g.players[h_idx],
                            target_player.as_ref(),
                        );
                        let _ = self
                            .client
                            .send_message("chat_id", chat_id, "interactive", &announce)
                            .await;
                    }
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
            }

            // 检查是否游戏结束
            let ended = {
                let games = self.wolf_games.lock();
                games
                    .get(chat_id)
                    .map(|g| g.stage == Stage::Ended)
                    .unwrap_or(false)
            };
            if ended {
                let game = {
                    let games = self.wolf_games.lock();
                    games.get(chat_id).cloned()
                };
                if let Some(g) = game {
                    if let Some(w) = g.victory() {
                        let c = build_summary_card(&g, w);
                        let _ = self
                            .client
                            .send_message("chat_id", chat_id, "interactive", &c)
                            .await;
                        info!(chat = %chat_id, winner = ?w, "wolf game ended");
                    }
                    // 把统一大厅卡推到最下面：清掉 poker game 里的 lobby_msg_id
                    if let Some(pg) = self.games.lock().get_mut(chat_id) {
                        pg.lobby_msg_id = None;
                        self.persist_locked(chat_id, pg);
                    }
                    let _ = self.refresh_lobby(chat_id).await;
                }
                return;
            }
        }
        let stuck_stage = {
            let games = self.wolf_games.lock();
            games.get(chat_id).map(|g| g.stage)
        };
        warn!(?stuck_stage, %chat_id, "advance_wolf hit iteration limit");
    }

    // ========================================================================
    // AI 决策包装（外部调用 LLM）
    // ========================================================================

    /// 把 LLM 返回的 thinking 落地到 game.thinking_log，下次同一玩家做决策时
    /// 能在 prompt 里看到自己上几轮的内心独白，保持策略弧线的连贯性。
    /// 严格私密：只属于该 player，绝不会被其他 AI 在 build_view 里看到。
    fn save_thinking(
        &self,
        chat_id: &str,
        player: usize,
        kind: ThinkingKind,
        thinking: Option<String>,
    ) {
        let Some(t) = thinking else { return };
        let mut games = self.wolf_games.lock();
        if let Some(g) = games.get_mut(chat_id) {
            g.push_thinking(player, kind, t);
            self.persist_wolf_locked(chat_id, g);
        }
    }

    /// AI 狼决策：返回目标 idx + 可选的狼频道发言。
    async fn wolf_ai_pick(
        &self,
        chat_id: &str,
        ai_idx: usize,
        speak_enabled: bool,
        history: &AttemptHistory,
    ) -> wolf_llm::WolfPickDecision {
        let game = {
            let games = self.wolf_games.lock();
            games.get(chat_id).cloned()
        };
        let Some(game) = game else {
            return wolf_llm::WolfPickDecision { target_idx: 0, chat: None };
        };
        let view = wolf_llm::build_view(&game, ai_idx);
        match &self.llm {
            Some(llm) => {
                let (decision, thinking) =
                    wolf_llm::wolf_pick(llm, &view, &game, speak_enabled, history).await;
                self.save_thinking(chat_id, ai_idx, ThinkingKind::WolfPick, thinking);
                decision
            }
            None => {
                // fallback: 选第一个非狼存活
                let target_idx = game
                    .players
                    .iter()
                    .enumerate()
                    .find(|(i, p)| p.alive && !game.is_wolf(*i))
                    .map(|(i, _)| i)
                    .unwrap_or(0);
                wolf_llm::WolfPickDecision { target_idx, chat: None }
            }
        }
    }

    /// 给某只人类狼发夜间卡（首次 send，再次 update_card）。
    async fn send_or_update_wolf_night_card(
        &self,
        chat_id: &str,
        wolf_idx: usize,
        wolf_open_id: &str,
    ) {
        let (card, existing_msg_id) = {
            let games = self.wolf_games.lock();
            let Some(g) = games.get(chat_id) else { return };
            let card = build_wolf_night_card(g, &g.players[wolf_idx]);
            let existing = g.wolf_night_msg(wolf_idx).map(String::from);
            (card, existing)
        };

        // ephemeral 卡片不能用 PATCH 更新，必须 delete 旧的 + send 新的，
        // 否则每次状态变更都堆一张新卡刷屏。失败也不致命（旧卡留着，看着不爽但不会卡逻辑）。
        if let Some(old_id) = &existing_msg_id {
            let _ = self.client.delete_ephemeral(old_id).await;
        }
        match self
            .client
            .send_ephemeral_card(chat_id, wolf_open_id, &card)
            .await
        {
            Ok(new_id) => {
                let mut games = self.wolf_games.lock();
                if let Some(g) = games.get_mut(chat_id) {
                    g.set_wolf_night_msg(wolf_idx, new_id);
                    self.persist_wolf_locked(chat_id, g);
                }
            }
            Err(e) => warn!(?e, %wolf_open_id, "failed to send wolf night card"),
        }
    }

    /// 广播：把所有人类狼的夜间卡都更新一遍。给聊天 / 进度 / 就绪状态变化用。
    pub(crate) async fn broadcast_wolf_night_update(&self, chat_id: &str) {
        let wolves: Vec<(usize, String)> = {
            let games = self.wolf_games.lock();
            let Some(g) = games.get(chat_id) else { return };
            g.alive_wolves()
                .into_iter()
                .filter(|w| !g.players[*w].is_ai)
                .map(|i| (i, g.players[i].open_id.clone()))
                .collect()
        };
        for (wolf_idx, oid) in wolves {
            self.send_or_update_wolf_night_card(chat_id, wolf_idx, &oid).await;
        }
    }

    async fn seer_ai_pick(
        &self,
        chat_id: &str,
        ai_idx: usize,
        history: &AttemptHistory,
    ) -> usize {
        let game = {
            let games = self.wolf_games.lock();
            games.get(chat_id).cloned()
        };
        let Some(game) = game else { return 0 };
        let view = wolf_llm::build_view(&game, ai_idx);
        match &self.llm {
            Some(llm) => {
                let (target, thinking) = wolf_llm::seer_pick(llm, &view, history).await;
                self.save_thinking(chat_id, ai_idx, ThinkingKind::SeerCheck, thinking);
                target
            }
            None => game
                .players
                .iter()
                .enumerate()
                .find(|(i, p)| p.alive && *i != ai_idx)
                .map(|(i, _)| i)
                .unwrap_or(0),
        }
    }

    async fn witch_ai_decide(
        &self,
        chat_id: &str,
        ai_idx: usize,
        history: &AttemptHistory,
    ) -> wolf_llm::WitchDecision {
        let game = {
            let games = self.wolf_games.lock();
            games.get(chat_id).cloned()
        };
        let Some(game) = game else {
            return wolf_llm::WitchDecision::Skip;
        };
        let view = wolf_llm::build_view(&game, ai_idx);
        match &self.llm {
            Some(llm) => {
                let (decision, thinking) =
                    wolf_llm::witch_decide(llm, &view, &game, history).await;
                self.save_thinking(chat_id, ai_idx, ThinkingKind::WitchAct, thinking);
                decision
            }
            None => wolf_llm::WitchDecision::Skip,
        }
    }

    async fn vote_ai_pick(
        &self,
        chat_id: &str,
        ai_idx: usize,
        history: &AttemptHistory,
    ) -> wolf_llm::VoteDecision {
        let game = {
            let games = self.wolf_games.lock();
            games.get(chat_id).cloned()
        };
        let Some(game) = game else {
            return wolf_llm::VoteDecision { target_idx: None };
        };
        let view = wolf_llm::build_view(&game, ai_idx);
        match &self.llm {
            Some(llm) => {
                let (decision, thinking) = wolf_llm::vote_pick(llm, &view, history).await;
                self.save_thinking(chat_id, ai_idx, ThinkingKind::DayVote, thinking);
                decision
            }
            None => wolf_llm::VoteDecision { target_idx: None },
        }
    }

    async fn hunter_ai_pick(
        &self,
        chat_id: &str,
        ai_idx: usize,
        history: &AttemptHistory,
    ) -> Option<usize> {
        let game = {
            let games = self.wolf_games.lock();
            games.get(chat_id).cloned()
        };
        let Some(game) = game else { return None };
        let view = wolf_llm::build_view(&game, ai_idx);
        match &self.llm {
            Some(llm) => {
                let (target, thinking) = wolf_llm::hunter_pick(llm, &view, history).await;
                self.save_thinking(chat_id, ai_idx, ThinkingKind::HunterShoot, thinking);
                target
            }
            None => None,
        }
    }

    async fn sheriff_speech_ai(&self, chat_id: &str, ai_idx: usize) -> String {
        let game = {
            let games = self.wolf_games.lock();
            games.get(chat_id).cloned()
        };
        let Some(game) = game else { return String::new() };
        let view = wolf_llm::build_view(&game, ai_idx);
        match &self.llm {
            Some(llm) => {
                let (speech, thinking) = wolf_llm::sheriff_speech(llm, &view).await;
                self.save_thinking(chat_id, ai_idx, ThinkingKind::SheriffSpeech, thinking);
                speech
            }
            None => "我支持公平选举。".into(),
        }
    }

    async fn day_speech_ai(&self, chat_id: &str, ai_idx: usize) -> String {
        let game = {
            let games = self.wolf_games.lock();
            games.get(chat_id).cloned()
        };
        let Some(game) = game else { return String::new() };
        let view = wolf_llm::build_view(&game, ai_idx);
        match &self.llm {
            Some(llm) => {
                let (speech, thinking) = wolf_llm::day_speech(llm, &view).await;
                self.save_thinking(chat_id, ai_idx, ThinkingKind::DaySpeech, thinking);
                speech
            }
            None => String::new(),
        }
    }

    async fn sheriff_direction_ai(&self, chat_id: &str, ai_idx: usize) -> bool {
        let game = {
            let games = self.wolf_games.lock();
            games.get(chat_id).cloned()
        };
        let Some(game) = game else { return true };
        let view = wolf_llm::build_view(&game, ai_idx);
        match &self.llm {
            Some(llm) => {
                let (clockwise, thinking) = wolf_llm::sheriff_direction(llm, &view).await;
                self.save_thinking(chat_id, ai_idx, ThinkingKind::SheriffDirection, thinking);
                clockwise
            }
            None => true, // 默认警上
        }
    }

    /// 倒地猎人 / 狼王的合并决策：遗言 + 开枪目标 一次 LLM 调用搞定。
    /// retry-with-feedback：如果 LLM 给的开枪目标违法（死人 / 自己），
    /// 把错误反馈让 LLM 重选一遍。
    async fn dying_shooter_combined_ai(
        &self,
        chat_id: &str,
        ai_idx: usize,
    ) -> wolf_llm::DyingShooterDecision {
        let game_snapshot = {
            let games = self.wolf_games.lock();
            games.get(chat_id).cloned()
        };
        let Some(game) = game_snapshot else {
            return wolf_llm::DyingShooterDecision { speech: String::new(), target: None };
        };
        let view = wolf_llm::build_view(&game, ai_idx);
        let mut hist: AttemptHistory = vec![];
        // 候选目标：alive 且非自己
        let valid_targets: std::collections::HashSet<usize> = game
            .players
            .iter()
            .enumerate()
            .filter(|(i, p)| p.alive && *i != ai_idx)
            .map(|(i, _)| i)
            .collect();
        for _ in 0..3 {
            let llm = match &self.llm {
                Some(l) => l,
                None => {
                    return wolf_llm::DyingShooterDecision { speech: String::new(), target: None };
                }
            };
            let (decision, thinking) =
                wolf_llm::dying_hunter_combined(llm, &view, &hist).await;
            // 验证目标合法性
            match decision.target {
                None => {
                    self.save_thinking(chat_id, ai_idx, ThinkingKind::DyingShoot, thinking);
                    return decision;
                }
                Some(idx) if valid_targets.contains(&idx) => {
                    self.save_thinking(chat_id, ai_idx, ThinkingKind::DyingShoot, thinking);
                    return decision;
                }
                Some(idx) => {
                    hist.push((
                        format!(
                            "{{\"speech\": \"{}\", \"target_idx\": {}}}",
                            decision.speech.replace('"', "'"),
                            idx
                        ),
                        format!("idx {} 不是合法目标（必须是存活的非自己玩家）", idx),
                    ));
                }
            }
        }
        // 3 次都没给合法答案 → 留遗言不开枪
        wolf_llm::DyingShooterDecision { speech: String::new(), target: None }
    }

    async fn last_words_ai(&self, chat_id: &str, ai_idx: usize) -> String {
        let game = {
            let games = self.wolf_games.lock();
            games.get(chat_id).cloned()
        };
        let Some(game) = game else { return String::new() };
        let view = wolf_llm::build_view(&game, ai_idx);
        match &self.llm {
            Some(llm) => {
                let (speech, thinking) = wolf_llm::last_words(llm, &view).await;
                self.save_thinking(chat_id, ai_idx, ThinkingKind::LastWords, thinking);
                speech
            }
            None => String::new(),
        }
    }

    /// 公开发言卡（兼容旧接口：sheriff = SheriffMain，否则 Day）。
    async fn refresh_speech_public(&self, chat_id: &str, sheriff: bool) {
        let kind = if sheriff {
            SpeechKind::SheriffMain
        } else {
            SpeechKind::Day
        };
        self.refresh_speech_public_kind(chat_id, kind).await;
    }

    async fn refresh_speech_public_kind(&self, chat_id: &str, kind: SpeechKind) {
        let (card_value, existing_msg) = {
            let games = self.wolf_games.lock();
            let Some(g) = games.get(chat_id) else { return };
            let (title, template, order, speeches, cur_idx, existing) = match kind {
                SpeechKind::SheriffMain => (
                    "🎙️ 上警·竞选发言",
                    "yellow",
                    g.sheriff_speech_order.clone(),
                    g.sheriff_speeches.clone(),
                    g.sheriff_speech_idx,
                    g.sheriff_speech_public_msg.clone(),
                ),
                SpeechKind::Day => (
                    "🎙️ 白天·轮流发言",
                    "wathet",
                    g.day_speech_order.clone(),
                    g.day_speeches.clone(),
                    g.day_speech_idx,
                    g.day_speech_public_msg.clone(),
                ),
            };
            let c = build_speech_public_card(g, title, template, &order, &speeches, cur_idx);
            (c, existing)
        };

        if let Some(msg_id) = existing_msg {
            if self.client.update_card(&msg_id, &card_value).await.is_ok() {
                return;
            }
        }
        match self
            .client
            .send_message("chat_id", chat_id, "interactive", &card_value)
            .await
        {
            Ok(new_id) => {
                let mut games = self.wolf_games.lock();
                if let Some(g) = games.get_mut(chat_id) {
                    match kind {
                        SpeechKind::SheriffMain => g.sheriff_speech_public_msg = Some(new_id),
                        SpeechKind::Day => g.day_speech_public_msg = Some(new_id),
                    }
                    self.persist_wolf_locked(chat_id, g);
                }
            }
            Err(e) => warn!(?e, %chat_id, "speech public card send failed"),
        }
    }

    async fn refresh_last_words_public(&self, chat_id: &str) {
        let (card_value, existing_msg) = {
            let games = self.wolf_games.lock();
            let Some(g) = games.get(chat_id) else { return };
            let c = build_last_words_public_card(
                g,
                &g.last_words_queue,
                &g.last_words_speeches,
                g.last_words_idx,
            );
            (c, g.last_words_public_msg.clone())
        };
        if let Some(msg_id) = existing_msg {
            if self.client.update_card(&msg_id, &card_value).await.is_ok() {
                return;
            }
        }
        if let Ok(new_id) = self
            .client
            .send_message("chat_id", chat_id, "interactive", &card_value)
            .await
        {
            let mut games = self.wolf_games.lock();
            if let Some(g) = games.get_mut(chat_id) {
                g.last_words_public_msg = Some(new_id);
                self.persist_wolf_locked(chat_id, g);
            }
        }
    }

    async fn send_or_update_last_words_private(
        &self,
        chat_id: &str,
        speaker_idx: usize,
        speaker_open_id: &str,
    ) {
        let (card_value, existing_msg) = {
            let games = self.wolf_games.lock();
            let Some(g) = games.get(chat_id) else { return };
            let c = build_last_words_private_card(g, &g.players[speaker_idx]);
            (c, g.last_words_private_msg.clone())
        };
        // ephemeral 不能 PATCH：先删后发
        if let Some(old) = &existing_msg {
            let _ = self.client.delete_ephemeral(old).await;
        }
        if let Ok(new_id) = self
            .client
            .send_ephemeral_card(chat_id, speaker_open_id, &card_value)
            .await
        {
            let mut games = self.wolf_games.lock();
            if let Some(g) = games.get_mut(chat_id) {
                g.last_words_private_msg = Some(new_id);
                self.persist_wolf_locked(chat_id, g);
            }
        }
    }

    /// 给当前发言人发私密输入卡。首发 → 存 msg_id；后续切换发言人时旧卡作废。
    async fn send_or_update_speech_private(
        &self,
        chat_id: &str,
        speaker_idx: usize,
        speaker_open_id: &str,
        sheriff: bool,
    ) {
        let (card_value, existing_msg) = {
            let games = self.wolf_games.lock();
            let Some(g) = games.get(chat_id) else { return };
            let (title, template, submit_action, skip_action, placeholder, existing) = if sheriff {
                (
                    "🎤 你的竞选发言",
                    "yellow",
                    "wolf_sheriff_speech_submit",
                    "wolf_sheriff_speech_skip",
                    "拉票 / 自报身份 / 攻击对手…",
                    g.sheriff_speech_private_msg.clone(),
                )
            } else {
                (
                    "🎤 你的白天发言",
                    "wathet",
                    "wolf_day_speech_submit",
                    "wolf_day_speech_skip",
                    "分析 / 报查验 / 表态…",
                    g.day_speech_private_msg.clone(),
                )
            };
            let viewer = &g.players[speaker_idx];
            let c = build_speech_private_card(
                g,
                viewer,
                title,
                template,
                submit_action,
                skip_action,
                placeholder,
            );
            (c, existing)
        };

        // ephemeral 不能 PATCH：先删后发
        if let Some(old) = &existing_msg {
            let _ = self.client.delete_ephemeral(old).await;
        }
        match self
            .client
            .send_ephemeral_card(chat_id, speaker_open_id, &card_value)
            .await
        {
            Ok(new_id) => {
                let mut games = self.wolf_games.lock();
                if let Some(g) = games.get_mut(chat_id) {
                    if sheriff {
                        g.sheriff_speech_private_msg = Some(new_id);
                    } else {
                        g.day_speech_private_msg = Some(new_id);
                    }
                    self.persist_wolf_locked(chat_id, g);
                }
            }
            Err(e) => warn!(?e, %speaker_open_id, "speech private card send failed"),
        }
    }

    async fn guard_ai_pick(
        &self,
        chat_id: &str,
        ai_idx: usize,
        history: &AttemptHistory,
    ) -> usize {
        let game = {
            let games = self.wolf_games.lock();
            games.get(chat_id).cloned()
        };
        let Some(game) = game else { return ai_idx };
        let view = wolf_llm::build_view(&game, ai_idx);
        match &self.llm {
            Some(llm) => {
                let (target, thinking) =
                    wolf_llm::guard_pick(llm, &view, &game, history).await;
                self.save_thinking(chat_id, ai_idx, ThinkingKind::GuardPick, thinking);
                target
            }
            None => {
                // fallback：守自己（如果上夜没守过自己）；否则任意非昨守目标
                if game.last_guard_target != Some(ai_idx) {
                    ai_idx
                } else {
                    game.alive_indices()
                        .into_iter()
                        .find(|i| game.last_guard_target != Some(*i))
                        .unwrap_or(ai_idx)
                }
            }
        }
    }

    async fn sheriff_run_ai(&self, chat_id: &str, ai_idx: usize) -> bool {
        let game = {
            let games = self.wolf_games.lock();
            games.get(chat_id).cloned()
        };
        let Some(game) = game else { return false };
        let view = wolf_llm::build_view(&game, ai_idx);
        match &self.llm {
            Some(llm) => {
                let (run, thinking) = wolf_llm::sheriff_run(llm, &view).await;
                self.save_thinking(chat_id, ai_idx, ThinkingKind::SheriffRun, thinking);
                run
            }
            // fallback: 预言家上警，其他人不上
            None => game.players[ai_idx].role == Some(crate::werewolf::game::Role::Seer),
        }
    }

    async fn sheriff_vote_ai(
        &self,
        chat_id: &str,
        ai_idx: usize,
        history: &AttemptHistory,
    ) -> Option<usize> {
        let (game, candidates) = {
            let games = self.wolf_games.lock();
            let g = games.get(chat_id).cloned();
            let cands = g
                .as_ref()
                .map(|g| {
                    g.sheriff_candidates()
                        .into_iter()
                        .map(|i| (i, g.players[i].name.clone()))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            (g, cands)
        };
        let Some(game) = game else { return None };
        let view = wolf_llm::build_view(&game, ai_idx);
        match &self.llm {
            Some(llm) => {
                let (target, thinking) =
                    wolf_llm::sheriff_vote(llm, &view, &candidates, history).await;
                self.save_thinking(chat_id, ai_idx, ThinkingKind::SheriffVote, thinking);
                target
            }
            None => candidates.first().map(|(i, _)| *i),
        }
    }

    async fn badge_pass_ai(
        &self,
        chat_id: &str,
        ai_idx: usize,
        history: &AttemptHistory,
    ) -> Option<usize> {
        let game = {
            let games = self.wolf_games.lock();
            games.get(chat_id).cloned()
        };
        let Some(game) = game else { return None };
        let view = wolf_llm::build_view(&game, ai_idx);
        match &self.llm {
            Some(llm) => {
                let (target, thinking) = wolf_llm::badge_pass(llm, &view, history).await;
                self.save_thinking(chat_id, ai_idx, ThinkingKind::BadgePass, thinking);
                target
            }
            None => None,
        }
    }
}
