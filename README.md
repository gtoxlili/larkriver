# larkriver

飞书群聊德州扑克机器人。

- 多玩家德州扑克，支持边池和全押
- 卡片 JSON 2.0：手牌方块、加注 form 输入、按钮分级
- 手牌通过 ephemeral 群消息发送（仅本人可见，无需私聊机器人）
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

群里 @ 机器人加关键词，或 `/poker <关键词>`：

| 命令 | 说明 |
| --- | --- |
| `join` / `加入` | 入桌 |
| `leave` / `离开` | 离桌（牌局未开始时） |
| `start` / `开始` | 开局，需 ≥ 2 名持筹码玩家 |
| `state` / `状态` | 查看当前状态（私下回复） |
| `chips` / `筹码` | 查看各家筹码（私下回复） |
| `reset` / `重置` | 重置牌桌 |
| `help` / `帮助` | 帮助卡 |

也可以点持久化大厅卡上的 `[加入]` `[加入 AI]` `[离开]` `[开局]` `[短牌]` 按钮。`[短牌]` 走 6+ Hold'em：36 张牌（去掉 2-5），同花 > 葫芦、三条 > 顺子，A 低顺是 A-6-7-8-9。`/poker start short` 是文字版等价命令。

行动按钮只发给当前 actor，仅他可见，包括 `[弃牌] [跟注 X] [全押]` 一行 + 加注 form 输入框 + 三个加注预设。

## AI 对手

设置了 `OPENAI_API_KEY` 后，大厅多一个 `[加入 AI]` 按钮，点一次加一个 LLM 驱动的 AI 玩家（可加多个）。AI 轮到时机器人会带着公共牌、手牌、底池、equity、历史动作请 LLM 给出 fold/check/call/raise/allin 决策，模型用 `response_format=json_object` 返回结构化 JSON。LLM 失败/超时自动 fallback 到 check 或 fold。

兼容任何 OpenAI 风格端点（DeepSeek / 豆包 / OpenAI / OpenRouter / vLLM 等）。`.env.example` 里有几个端点的配置示例。

## 注意事项

- 卡片 JSON 2.0 要求 Lark 客户端 v7.20+
- 游戏状态在内存里，重启会丢失当前牌局，玩家筹码回到初始 1000
- 牌局卡住用 `/poker reset` 手动重置
- 双层去重抗飞书重投：`event_id`（120s 窗口） + `(open_id, value)` 指纹（3s 窗口）

## 项目结构

```
src/
├── main.rs          入口；--mock <open_id> 把所有卡片样式发到群里看
├── config.rs        env 加载
├── feishu/
│   ├── client.rs    token 缓存 + 发消息 / 临时消息 / 更新卡片
│   ├── cards.rs     JSON 2.0 helpers
│   └── events.rs    webhook payload 解析
├── poker/
│   ├── card.rs      Card / Suit / Rank / Deck
│   └── hand.rs      7 张牌评估器
├── game.rs          状态机
├── bot.rs           事件分发 + 卡片渲染 + dedup
└── server.rs        axum HTTP server
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
