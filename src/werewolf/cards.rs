//! 狼人杀的卡片渲染。
//!
//! 命名约定：所有 callback action id 以 `wolf_` 前缀，避免和德州扑克冲突。
//!
//! 关键卡片：
//! - lobby：持久大厅卡，原地更新
//! - role reveal：开局后给每位玩家发的角色专属 ephemeral
//! - wolf night：给每个存活狼人发的"今晚杀谁"
//! - seer night：给预言家发的"查谁"
//! - witch night：给女巫发的"救/毒/跳过"（带今晚谁要死的提示）
//! - day reveal：公开广播昨夜死讯
//! - day vote：每个存活玩家收到的投票卡
//! - hunter shoot：猎人收到的"开枪带谁"
//! - summary：游戏结束后公开的真身揭晓 + 阵营胜利

use crate::feishu::cards::*;
use crate::werewolf::game::*;
use serde_json::{json, Value};

// ============================================================================
// 通用工具
// ============================================================================

/// 渲染玩家名（人 → @-mention，AI → 加粗 + emoji）。
pub fn display_name(p: &Player) -> String {
    if p.is_ai {
        let emoji = p
            .persona
            .map(|persona| persona.emoji())
            .unwrap_or("🤖");
        format!("{} **{}**", emoji, p.name)
    } else {
        at(&p.open_id)
    }
}

/// 把 base value 和增量合并成单个 callback value。
fn merge(a: &Value, b: &Value) -> Value {
    let mut out = a.clone();
    if let (Some(o), Some(bm)) = (out.as_object_mut(), b.as_object()) {
        for (k, v) in bm {
            o.insert(k.clone(), v.clone());
        }
    }
    out
}

/// 玩家选择按钮组。每个候选玩家一颗按钮。
fn target_buttons(
    chat_id: &str,
    game_count: u32,
    actor_open_id: &str,
    action: &str,
    targets: &[(usize, &Player)],
    extra: Option<&Value>,
    style: &str,
) -> Vec<Value> {
    let v_base = json!({
        "chat_id": chat_id,
        "game": game_count,
        "actor": actor_open_id,
        "action": action,
    });
    let extra = extra.cloned().unwrap_or_else(|| json!({}));
    targets
        .iter()
        .map(|(i, p)| {
            let v = merge(
                &v_base,
                &merge(
                    &json!({ "target": p.open_id, "target_idx": i }),
                    &extra,
                ),
            );
            button(&p.name, v, style)
        })
        .collect()
}

// ============================================================================
// 开局：身份揭晓 ephemeral
// ============================================================================

pub fn build_role_reveal_card(game: &WolfGame, viewer: &Player) -> Value {
    let role = viewer.role.expect("role assigned at start");
    let mut elements: Vec<Value> = vec![
        markdown(&format!(
            "{} 你的身份是 **{}**",
            role.emoji(),
            role.label()
        )),
        markdown(role.description()),
    ];

    // 狼人 / 狼王 能看到狼队友（含其他狼王）
    if role.is_wolf() {
        let teammates: Vec<String> = game
            .players
            .iter()
            .filter(|p| {
                p.role.map(|r| r.is_wolf()).unwrap_or(false) && p.open_id != viewer.open_id
            })
            .map(|p| {
                let r_label = p
                    .role
                    .map(|r| r.label())
                    .unwrap_or("狼");
                format!("{} ({})", p.name, r_label)
            })
            .collect();
        if !teammates.is_empty() {
            elements.push(markdown(&format!(
                "🐺 你的狼队友：**{}**",
                teammates.join("、")
            )));
        } else {
            elements.push(markdown("🐺 你是独狼。"));
        }
    }

    elements.push(note(
        "仅你可见 · 看完别外泄，外泄就没有狼人杀了",
    ));

    card(
        header_with_subtitle(
            "🪪 你的身份",
            &format!("第 {} 局", game.game_count),
            "purple",
        ),
        elements,
    )
}

// ============================================================================
// 开局：公开公告
// ============================================================================

pub fn build_game_start_card(game: &WolfGame) -> Value {
    let names: Vec<String> = game
        .players
        .iter()
        .map(|p| display_name(p))
        .collect();
    // 板娘信息：人数 → 角色配比
    let n = game.players.len();
    let dist_label = match n {
        9 => "3 狼 / 预 / 女 / 猎 / 3 民（不上警）",
        10 => "2 狼 + 狼王 / 预 / 女 / 猎 / 守 / 3 民",
        11 => "2 狼 + 狼王 / 预 / 女 / 猎 / 守 / 4 民",
        12 => "3 狼 + 狼王 / 预 / 女 / 猎 / 守 / 4 民",
        _ => "未知",
    };
    card(
        header_with_subtitle(
            &format!("🌒 第 {} 局开始 · 入夜", game.game_count),
            &format!("{} 人 · {}", n, dist_label),
            "purple",
        ),
        vec![
            markdown(&format!("**入夜玩家：**\n{}", names.join(" · "))),
            note("身份已通过私密卡发给每位玩家。屠城规则：狼数 ≥ 好人数 即狼胜。"),
        ],
    )
}

