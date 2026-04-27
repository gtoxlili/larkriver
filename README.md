# larkriver

飞书群聊游戏机器人——支持德州扑克 + 狼人杀两种模式。

- **德州扑克**：多玩家、边池、全押、6+ 短牌
- **狼人杀**：9-12 人、狼/狼王/预/女/猎/守/民、屠城规则、上警警徽流转
- 卡片 JSON 2.0：手牌方块、加注 form 输入、按钮分级
- 手牌 / 身份 / 夜间技能 / 投票 通过 ephemeral 群消息发送（仅本人可见，无需私聊机器人）
- 行动卡 ephemeral 给当前 actor，其他人只看公开公告

## 部署

### 1. 拉镜像并运行

```bash
docker pull ghcr.io/gtoxlili/larkriver:latest

docker run -d \
  --name larkriver \
  --restart unless-stopped \
  -e FEISHU_APP_ID=<your_app_id> \
  -e FEISHU_APP_SECRET=<your_app_secret> \
  -e ALLOWED_CHAT_ID=<oc_xxx> \
  -e BIND_ADDR=0.0.0.0:8080 \
  -e RUST_LOG=larkriver=info,tower_http=info \
  -p 8080:8080 \
  ghcr.io/gtoxlili/larkriver:latest
```

### 2. 飞书后台配置

