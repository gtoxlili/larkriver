//! 狼人杀模式（Werewolf）—— 与德州扑克并行存在的第二种玩法。
//!
//! 模块划分：
//! - `game`：状态机、角色、阶段、胜负判定（纯逻辑，无 IO）
//! - `cards`：飞书卡片渲染
//! - `llm`：每个角色的 AI 决策 prompt
//! - `handlers`：bot.rs 的 Werewolf 扩展（impl Bot）
//!
//! 每个 chat 同时最多一桌狼人杀 + 一桌德州，两者完全独立。

pub mod cards;
pub mod game;
pub mod handlers;
pub mod llm;

pub use game::WolfGame;