// ============================================================================
// 夜晚 - 守卫
// ============================================================================

pub fn build_guard_night_card(game: &WolfGame, viewer: &Player) -> Value {
    let mut elements: Vec<Value> = vec![markdown(&format!(
        "🛡️ **第 {} 夜** · 选择今晚守护的玩家（含自己）。",
        game.day
    ))];
    if let Some(prev) = game.last_guard_target {
        elements.push(markdown(&format!(
            "昨夜你守了 **{}**（今夜不可再守此人）",
            game.players[prev].name
        )));
    }
    let targets: Vec<(usize, &Player)> = game
        .players
        .iter()
        .enumerate()
        .filter(|(i, p)| p.alive && game.last_guard_target != Some(*i))
        .collect();
    elements.push(markdown("**选择守护目标：**"));
    let buttons = target_buttons(
        &game.chat_id,
        game.game_count,
        &viewer.open_id,
        "wolf_guard_pick",
        &targets,
        None,
        "primary",
    );
    elements.extend(button_grid(buttons, 3));
    elements.push(note("仅你可见 · 同守同救会导致目标依然死亡"));
    card(
        header_with_subtitle(
            "🛡️ 守卫",
            &format!("第 {} 夜", game.day),
            "blue",
        ),
        elements,
    )
}

// ============================================================================
// 夜晚 - 狼人选择
// ============================================================================

/// 狼人行动卡。配合 update_card 使用——卡片 ID 存在 `WolfGame::wolf_night_msgs`，
/// 每次状态变化（选目标 / 发言 / 队友决定）都更新现有卡片，不重发。
///
/// 卡片包含：
/// 1. 队友进度（每只狼的目标 + 是否就绪）
/// 2. 聊天历史
/// 3. 我的当前选择高亮
/// 4. 输入框 + 发送按钮（form_submit）
/// 5. 目标选择按钮
/// 6. "我决定了 / 已就绪 (等队友)" 状态按钮
pub fn build_wolf_night_card(game: &WolfGame, viewer: &Player) -> Value {
    let viewer_idx = game
        .players
        .iter()
        .position(|p| p.open_id == viewer.open_id)
        .unwrap_or(0);

    let mut elements: Vec<Value> = vec![markdown(&format!(
        "🌒 **第 {} 夜** · 选目标 → 在卡片里讨论 → 全员都点 [我决定了] 才进下一阶段。",
        game.day
    ))];

    // 1. 全员（含自己）进度
    let alive_wolves = game.alive_wolves();
    let progress_lines: Vec<String> = alive_wolves
        .iter()
        .map(|w| {
            let p = &game.players[*w];
            let pick = game
                .wolf_kill_votes
                .iter()
                .find(|(idx, _)| idx == w)
                .map(|(_, t)| game.players[*t].name.as_str())
                .unwrap_or("未选");
            let ready = if game.is_wolf_ready(*w) { "✅ 就绪" } else { "—" };
            let me_marker = if *w == viewer_idx { " (你)" } else { "" };
            format!("• {}{} → **{}** · {}", p.name, me_marker, pick, ready)
        })
        .collect();
    elements.push(markdown(&format!(
        "**队伍进度：**\n{}",
        progress_lines.join("\n")
    )));

    // 2. 聊天历史
    if !game.wolf_chat.is_empty() {
        elements.push(hr());
        let chat_lines: Vec<String> = game
            .wolf_chat
            .iter()
            .map(|(idx, msg)| {
                let speaker = if *idx == viewer_idx {
                    format!("**你**")
                } else {
                    format!("🐺 **{}**", game.players[*idx].name)
                };
                format!("{}：{}", speaker, msg)
            })
            .collect();
        elements.push(markdown(&format!(
            "**狼人聊天：**\n{}",
            chat_lines.join("\n")
        )));
    }

    elements.push(hr());

    // 3. 输入框 + 发送（form 提交）
    let chat_form_value = json!({
        "chat_id": game.chat_id,
        "game": game.game_count,
        "actor": viewer.open_id,
        "action": "wolf_chat_send",
    });
    elements.push(form(
        &format!("wolf_chat_form_{}", game.day),
        vec![column_set(vec![
            column(
                vec![input_field(
                    "message",
                    "在这里和队友说话…",
                    "",
                    "发言",
                )],
                3,
            ),
            column(
                vec![submit_button("💬 发送", chat_form_value, "primary")],
                1,
            ),
        ])],
    ));

    // 4. 目标选择按钮
    let targets: Vec<(usize, &Player)> = game
        .players
        .iter()
        .enumerate()
        .filter(|(_, p)| p.alive && !p.role.map(|r| r.is_wolf()).unwrap_or(false))
        .collect();
    elements.push(markdown("**选择目标：**"));
    let buttons = target_buttons(
        &game.chat_id,
        game.game_count,
        &viewer.open_id,
        "wolf_kill",
        &targets,
        None,
        "danger",
    );
    elements.extend(button_grid(buttons, 3));

    // 5. 我决定了 / 已就绪 按钮
    let my_pick = game
        .wolf_kill_votes
        .iter()
        .find(|(w, _)| *w == viewer_idx)
        .map(|(_, t)| game.players[*t].name.clone());
    let am_ready = game.is_wolf_ready(viewer_idx);

    let v_ready = json!({
        "chat_id": game.chat_id,
        "game": game.game_count,
        "actor": viewer.open_id,
        "action": "wolf_ready",
    });
    let ready_btn = match (my_pick.as_deref(), am_ready) {
        (None, _) => button("🔒 先选目标", v_ready, "default"),
        (Some(_t), false) => button(
            "🔒 我决定了 (锁定后等队友)",
            v_ready,
            "primary_filled",
        ),
        (Some(_t), true) => button("✅ 已就绪 · 等队友", v_ready, "default"),
    };
    elements.push(actions(vec![ready_btn]));

    elements.push(note(
        "仅狼队伍可见 · 选了目标后再点决定 · 全员决定才结夜",
    ));

    card(
        header_with_subtitle(
            "🐺 狼人行动",
            &format!("第 {} 夜", game.day),
            "carmine",
        ),
        elements,
    )
}

