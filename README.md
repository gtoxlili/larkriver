# lark-poker

Texas Hold'em poker bot for a Feishu (Lark) group chat, written in Rust.

Players type `@bot join` to enter the next hand and `@bot start` to deal. All in-game
actions (Fold/Check/Call/Raise/All-in) are buttons on interactive cards. Hole cards
are sent as direct messages (DMs) so other players cannot see them.

## Quick start

```bash
cp .env.example .env
# Edit .env with your app credentials and chat id
cargo run --release
```

### Run with Docker

```bash
docker build -t lark-poker .
docker run --rm -p 8080:8080 --env-file .env lark-poker
```

Then expose the HTTP port (default `:8080`) to the public internet (e.g. `ngrok http 8080`)
and set the resulting `https://.../webhook/event` and `https://.../webhook/card` URLs in
your Feishu app's **Event Subscriptions** and **Card Request URL** pages.

## Required Feishu app permissions

In the Feishu Developer Console under **Permissions & Scopes**, enable:

| Scope | Purpose |
| ----- | ------- |
| `im:message` | Send messages |
| `im:message:send_as_bot` | Send as the bot |
| `im:message.group_at_msg` (or `:readonly`) | Receive @-mentions in group |
| `im:message.p2p_msg` (or `:readonly`) | Receive direct messages |
| `im:chat:readonly` | Read group info / member list |

Subscribe to these **events** under **Event Subscriptions**:

- `im.message.receive_v1` — receive group/DM messages

Add the bot to the target group, then in **Settings of the bot** enable
"Card Action Configuration" and set the card request URL.

## How to play

1. In the group, send `@bot join` — repeat for every player.
2. Once at least 2 have joined, anyone sends `@bot start`.
3. Each player receives their hole cards as a DM from the bot.
4. The group sees community cards, the pot, and a card with action buttons for
   the player to act. Click a button to take action.
5. After the river or once everyone but one folds, the bot announces the winner
   and adjusts chip stacks. Run `@bot start` again to play another hand with the
   same players.

## Commands (group chat, with @-mention)

| Command | Effect |
| ------- | ------ |
| `join`  | Enter the lobby for the next hand |
| `leave` | Leave the lobby |
| `start` | Deal a new hand (≥ 2 players required) |
| `state` | Re-post the current game state |
| `chips` | List every player's chip stack |
| `reset` | Reset the table (clears chips and players) |

## Project layout

```
src/
  main.rs        # entry point
  config.rs      # env config
  feishu/        # Feishu API client + card builders + event types
  poker/         # Cards, deck, 7-card hand evaluator
  game.rs        # Game state machine (betting, side pots, showdown)
  bot.rs         # Maps Feishu events → game actions and back
  server.rs      # axum HTTP server (webhook endpoints)
```
