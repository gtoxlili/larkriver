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
use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::time::Duration;
use tracing::{info, warn};

/// 三种顺序发言的语义区分。
enum SpeechKind {
    SheriffMain,
    SheriffSide,
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
            "wolf_sheriff_side_submit" => {
                let speech = action
                    .form_value
                    .get("speech")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                self.submit_sheriff_side_and_advance(&chat_id, &action.open_id, speech).await
            }
            "wolf_sheriff_side_skip" => {
                self.submit_sheriff_side_and_advance(&chat_id, &action.open_id, String::new())
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

    async fn submit_sheriff_side_and_advance(
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
            let r = g.submit_sheriff_side_speech(speaker_open_id, speech);
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
                    // AI 守卫
                    let target_idx = self.guard_ai_pick(chat_id, g_idx).await;
                    let target_oid = {
                        let games = self.wolf_games.lock();
                        games
                            .get(chat_id)
                            .and_then(|g| g.players.get(target_idx).map(|p| p.open_id.clone()))
                    };
                    let Some(t) = target_oid else { return };
                    {
                        let mut games = self.wolf_games.lock();
                        let Some(g) = games.get_mut(chat_id) else { return };
                        if let Err(e) = g.guard_pick(&g_oid, &t) {
                            warn!(?e, "AI guard_pick failed, falling back to self");
                            // 退回到守自己
                            let _ = g.guard_pick(&g_oid, &g_oid);
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
                        let target_idx = self.sheriff_vote_ai(chat_id, idx).await;
                        let target_oid = target_idx.and_then(|t| {
                            let games = self.wolf_games.lock();
                            games
                                .get(chat_id)
                                .and_then(|g| g.players.get(t).map(|p| p.open_id.clone()))
                        });
                        {
                            let mut games = self.wolf_games.lock();
                            if let Some(g) = games.get_mut(chat_id) {
                                let _ = g.cast_sheriff_vote(&oid, target_oid.as_deref());
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
                    // AI 警长
                    let target_idx = self.badge_pass_ai(chat_id, h_idx).await;
                    let target_oid = target_idx.and_then(|t| {
                        let games = self.wolf_games.lock();
                        games
                            .get(chat_id)
                            .and_then(|g| g.players.get(t).map(|p| p.open_id.clone()))
                    });
                    let new_holder = {
                        let mut games = self.wolf_games.lock();
                        let Some(g) = games.get_mut(chat_id) else { return };
                        let r = g.transfer_badge(&h_oid, target_oid.as_deref());
                        if r.is_ok() {
                            self.persist_wolf_locked(chat_id, g);
                        }
                        r.ok().flatten()
                    };
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
                            let decision = self.wolf_ai_pick(chat_id, idx, false).await;
                            let target_open_id = {
                                let games = self.wolf_games.lock();
                                games.get(chat_id).and_then(|g| {
                                    g.players.get(decision.target_idx).map(|p| p.open_id.clone())
                                })
                            };
                            if let Some(t) = target_open_id {
                                let _ = self.apply_wolf_kill(chat_id, &open_id, &t);
                            }
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
                        let decision = self.wolf_ai_pick(chat_id, idx, true).await;
                        let target_open_id = {
                            let games = self.wolf_games.lock();
                            games.get(chat_id).and_then(|g| {
                                g.players.get(decision.target_idx).map(|p| p.open_id.clone())
                            })
                        };
                        if let Some(t) = target_open_id {
                            let _ = self.apply_wolf_kill(chat_id, &open_id, &t);
                        }
                        // AI 发言（如果 LLM 返回了）+ 自动就绪
                        {
                            let mut games = self.wolf_games.lock();
                            if let Some(g) = games.get_mut(chat_id) {
                                if let Some(msg) = decision.chat {
                                    let _ = g.wolf_say(&open_id, msg);
                                }
                                let _ = g.wolf_mark_ready(&open_id);
                                self.persist_wolf_locked(chat_id, g);
                            }
                        }
                        // 同步给所有狼的卡片
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

                    // AI 预言家
                    let target_idx = self.seer_ai_pick(chat_id, seer_idx).await;
                    let target_oid = {
                        let games = self.wolf_games.lock();
                        games
                            .get(chat_id)
                            .and_then(|g| g.players.get(target_idx).map(|p| p.open_id.clone()))
                    };
                    let Some(target) = target_oid else { return };
                    {
                        let mut games = self.wolf_games.lock();
                        let Some(g) = games.get_mut(chat_id) else { return };
                        if let Err(e) = g.seer_check(&seer_oid, &target) {
                            warn!(?e, "AI seer_check failed");
                            return;
                        }
                        self.persist_wolf_locked(chat_id, g);
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

                    // AI 女巫
                    let decision = self.witch_ai_decide(chat_id, witch_idx).await;
                    {
                        let mut games = self.wolf_games.lock();
                        let Some(g) = games.get_mut(chat_id) else { return };
                        let r = match decision {
                            wolf_llm::WitchDecision::Save => g.witch_act(&witch_oid, true, None),
                            wolf_llm::WitchDecision::Poison(idx) => {
                                let target_oid = g.players[idx].open_id.clone();
                                g.witch_act(&witch_oid, false, Some(&target_oid))
                            }
                            wolf_llm::WitchDecision::Skip => g.witch_act(&witch_oid, false, None),
                        };
                        if let Err(e) = r {
                            warn!(?e, "AI witch_act failed");
                            // 自动跳过避免死锁
                            let _ = g.witch_act(&witch_oid, false, None);
                        }
                        self.persist_wolf_locked(chat_id, g);
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
                            let _ = g.finish_last_words();
                            self.persist_wolf_locked(chat_id, g);
                        }
                        continue;
                    };
                    if is_ai {
                        let text = self.last_words_ai(chat_id, spk_idx).await;
                        {
                            let mut games = self.wolf_games.lock();
                            if let Some(g) = games.get_mut(chat_id) {
                                let _ = g.submit_last_words(&spk_oid, text);
                                self.persist_wolf_locked(chat_id, g);
                            }
                        }
                        tokio::time::sleep(Duration::from_millis(700)).await;
                    } else {
                        self.send_or_update_last_words_private(chat_id, spk_idx, &spk_oid)
                            .await;
                        return;
                    }
                }

                Stage::SheriffSideSpeech => {
                    self.refresh_speech_public_kind(chat_id, SpeechKind::SheriffSide).await;
                    let current: Option<(usize, String, bool)> = {
                        let games = self.wolf_games.lock();
                        let Some(g) = games.get(chat_id) else { return };
                        g.current_sheriff_side_speaker().map(|i| {
                            let p = &g.players[i];
                            (i, p.open_id.clone(), p.is_ai)
                        })
                    };
                    let Some((spk_idx, spk_oid, is_ai)) = current else {
                        let mut games = self.wolf_games.lock();
                        if let Some(g) = games.get_mut(chat_id) {
                            let _ = g.finish_sheriff_side_speeches();
                            self.persist_wolf_locked(chat_id, g);
                        }
                        continue;
                    };
                    if is_ai {
                        let text = self.sheriff_side_speech_ai(chat_id, spk_idx).await;
                        {
                            let mut games = self.wolf_games.lock();
                            if let Some(g) = games.get_mut(chat_id) {
                                let _ = g.submit_sheriff_side_speech(&spk_oid, text);
                                self.persist_wolf_locked(chat_id, g);
                            }
                        }
                        tokio::time::sleep(Duration::from_millis(600)).await;
                    } else {
                        self.send_or_update_sheriff_side_private(chat_id, spk_idx, &spk_oid)
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
                                let _ = g.finish_sheriff_speeches();
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
                                let _ = g.submit_sheriff_speech(&spk_oid, text);
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
                                let _ = g.enter_day_vote();
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
                                let _ = g.submit_day_speech(&spk_oid, text);
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
                        let decision = self.vote_ai_pick(chat_id, idx).await;
                        let (target_oid, name, persona) = {
                            let games = self.wolf_games.lock();
                            let Some(g) = games.get(chat_id) else { return };
                            let p = &g.players[idx];
                            let t = decision
                                .target_idx
                                .and_then(|t| g.players.get(t))
                                .map(|p| p.open_id.clone());
                            (t, p.name.clone(), p.persona)
                        };
                        {
                            let mut games = self.wolf_games.lock();
                            if let Some(g) = games.get_mut(chat_id) {
                                let _ = g.cast_vote(&oid, target_oid.as_deref());
                                self.persist_wolf_locked(chat_id, g);
                            }
                        }
                        if let Some(q) = decision.quip {
                            let emoji = persona.map(|p| p.emoji()).unwrap_or("💬");
                            let post = build_ai_quip_post(emoji, &name, &q);
                            let _ = self
                                .client
                                .send_message("chat_id", chat_id, "post", &post)
                                .await;
                        }
                        tokio::time::sleep(Duration::from_millis(400)).await;
                    }

                    // 2. 是否所有人都投了？
                    let all_voted = {
                        let games = self.wolf_games.lock();
                        games.get(chat_id).map(|g| g.all_alive_voted()).unwrap_or(false)
                    };
                    if !all_voted {
                        // 给还没投的人类发投票卡
                        let humans_pending: Vec<String> = {
                            let games = self.wolf_games.lock();
                            let Some(g) = games.get(chat_id) else { return };
                            g.alive_indices()
                                .into_iter()
                                .filter(|i| {
                                    !g.players[*i].is_ai
                                        && g.day_votes.for_voter(*i).is_none()
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
                                    let c = build_vote_card(&g, &g.players[p_idx]);
                                    let _ = self
                                        .client
                                        .send_ephemeral_card(chat_id, &oid, &c)
                                        .await;
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

                    // AI 猎人
                    let target_idx = self.hunter_ai_pick(chat_id, h_idx).await;
                    let target_oid = target_idx.and_then(|t| {
                        let games = self.wolf_games.lock();
                        games
                            .get(chat_id)
                            .and_then(|g| g.players.get(t).map(|p| p.open_id.clone()))
                    });
                    let shot_idx = {
                        let mut games = self.wolf_games.lock();
                        let Some(g) = games.get_mut(chat_id) else { return };
                        let r = g.hunter_shoot(&h_oid, target_oid.as_deref());
                        if r.is_ok() {
                            self.persist_wolf_locked(chat_id, g);
                        }
                        r.ok().flatten()
                    };
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
        warn!("advance_wolf hit iteration limit");
    }

    // ========================================================================
    // AI 决策包装（外部调用 LLM）
    // ========================================================================

    /// AI 狼决策：返回目标 idx + 可选的狼频道发言。
    async fn wolf_ai_pick(
        &self,
        chat_id: &str,
        ai_idx: usize,
        speak_enabled: bool,
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
            Some(llm) => wolf_llm::wolf_pick(llm, &view, &game, speak_enabled).await,
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

        if let Some(msg_id) = existing_msg_id {
            if self.client.update_card(&msg_id, &card).await.is_ok() {
                return;
            }
            // update 失败（卡可能被删了），重发新卡
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

    async fn seer_ai_pick(&self, chat_id: &str, ai_idx: usize) -> usize {
        let game = {
            let games = self.wolf_games.lock();
            games.get(chat_id).cloned()
        };
        let Some(game) = game else { return 0 };
        let view = wolf_llm::build_view(&game, ai_idx);
        match &self.llm {
            Some(llm) => wolf_llm::seer_pick(llm, &view).await,
            None => game
                .players
                .iter()
                .enumerate()
                .find(|(i, p)| p.alive && *i != ai_idx)
                .map(|(i, _)| i)
                .unwrap_or(0),
        }
    }

    async fn witch_ai_decide(&self, chat_id: &str, ai_idx: usize) -> wolf_llm::WitchDecision {
        let game = {
            let games = self.wolf_games.lock();
            games.get(chat_id).cloned()
        };
        let Some(game) = game else {
            return wolf_llm::WitchDecision::Skip;
        };
        let view = wolf_llm::build_view(&game, ai_idx);
        match &self.llm {
            Some(llm) => wolf_llm::witch_decide(llm, &view, &game).await,
            None => wolf_llm::WitchDecision::Skip,
        }
    }

    async fn vote_ai_pick(&self, chat_id: &str, ai_idx: usize) -> wolf_llm::VoteDecision {
        let game = {
            let games = self.wolf_games.lock();
            games.get(chat_id).cloned()
        };
        let Some(game) = game else {
            return wolf_llm::VoteDecision { target_idx: None, quip: None };
        };
        let view = wolf_llm::build_view(&game, ai_idx);
        match &self.llm {
            Some(llm) => wolf_llm::vote_pick(llm, &view).await,
            None => wolf_llm::VoteDecision { target_idx: None, quip: None },
        }
    }

    async fn hunter_ai_pick(&self, chat_id: &str, ai_idx: usize) -> Option<usize> {
        let game = {
            let games = self.wolf_games.lock();
            games.get(chat_id).cloned()
        };
        let Some(game) = game else { return None };
        let view = wolf_llm::build_view(&game, ai_idx);
        match &self.llm {
            Some(llm) => wolf_llm::hunter_pick(llm, &view).await,
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
            Some(llm) => wolf_llm::sheriff_speech(llm, &view).await,
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
            Some(llm) => wolf_llm::day_speech(llm, &view).await,
            None => String::new(),
        }
    }

    async fn sheriff_side_speech_ai(&self, chat_id: &str, ai_idx: usize) -> String {
        let game = {
            let games = self.wolf_games.lock();
            games.get(chat_id).cloned()
        };
        let Some(game) = game else { return String::new() };
        let view = wolf_llm::build_view(&game, ai_idx);
        match &self.llm {
            Some(llm) => wolf_llm::sheriff_side_speech(llm, &view).await,
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
            Some(llm) => wolf_llm::sheriff_direction(llm, &view).await,
            None => true, // 默认警上
        }
    }

    async fn last_words_ai(&self, chat_id: &str, ai_idx: usize) -> String {
        let game = {
            let games = self.wolf_games.lock();
            games.get(chat_id).cloned()
        };
        let Some(game) = game else { return String::new() };
        let view = wolf_llm::build_view(&game, ai_idx);
        match &self.llm {
            Some(llm) => wolf_llm::last_words(llm, &view).await,
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
                SpeechKind::SheriffSide => (
                    "🎙️ 警下·非候选人发言",
                    "yellow",
                    g.sheriff_side_order.clone(),
                    g.sheriff_side_speeches.clone(),
                    g.sheriff_side_idx,
                    g.sheriff_side_public_msg.clone(),
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
                        SpeechKind::SheriffSide => g.sheriff_side_public_msg = Some(new_id),
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
        if let Some(msg_id) = existing_msg {
            if self.client.update_card(&msg_id, &card_value).await.is_ok() {
                return;
            }
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

    async fn send_or_update_sheriff_side_private(
        &self,
        chat_id: &str,
        speaker_idx: usize,
        speaker_open_id: &str,
    ) {
        let (card_value, existing_msg) = {
            let games = self.wolf_games.lock();
            let Some(g) = games.get(chat_id) else { return };
            let viewer = &g.players[speaker_idx];
            let c = build_speech_private_card(
                g,
                viewer,
                "🎤 警下发言",
                "yellow",
                "wolf_sheriff_side_submit",
                "wolf_sheriff_side_skip",
                "对警上候选人的看法…",
            );
            (c, g.sheriff_side_private_msg.clone())
        };
        if let Some(msg_id) = existing_msg {
            if self.client.update_card(&msg_id, &card_value).await.is_ok() {
                return;
            }
        }
        if let Ok(new_id) = self
            .client
            .send_ephemeral_card(chat_id, speaker_open_id, &card_value)
            .await
        {
            let mut games = self.wolf_games.lock();
            if let Some(g) = games.get_mut(chat_id) {
                g.sheriff_side_private_msg = Some(new_id);
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

        if let Some(msg_id) = existing_msg {
            if self.client.update_card(&msg_id, &card_value).await.is_ok() {
                return;
            }
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

    async fn guard_ai_pick(&self, chat_id: &str, ai_idx: usize) -> usize {
        let game = {
            let games = self.wolf_games.lock();
            games.get(chat_id).cloned()
        };
        let Some(game) = game else { return ai_idx };
        let view = wolf_llm::build_view(&game, ai_idx);
        match &self.llm {
            Some(llm) => wolf_llm::guard_pick(llm, &view, &game).await,
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
            Some(llm) => wolf_llm::sheriff_run(llm, &view).await,
            // fallback: 预言家上警，其他人不上
            None => game.players[ai_idx].role == Some(crate::werewolf::game::Role::Seer),
        }
    }

    async fn sheriff_vote_ai(&self, chat_id: &str, ai_idx: usize) -> Option<usize> {
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
            Some(llm) => wolf_llm::sheriff_vote(llm, &view, &candidates).await,
            None => candidates.first().map(|(i, _)| *i),
        }
    }

    async fn badge_pass_ai(&self, chat_id: &str, ai_idx: usize) -> Option<usize> {
        let game = {
            let games = self.wolf_games.lock();
            games.get(chat_id).cloned()
        };
        let Some(game) = game else { return None };
        let view = wolf_llm::build_view(&game, ai_idx);
        match &self.llm {
            Some(llm) => wolf_llm::badge_pass(llm, &view).await,
            None => None,
        }
    }
}