// ============================================================================
// 夜晚 - 预言家
// ============================================================================

pub fn build_seer_night_card(game: &WolfGame, viewer: &Player) -> Value {
    let mut elements: Vec<Value> = vec![markdown(&format!(
        "🔮 **第 {} 夜** · 选择一位玩家查验身份。",
        game.day
    ))];

    // 历史查验
    if !game.seer_history.is_empty() {
        let lines: Vec<String> = game
            .seer_history
            .iter()
            .map(|c| {
                let nm = &game.players[c.target_idx].name;
                let kind = if c.is_wolf { "🐺 狼人" } else { "✅ 好人" };
                format!("• 第 {} 夜：{} 是 {}", c.day, nm, kind)
            })
            .collect();
        elements.push(markdown(&format!("**历史查验：**\n{}", lines.join("\n"))));
    }

    let targets: Vec<(usize, &Player)> = game
        .players
        .iter()
        .enumerate()
        .filter(|(_, p)| p.alive && p.open_id != viewer.open_id)
        .collect();
    let buttons = target_buttons(
        &game.chat_id,
        game.game_count,
        &viewer.open_id,
        "wolf_seer_check",
        &targets,
        None,
        "primary",
    );
    elements.extend(button_grid(buttons, 3));
    elements.push(note("仅你可见"));
    card(
        header_with_subtitle(
            "🔮 预言家",
            &format!("第 {} 夜", game.day),
            "indigo",
        ),
        elements,
    )
}

/// 预言家查验结果：私下回执。
pub fn build_seer_result_card(game: &WolfGame, target: &Player, is_wolf: bool) -> Value {
    let badge = if is_wolf { "🐺 **狼人**" } else { "✅ **好人**" };
    let template = if is_wolf { "carmine" } else { "turquoise" };
    card(
        header(
            &format!("🔮 查验结果 · 第 {} 夜", game.day),
            template,
        ),
        vec![
            markdown(&format!("{} 的身份是 {}", target.name, badge)),
            note("仅你可见 · 别说漏嘴"),
        ],
    )
}

// ============================================================================
// 夜晚 - 女巫
// ============================================================================

pub fn build_witch_night_card(game: &WolfGame, viewer: &Player) -> Value {
    let mut elements: Vec<Value> = vec![];

    // 揭晓今晚的猎物
    if let Some(victim_idx) = game.night_victim {
        let v = &game.players[victim_idx];
        elements.push(markdown(&format!(
            "🌑 **第 {} 夜** · 今晚狼人选择杀害 **{}**。",
            game.day, v.name
        )));
    } else {
        elements.push(markdown(&format!(
            "🌑 **第 {} 夜** · 今晚狼人空刀，没有目标。",
            game.day
        )));
    }

    elements.push(markdown(&format!(
        "**药剂状态：** 救药 {} · 毒药 {}",
        if game.witch_save_used { "✗ 已用" } else { "✓ 可用" },
        if game.witch_poison_used { "✗ 已用" } else { "✓ 可用" },
    )));

    let v_base = json!({
        "chat_id": game.chat_id,
        "game": game.game_count,
        "actor": viewer.open_id,
    });

    let mut buttons: Vec<Value> = vec![];

    // 救
    if !game.witch_save_used && game.night_victim.is_some() {
        buttons.push(button(
            "💊 救",
            merge(&v_base, &json!({ "action": "wolf_witch_save" })),
            "primary",
        ));
    }

    // 跳过
    buttons.push(button(
        "跳过",
        merge(&v_base, &json!({ "action": "wolf_witch_skip" })),
        "default",
    ));
    elements.push(actions(buttons));

    // 毒目标按钮
    if !game.witch_poison_used {
        let targets: Vec<(usize, &Player)> = game
            .players
            .iter()
            .enumerate()
            .filter(|(_, p)| p.alive && p.open_id != viewer.open_id)
            .collect();
        elements.push(markdown("**或者使用毒药：**"));
        let pbuttons = target_buttons(
            &game.chat_id,
            game.game_count,
            &viewer.open_id,
            "wolf_witch_poison",
            &targets,
            None,
            "danger",
        );
        elements.extend(button_grid(pbuttons, 3));
        elements.push(note("毒和救只能二选一 · 仅你可见"));
    } else {
        elements.push(note("毒药已用 · 仅你可见"));
    }

    card(
        header_with_subtitle(
            "🧪 女巫",
            &format!("第 {} 夜", game.day),
            "violet",
        ),
        elements,
    )
}