在 [飞书开放平台](https://open.feishu.cn/app) 新建企业自建应用，添加机器人能力。

**权限管理**

| Scope | 用途 |
| --- | --- |
| `im:message:send_as_bot` | 发消息 |
| `im:message.group_at_msg:readonly` | 接收群里 @ 机器人 |
| `im:chat:readonly` | 读群成员、用户进群事件 |

**事件配置**（事件与回调 → 事件配置）

- 订阅方式：将事件发送至开发者服务器
- 请求地址：`http://<your-server>:8080/webhook/event`
- 订阅事件：
  - `im.message.receive_v1`
  - `im.chat.member.user.added_v1`

不要在事件配置页订阅 `card.action.trigger`。

**回调配置**（事件与回调 → 回调配置）

- 请求地址：`http://<your-server>:8080/webhook/card`
- 订阅回调：仅 `card.action.trigger`
- 删除：`card.action.trigger_v1`、`url.preview.get`、`profile.view.get`

`card.action.trigger_v1` 和 `card.action.trigger` 同时订阅会造成同一次按钮点击发两份回调。

**版本管理与发布**

每次改完订阅或权限都要创建版本并发布。

### 3. 把机器人拉进群

群设置 → 机器人 → 添加机器人。

## 玩法

### 统一大厅

每个群只有一张大厅卡，玩家先点 **[加入]** 入座，再选模式：

- **[🎰 开始德州]** / **[🎰 短牌德州]** — 走德州流程
- **[🐺 开始狼人杀]** — 走狼人杀流程

人数门槛：
- 德州：≥ 2 名持筹码玩家
- 狼人杀：9-12 人（标准板娘）

按钮共用：`[加入]` `[加入 AI]` `[移除 AI]` `[离开]` `[重置]`。任一模式进行中时所有按钮收起，只显示当前游戏状态。

### 文字命令

#### 德州

`/poker <关键词>` 或 `@机器人 <关键词>`：

| 命令 | 说明 |
| --- | --- |
| `join` / `加入` | 入桌 |
| `leave` / `离开` | 离桌（牌局未开始时） |
| `start` / `开始` | 开局，需 ≥ 2 名持筹码玩家。`start short` 走 6+ 短牌 |
| `state` / `状态` | 查看当前状态（私下回复） |
| `chips` / `筹码` | 查看各家筹码（私下回复） |
| `reset` / `重置` | 重置房间（清空德州+狼人杀） |
| `help` / `帮助` | 帮助卡 |

短牌：36 张牌（去掉 2-5），同花 > 葫芦、三条 > 顺子，A 低顺是 A-6-7-8-9。

行动按钮只发给当前 actor，仅他可见，包括 `[弃牌] [跟注 X] [全押]` 一行 + 加注 form 输入框 + 三个加注预设。

#### 狼人杀

`/wolf <关键词>` 或 `@机器人 wolf <关键词>` 或 `@机器人 狼 <关键词>`：

| 命令 | 说明 |
| --- | --- |
| `join` / `加入` | 入房（同 `/poker join`） |
| `leave` / `离开` | 离房（同 `/poker leave`） |
| `start` / `开始` | 开狼人杀，需 9-12 名玩家 |
| `reset` / `重置` | 重置房间（同 `/poker reset`） |
| `help` / `帮助` | 狼人杀帮助卡 |

`join` / `leave` / `reset` 在两个命名空间下完全等价——大厅名册是统一的。

**角色配比**：

| 人数 | 角色 |
| --- | --- |
| 9 | 3 狼 + 预言家 + 女巫 + 猎人 + 3 村民（不上警） |
| 10 | 2 狼 + **狼王** + 预言家 + 女巫 + 猎人 + 守卫 + 3 村民 |
| 11 | 2 狼 + **狼王** + 预言家 + 女巫 + 猎人 + 守卫 + 4 村民 |
| 12 | 3 狼 + **狼王** + 预言家 + 女巫 + 猎人 + 守卫 + 4 村民 |

**流程**：开局 → 身份 ephemeral → **第 1 夜**（守卫 → 狼刀 → 预言家 → 女巫）→ 黎明公布死讯 → **死亡遗言**（被狼刀死者轮流发言，被毒者无遗言）→ **上警阶段**（10+ 人板，仅第 1 天）→ **上警发言**（候选 ≥ 2 时按提名顺序轮流）→ **警下发言**（非候选人轮流表态）→ 警长投票 → **警长选警上 / 警下** → **白天轮流发言**（警长末位归票）→ 全员投票放逐 → **放逐者遗言** → 猎人 / 狼王开枪 / 警徽流转 → 检查胜负 → 下一夜 / 结束。

**胜负**（屠城）：好人胜利需击杀全部狼人（含狼王）；存活狼人 ≥ 存活好人 即狼胜。

**关键规则**：
- **狼王 (10+ 人板)**：狼阵营。被投票放逐时可开枪带走一人；**被毒不能开枪**。预言家查验显示为狼。
- **守卫 (10+ 人板)**：每晚守一名玩家（含自己），不可连续两晚守同一人。**同守同救**：被守 + 被救 → 双重保护抵消，依然死亡。
- **女巫**：救药 / 毒药各 1 瓶，同晚不可救+毒。
- **猎人**：被狼刀或被放逐时可开枪带走一人。**被毒**不能开枪。
- **警长 (10+ 人板)**：1.5 倍票权（整数 3 vs 普通 2）；死亡时可移交警徽给存活玩家或撕毁。**警长决定白天发言警上 / 警下方向，本人末位归票**。
- **死亡遗言**：被狼刀 / 被放逐者有遗言（轮流发表）；**被毒杀者无遗言**（悄悄死）；被开枪带走者也无遗言。

**讨论 / 发言机制**：
- **狼人夜间** ：
  - 全 AI 局：跳过讨论，AI 顺序决策后直接结算
  - 混合 / 全人类局：每只狼收到带聊天的私密行动卡（update_card 原地刷新），AI 也会发表一句协调队友的话；人类可在卡片输入框里发言、改目标、点 [我决定了] 锁定。**全员就绪才结夜**
- **上警发言（10+ 板，候选 ≥ 2）**：候选人按提名顺序轮流，公开"上警发言卡"显示进度+历史，当前候选人收到私密输入卡（[✅ 说完了] / [⏭ 沉默]）
- **白天发言**：警长起手顺时针轮流（无警长则随机起点），公开发言卡 update_card 同步进度，当前发言人私密收到输入卡。**全员说完才能投票**，避免抢话

**AI 玩家**：配 `OPENAI_API_KEY` 后可加入 LLM 驱动的 AI。每个角色都有专属 prompt：狼/狼王夜杀 + 队伍频道发言 / 预查验 / 女巫救毒 / 守卫守护 / 猎人 / 狼王开枪 / 上警决策 / 警长投票 / 警徽流转 / 白天发言。AI 失败/超时自动 fallback 到安全选择。

## AI 对手

设置了 `OPENAI_API_KEY` 后，大厅多一个 `[加入 AI]` 按钮，点一次加一个 LLM 驱动的 AI 玩家（可加多个）。AI 轮到时机器人会带着公共牌、手牌、底池、equity、历史动作请 LLM 给出 fold/check/call/raise/allin 决策，模型用 `response_format=json_object` 返回结构化 JSON。LLM 失败/超时自动 fallback 到 check 或 fold。

兼容任何 OpenAI 风格端点（DeepSeek / 豆包 / OpenAI / OpenRouter / vLLM 等）。`.env.example` 里有几个端点的配置示例。

## 注意事项

- 卡片 JSON 2.0 要求 Lark 客户端 v7.20+
- 游戏状态持久化到 redb（`LARKRIVER_DB_PATH`，默认 `larkriver.redb`），筹码 / 玩家 / AI 座位重启不丢
- 进行中的一手牌如果在容器重启之间被打断，下次 `[开局]` 会按"卡住"处理：退还场上筹码 + 重新发牌
- 牌局卡住用 `/poker reset` 手动重置
- 双层去重抗飞书重投：`event_id`（120s 窗口） + `(open_id, value)` 指纹（3s 窗口）

## 项目结构

```
src/
├── main.rs              入口；--mock <open_id> 把所有卡片样式发到群里看
├── config.rs            env 加载
├── feishu/
│   ├── client.rs        token 缓存 + 发消息 / 临时消息 / 更新卡片
│   ├── cards.rs         JSON 2.0 helpers
│   └── events.rs        webhook payload 解析
├── poker/               德州扑克
│   ├── card.rs          Card / Suit / Rank / Deck
│   ├── hand.rs          7 张牌评估器
│   └── equity.rs        Monte Carlo 胜率
├── werewolf/            狼人杀
│   ├── game.rs          角色 / 阶段 / 状态机
│   ├── cards.rs         身份卡 / 夜间卡 / 投票卡 / 结算卡
│   ├── llm.rs           各角色的 AI 决策 prompt
│   └── handlers.rs      bot.rs 的 Bot 扩展（命令/回调/AI 推进）
├── game.rs              德州状态机
├── llm.rs               OpenAI 客户端封装（共享）
├── bot.rs               事件分发 + 卡片渲染 + dedup
├── storage.rs           redb 持久化（games + wolf_games 两张表）
└── server.rs            axum HTTP server
```

## 开发

```bash
git clone https://github.com/gtoxlili/larkriver
cd larkriver
cp .env.example .env
cargo run --release
cargo test
```

把所有卡片样式发到 `ALLOWED_CHAT_ID` 看效果：

```bash
./target/release/larkriver --mock <你的 open_id>
```

## CI 和镜像

每次推 `main` 或打 `v*` tag，[.github/workflows/ci.yml](.github/workflows/ci.yml) 会跑 `docker buildx build`。Dockerfile 里有 `test` 阶段，cargo test 通过后才 build runtime 阶段，最后推到 `ghcr.io/gtoxlili/larkriver:{latest, main, sha-<short>, v<semver>}`。

Dockerfile 用 cargo-chef 缓存依赖，distroless/cc-debian12:nonroot 作运行时，最终镜像约 30 MB。

## License

MIT