// ============================================================================
// 白天 - 公布死讯
// ============================================================================

pub fn build_day_reveal_card(game: &WolfGame) -> Value {
    let mut body = String::new();
    if game.last_night_deaths.is_empty() {
        body.push_str("🌅 **平安夜**，昨夜无人死亡。");
    } else {
        body.push_str("🌅 昨夜以下玩家死亡：\n");
        for idx in &game.last_night_deaths {
            let p = &game.players[*idx];
            // 死因不公开（避免泄漏角色），统一显示"出局"
            body.push_str(&format!("• {} 出局\n", display_name(p)));
        }
    }

    let alive_count = game.alive_count();
    body.push_str(&format!("\n仍在场：**{}** 人。", alive_count));

    card(
        header_with_subtitle(
            &format!("🌅 第 {} 天 · 黎明", game.day),
            "天亮了，请讨论再投票",
            "yellow",
        ),
        vec![markdown(&body)],
    )
}

// ============================================================================
// 白天 - 上警阶段
// ============================================================================

pub fn build_sheriff_nominate_card(game: &WolfGame, viewer: &Player) -> Value {
    let v_base = json!({
        "chat_id": game.chat_id,
        "game": game.game_count,
        "actor": viewer.open_id,
    });
    let buttons = vec![
        button(
            "🎖️ 上警",
            merge(&v_base, &json!({ "action": "wolf_sheriff_run" })),
            "primary",
        ),
        button(
            "不上警",
            merge(&v_base, &json!({ "action": "wolf_sheriff_skip" })),
            "default",
        ),
    ];
    card(
        header_with_subtitle(
            "🎖️ 上警阶段",
            "决定是否参选警长",
            "yellow",
        ),
        vec![
            markdown(
                "**第 1 天上警**：你可以选择参选警长。警长有 1.5 倍票权，死亡时可移交警徽。\n\
                 候选人不能投票，由其他玩家投出。",
            ),
            actions(buttons),
            note("仅你可见"),
        ],
    )
}

pub fn build_sheriff_candidates_card(game: &WolfGame) -> Value {
    let candidates = game.sheriff_candidates();
    let body = if candidates.is_empty() {
        "⚠️ 无人参选，本局无警长。".to_string()
    } else {
        let lines: Vec<String> = candidates
            .iter()
            .map(|i| format!("• {}", display_name(&game.players[*i])))
            .collect();
        format!("候选人：\n{}", lines.join("\n"))
    };
    card(
        header("🎖️ 警长候选人", "yellow"),
        vec![markdown(&body)],
    )
}

pub fn build_sheriff_vote_card(game: &WolfGame, viewer: &Player) -> Value {
    let candidates: Vec<(usize, &Player)> = game
        .sheriff_candidates()
        .into_iter()
        .map(|i| (i, &game.players[i]))
        .collect();

    let mut elements: Vec<Value> = vec![markdown(
        "🗳️ **警长投票** · 选一位候选人当警长。",
    )];
    let buttons = target_buttons(
        &game.chat_id,
        game.game_count,
        &viewer.open_id,
        "wolf_sheriff_vote",
        &candidates,
        None,
        "primary",
    );
    // 候选 ≥ 6 时按钮挤成一坨；按 3 个一行换行渲染。
    elements.extend(button_grid(buttons, 3));
    let v_abs = json!({
        "chat_id": game.chat_id,
        "game": game.game_count,
        "actor": viewer.open_id,
        "action": "wolf_sheriff_vote_abstain",
    });
    elements.push(actions(vec![button("弃权", v_abs, "default")]));
    elements.push(note("仅你可见 · 候选人本人不能投票"));
    card(
        header("🎖️ 警长投票", "yellow"),
        elements,
    )
}

// ============================================================================
// 顺序发言（上警发言 / 白天轮流发言）
// ============================================================================

/// 公开发言卡：显示发言顺序、已发言历史、当前轮到谁。update_card 原地刷新。
pub fn build_speech_public_card(
    game: &WolfGame,
    title: &str,
    template: &str,
    order: &[usize],
    speeches: &[(usize, String)],
    cur_idx: usize,
) -> Value {
    let mut elements: Vec<Value> = vec![];

    let order_line: Vec<String> = order
        .iter()
        .enumerate()
        .map(|(i, p_idx)| {
            let p = &game.players[*p_idx];
            let name = if p.is_ai {
                format!("🤖 **{}**", p.name)
            } else {
                p.name.clone()
            };
            let marker = if i < cur_idx {
                "✓"
            } else if i == cur_idx {
                "🎤"
            } else {
                "·"
            };
            format!("{} {}", marker, name)
        })
        .collect();
    elements.push(markdown(&format!("**发言顺序**\n{}", order_line.join(" → "))));

    if !speeches.is_empty() {
        elements.push(hr());
        let lines: Vec<String> = speeches
            .iter()
            .map(|(idx, text)| {
                format!("✅ **{}**：{}", game.players[*idx].name, text)
            })
            .collect();
        elements.push(markdown(&lines.join("\n\n")));
    }

    if cur_idx < order.len() {
        elements.push(hr());
        let cur = &game.players[order[cur_idx]];
        elements.push(markdown(&format!(
            "🎤 **当前发言**：{}",
            display_name(cur)
        )));
    }

    card(header(title, template), elements)
}

/// 私发输入卡：仅当前发言人收到。提供输入框 + [说完了] / [⏭ 沉默]。
pub fn build_speech_private_card(
    game: &WolfGame,
    viewer: &Player,
    title: &str,
    template: &str,
    submit_action: &str,
    skip_action: &str,
    placeholder: &str,
) -> Value {
    let v_base = json!({
        "chat_id": game.chat_id,
        "game": game.game_count,
        "actor": viewer.open_id,
    });
    let submit_v = merge(&v_base, &json!({ "action": submit_action }));
    let skip_v = merge(&v_base, &json!({ "action": skip_action }));

    let elements = vec![
        markdown("🎤 **轮到你发言**——直接说你的内容，或选择沉默。"),
        form(
            &format!("speech_form_{}_{}", game.game_count, game.day),
            vec![
                input_field("speech", placeholder, "", "发言"),
                actions(vec![
                    submit_button("✅ 说完了", submit_v, "primary_filled"),
                    button("⏭ 沉默", skip_v, "default"),
                ]),
            ],
        ),
        note("仅你可见 · 提交即结束你的回合 · 200 字内"),
    ];
    card(header(title, template), elements)
}

// ============================================================================
// 警长选方向（警上 / 警下）
// ============================================================================

pub fn build_sheriff_direction_card(game: &WolfGame, viewer: &Player) -> Value {
    let v_base = json!({
        "chat_id": game.chat_id,
        "game": game.game_count,
        "actor": viewer.open_id,
    });
    let up = merge(&v_base, &json!({ "action": "wolf_sheriff_dir_up" }));
    let down = merge(&v_base, &json!({ "action": "wolf_sheriff_dir_down" }));
    card(
        header_with_subtitle(
            "🎖️ 警长决定方向",
            "你将末位归票",
            "yellow",
        ),
        vec![
            markdown(
                "你当选了警长。请决定白天发言起手方向：\n\
                 - **警上 (顺时针)**：从你**右手第一位**起开始发言\n\
                 - **警下 (逆时针)**：从你**左手第一位**起开始发言\n\n\
                 你本人将**最后**发言（归票）。",
            ),
            actions(vec![
                button("🔁 警上", up, "primary_filled"),
                button("🔃 警下", down, "primary"),
            ]),
            note("仅你可见"),
        ],
    )
}

pub fn build_sheriff_direction_announce(_game: &WolfGame, sheriff: &Player, clockwise: bool) -> Value {
    let dir_label = if clockwise { "警上 (顺时针)" } else { "警下 (逆时针)" };
    card(
        header("🎖️ 警长指示", "yellow"),
        vec![markdown(&format!(
            "🎖️ {} 选择 **{}** 起手 · 警长本人末位归票",
            display_name(sheriff),
            dir_label,
        ))],
    )
}

// ============================================================================
// 死亡遗言
// ============================================================================

pub fn build_last_words_public_card(
    game: &WolfGame,
    queue: &[usize],
    speeches: &[(usize, String)],
    cur_idx: usize,
) -> Value {
    let mut elements: Vec<Value> = vec![];
    if queue.is_empty() {
        elements.push(markdown("本轮无遗言。"));
    } else {
        let order_line: Vec<String> = queue
            .iter()
            .enumerate()
            .map(|(i, p_idx)| {
                let p = &game.players[*p_idx];
                let marker = if i < cur_idx {
                    "✓"
                } else if i == cur_idx {
                    "🪦"
                } else {
                    "·"
                };
                format!("{} {}", marker, p.name)
            })
            .collect();
        elements.push(markdown(&format!("**遗言顺序**\n{}", order_line.join(" → "))));
        if !speeches.is_empty() {
            elements.push(hr());
            let lines: Vec<String> = speeches
                .iter()
                .map(|(idx, text)| format!("🪦 **{}**：{}", game.players[*idx].name, text))
                .collect();
            elements.push(markdown(&lines.join("\n\n")));
        }
        if cur_idx < queue.len() {
            elements.push(hr());
            let cur = &game.players[queue[cur_idx]];
            elements.push(markdown(&format!(
                "🎤 **正在发表遗言**：{}",
                display_name(cur)
            )));
        }
    }
    card(header("🪦 死亡遗言", "wathet"), elements)
}

pub fn build_last_words_private_card(game: &WolfGame, viewer: &Player) -> Value {
    let v_base = json!({
        "chat_id": game.chat_id,
        "game": game.game_count,
        "actor": viewer.open_id,
    });
    let submit_v = merge(&v_base, &json!({ "action": "wolf_last_words_submit" }));
    let skip_v = merge(&v_base, &json!({ "action": "wolf_last_words_skip" }));
    card(
        header("🎤 你的遗言", "carmine"),
        vec![
            markdown("🪦 **你已死亡，请发表你的遗言**——爆身份、报查杀、留信息都行。"),
            form(
                &format!("last_words_form_{}_{}", game.game_count, game.day),
                vec![
                    input_field("speech", "你的遗言…", "", "遗言"),
                    actions(vec![
                        submit_button("✅ 说完了", submit_v, "primary_filled"),
                        button("⏭ 沉默", skip_v, "default"),
                    ]),
                ],
            ),
            note("仅你可见 · 提交即结束遗言 · 300 字内"),
        ],
    )
}

// ============================================================================
// 白天 - 投票
// ============================================================================

pub fn build_vote_card(game: &WolfGame, viewer: &Player) -> Value {
    let targets: Vec<(usize, &Player)> = game
        .players
        .iter()
        .enumerate()
        .filter(|(_, p)| p.alive && p.open_id != viewer.open_id)
        .collect();

    let mut elements: Vec<Value> = vec![markdown(&format!(
        "🗳️ **第 {} 天** · 投出你怀疑的狼人。",
        game.day
    ))];

    let buttons = target_buttons(
        &game.chat_id,
        game.game_count,
        &viewer.open_id,
        "wolf_vote",
        &targets,
        None,
        "primary",
    );
    // 8-12 人候选挤一行很难看，按 3 个一行换行渲染。
    elements.extend(button_grid(buttons, 3));

    let v_abs = json!({
        "chat_id": game.chat_id,
        "game": game.game_count,
        "actor": viewer.open_id,
        "action": "wolf_vote_abstain",
    });
    elements.push(actions(vec![button("弃权", v_abs, "default")]));
    elements.push(note("仅你可见 · 投错也救不了"));

    card(
        header_with_subtitle(
            "🗳️ 投票",
            &format!("第 {} 天", game.day),
            "blue",
        ),
        elements,
    )
}

/// 投票完毕后公开广播：每个人投了谁。
pub fn build_vote_tally_card(game: &WolfGame) -> Value {
    let mut lines: Vec<String> = vec![];
    for (voter, target) in &game.day_votes.votes {
        let v = &game.players[*voter];
        let line = match target {
            Some(t) => format!("• {} → {}", display_name(v), display_name(&game.players[*t])),
            None => format!("• {} → 弃权", display_name(v)),
        };
        lines.push(line);
    }

    let outcome = match game.last_day_lynched {
        Some(idx) => format!(
            "🪦 **{}** 被投票放逐。",
            display_name(&game.players[idx])
        ),
        None => "🤝 平票或全员弃权，无人放逐。".into(),
    };

    card(
        header_with_subtitle(
            &format!("🗳️ 第 {} 天 · 投票结果", game.day),
            &outcome,
            "blue",
        ),
        vec![markdown(&lines.join("\n"))],
    )
}

// ============================================================================
// 猎人开枪
// ============================================================================

pub fn build_hunter_card(game: &WolfGame, viewer: &Player) -> Value {
    let targets: Vec<(usize, &Player)> = game
        .players
        .iter()
        .enumerate()
        .filter(|(_, p)| p.alive && p.open_id != viewer.open_id)
        .collect();

    let (title, prompt) = match viewer.role {
        Some(Role::WolfKing) => ("👑 狼王开枪", "👑 **狼王之死** · 选择带走一名玩家。"),
        _ => ("🏹 猎人开枪", "🏹 **猎人之死** · 你被带走了，可以拉一个人陪葬。"),
    };

    let mut elements: Vec<Value> = vec![markdown(prompt)];

    let buttons = target_buttons(
        &game.chat_id,
        game.game_count,
        &viewer.open_id,
        "wolf_hunter_shoot",
        &targets,
        None,
        "danger",
    );
    elements.extend(button_grid(buttons, 3));

    let v_skip = json!({
        "chat_id": game.chat_id,
        "game": game.game_count,
        "actor": viewer.open_id,
        "action": "wolf_hunter_skip",
    });
    elements.push(actions(vec![button("不开枪", v_skip, "default")]));
    elements.push(note("仅你可见"));

    card(
        header_with_subtitle(title, "选择带走的人", "carmine"),
        elements,
    )
}

/// 公开广播：临死前开枪打死了谁，或选择不开枪。
///
/// **不公开角色**——猎人和狼王共享开枪技能，狼王惯例伪装成猎人，所以这里
/// 用中性标题 "🏹 开枪"，不写"狼王开枪"也不直接断言"猎人开枪"。真身在结算
/// summary 卡才揭晓。
pub fn build_hunter_announce_card(
    shooter: &Player,
    target: Option<&Player>,
) -> Value {
    let body = match target {
        Some(t) => format!(
            "🏹 {} 临死前开枪，带走了 {}",
            display_name(shooter),
            display_name(t),
        ),
        None => format!("🏹 {} 选择不开枪", display_name(shooter)),
    };
    card(header("🏹 开枪", "carmine"), vec![markdown(&body)])
}

// ============================================================================
// 警徽流转
// ============================================================================

pub fn build_badge_pass_card(game: &WolfGame, viewer: &Player) -> Value {
    let targets: Vec<(usize, &Player)> = game
        .players
        .iter()
        .enumerate()
        .filter(|(_, p)| p.alive && p.open_id != viewer.open_id)
        .collect();
    let mut elements: Vec<Value> = vec![markdown(
        "🎖️ **警长之死** · 你需要决定警徽去向。",
    )];
    let buttons = target_buttons(
        &game.chat_id,
        game.game_count,
        &viewer.open_id,
        "wolf_badge_pass",
        &targets,
        None,
        "primary",
    );
    elements.extend(button_grid(buttons, 3));
    let v_destroy = json!({
        "chat_id": game.chat_id,
        "game": game.game_count,
        "actor": viewer.open_id,
        "action": "wolf_badge_destroy",
    });
    elements.push(actions(vec![button(
        "✂️ 撕毁警徽",
        v_destroy,
        "danger",
    )]));
    elements.push(note("仅你可见"));
    card(
        header("🎖️ 警徽流转", "yellow"),
        elements,
    )
}

pub fn build_badge_announce_card(
    sheriff: &Player,
    new_holder: Option<&Player>,
) -> Value {
    let body = match new_holder {
        Some(t) => format!(
            "🎖️ {} 临死前移交警徽给 {}",
            display_name(sheriff),
            display_name(t),
        ),
        None => format!("✂️ {} 撕毁了警徽，本局不再有警长", display_name(sheriff)),
    };
    card(header("🎖️ 警徽流转", "yellow"), vec![markdown(&body)])
}

// ============================================================================
// 游戏结束
// ============================================================================

pub fn build_summary_card(game: &WolfGame, winner: Winner) -> Value {
    let title = match winner {
        Winner::Good => "🎉 好人阵营胜利",
        Winner::Wolves => "🐺 狼人阵营胜利",
    };
    let template = match winner {
        Winner::Good => "turquoise",
        Winner::Wolves => "carmine",
    };

    let mut elements: Vec<Value> = vec![];

    // 1. 真身揭晓
    let role_lines: Vec<String> = game
        .players
        .iter()
        .map(|p| {
            let r = p
                .role
                .map(|r| format!("{} {}", r.emoji(), r.label()))
                .unwrap_or_else(|| "无角色".into());
            let alive = if p.alive { "✓ 存活" } else { "☠ 出局" };
            format!("• {} — **{}** · {}", display_name(p), r, alive)
        })
        .collect();
    elements.push(markdown(&format!("**真身揭晓：**\n{}", role_lines.join("\n"))));

    // 2. 全局复盘：按天 / 阶段渲染
    let recap_md = render_recap(game);
    if !recap_md.is_empty() {
        elements.push(hr());
        elements.push(markdown("**🎬 全局复盘**"));
        for chunk in recap_md {
            elements.push(markdown(&chunk));
        }
    }

    elements.push(note("点大厅卡的 [开局] 开下一局"));

    card(
        header_with_subtitle(
            title,
            &format!("第 {} 局 · 共 {} 天", game.game_count, game.day),
            template,
        ),
        elements,
    )
}

/// 把 `recap_log` 渲染成多个 markdown 段落（按天/阶段切块），交给 caller 拼到卡上。
/// 切多块是为了避免单条 markdown 超长——飞书卡片对单 element 长度有限制。
fn render_recap(game: &WolfGame) -> Vec<String> {
    use crate::werewolf::game::RecapEvent as E;

    let name = |idx: usize| -> &str {
        game.players.get(idx).map(|p| p.name.as_str()).unwrap_or("?")
    };

    // 把 recap_log 按"段"分组：每段对应一个标题块（例如 第 N 夜 / 第 N 天 黎明 /
    // 上警 / 第 N 天 发言 / 第 N 天 投票）。
    enum Section {
        Night(u32),
        Dawn(u32),
        Sheriff,
        DaySpeech(u32),
        DayVote(u32),
        Skill,
    }
    impl Section {
        fn title(&self) -> String {
            match self {
                Section::Night(d) => format!("🌒 **第 {d} 夜**"),
                Section::Dawn(d) => format!("🌅 **第 {d} 天 黎明**"),
                Section::Sheriff => "🎖️ **上警阶段**".into(),
                Section::DaySpeech(d) => format!("🎙️ **第 {d} 天 轮流发言**"),
                Section::DayVote(d) => format!("🗳️ **第 {d} 天 投票**"),
                Section::Skill => "🏹 **技能 · 警徽流转**".into(),
            }
        }
    }

    fn classify(event: &crate::werewolf::game::RecapEvent) -> Section {
        match event {
            E::GuardProtect { day, .. }
            | E::WolfFinalTarget { day, .. }
            | E::SeerCheck { day, .. }
            | E::Witch { day, .. } => Section::Night(*day),
            E::Death { day, night: true, .. } | E::LastWords { day, night: true, .. } => {
                Section::Dawn(*day)
            }
            E::SheriffCandidates { .. }
            | E::SheriffSpeech { .. }
            | E::SheriffElected { .. }
            | E::SheriffDirection { .. } => Section::Sheriff,
            E::DaySpeech { day, .. } => Section::DaySpeech(*day),
            E::DayVoteCast { day, .. }
            | E::DayLynch { day, .. }
            | E::Death { day, night: false, .. }
            | E::LastWords { day, night: false, .. } => Section::DayVote(*day),
            E::HunterShot { .. } | E::BadgePass { .. } => Section::Skill,
        }
    }

    fn section_key(s: &Section) -> (u8, u32) {
        match s {
            Section::Night(d) => (0, *d),
            Section::Dawn(d) => (1, *d),
            Section::Sheriff => (2, 0),
            Section::DaySpeech(d) => (3, *d),
            Section::DayVote(d) => (4, *d),
            Section::Skill => (5, 0),
        }
    }

    // 按段内顺序保留事件
    let mut sections: Vec<(Section, Vec<&E>)> = vec![];
    for ev in &game.recap_log {
        let s = classify(ev);
        let key = section_key(&s);
        if let Some(last) = sections.last_mut() {
            if section_key(&last.0) == key {
                last.1.push(ev);
                continue;
            }
        }
        sections.push((s, vec![ev]));
    }

    // 渲染每段为一个 markdown 字符串
    let mut out: Vec<String> = vec![];
    for (section, events) in sections {
        let mut buf = section.title();
        buf.push('\n');
        for ev in events {
            let line = match ev {
                E::GuardProtect { target, .. } => {
                    format!("  🛡️ 守卫守了 **{}**", name(*target))
                }
                E::WolfFinalTarget { target, .. } => match target {
                    Some(t) => format!("  🐺 狼人合议 → **{}**", name(*t)),
                    None => "  🐺 狼人空刀".to_string(),
                },
                E::SeerCheck { target, is_wolf, .. } => {
                    let badge = if *is_wolf { "🐺 狼人" } else { "✅ 好人" };
                    format!("  🔮 预言家查验 **{}** → {}", name(*target), badge)
                }
                E::Witch { save, poison, .. } => {
                    let mut parts: Vec<String> = vec![];
                    if *save {
                        parts.push("救人".into());
                    }
                    if let Some(t) = poison {
                        parts.push(format!("毒杀 **{}**", name(*t)));
                    }
                    if parts.is_empty() {
                        parts.push("跳过".into());
                    }
                    format!("  🧪 女巫 {}", parts.join(" + "))
                }
                E::Death { player, cause, .. } => {
                    format!("  ☠️ **{}** {}", name(*player), cause.label())
                }
                E::LastWords { player, text, .. } => {
                    format!("  💬 **{}** 遗言：{}", name(*player), text)
                }
                E::SheriffCandidates { candidates } => {
                    let names: Vec<&str> = candidates.iter().map(|i| name(*i)).collect();
                    if names.is_empty() {
                        "  无人上警".into()
                    } else {
                        format!("  候选人：{}", names.join(" / "))
                    }
                }
                E::SheriffSpeech { player, text } => {
                    format!("  🎤 **{}** (上警发言)：{}", name(*player), text)
                }
                E::SheriffElected { player } => match player {
                    Some(p) => format!("  🎖️ **{}** 当选警长", name(*p)),
                    None => "  ⚠️ 流局，本局无警长".into(),
                },
                E::SheriffDirection { clockwise } => {
                    format!(
                        "  ➡️ 警长选择 **{}** 起手，警长本人末位归票",
                        if *clockwise { "警上 (顺时针)" } else { "警下 (逆时针)" }
                    )
                }
                E::DaySpeech { player, text, .. } => {
                    format!("  🎤 **{}**：{}", name(*player), text)
                }
                E::DayVoteCast { voter, target, weight, .. } => {
                    let arrow = match target {
                        Some(t) => format!("→ **{}**", name(*t)),
                        None => "→ 弃权".into(),
                    };
                    let w_marker = if *weight == 3 { " (警徽 ×1.5)" } else { "" };
                    format!("  {} {}{}", name(*voter), arrow, w_marker)
                }
                E::DayLynch { target, .. } => match target {
                    Some(t) => format!("  🪦 **{}** 被放逐", name(*t)),
                    None => "  🤝 流局，无人放逐".into(),
                },
                E::HunterShot { shooter, target, .. } => match target {
                    Some(t) => format!("  🏹 **{}** 开枪带走 **{}**", name(*shooter), name(*t)),
                    None => format!("  🏹 **{}** 选择不开枪", name(*shooter)),
                },
                E::BadgePass { from, to, .. } => match to {
                    Some(t) => format!(
                        "  🎖️ 警徽从 **{}** 移交给 **{}**",
                        name(*from),
                        name(*t)
                    ),
                    None => format!("  ✂️ **{}** 撕毁警徽", name(*from)),
                },
            };
            buf.push_str(&line);
            buf.push('\n');
        }
        out.push(buf);
    }
    out
}

