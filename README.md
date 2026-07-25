# Polymarket YES/NO arbitrage alerter

Alerts-only tooling for the Polymarket YES/NO set arbitrage, in both directions — buy a
set below $1 and merge it, or split $1 into a set and sell it above $1. It **detects**
opportunities and **notifies** you; it never signs, funds, or places an order. Two
independent binaries:

- **`detector`** — a backend that **streams order books over the Polymarket websocket**
  for a user-curated watchlist, runs the arbitrage math on every book update, and exposes
  opportunities plus watchlist management over a small REST API.
- **`telegram-bot`** — a standalone app that pushes alerts to a Telegram chat and lets you
  manage the watchlist from chat (paste a link, search, add/remove).

```
Polymarket CLOB websocket (order books)      Gamma REST (resolve link / search, cold path)
        │  stream                                     │
        ▼                                             ▼
   ┌────────────────────────────────────────────────────┐
   │ detector  — live books → arb math → opportunities   │
   └───────────────┬──────────────────────┬─────────────┘
       REST /watch,/search (manage)   REST /opportunities (poll)
                   │                      │
                   ▼                      ▼
              ┌──────────────┐   Bot API   ┌──────────┐
              │ telegram-bot │ ───────────▶│ Telegram │
              └──────────────┘             └──────────┘
```

### Why websockets

Order books stream over `wss://ws-subscriptions-clob.polymarket.com/ws/market`: an initial
`book` snapshot per token, then `price_change` deltas. The detector keeps each book in
memory and re-evaluates the arbitrage the instant a level changes — no polling, no
snapshot lag. The only REST left is **Gamma**, used solely on the cold path to turn a
pasted link / slug / condition id or a search query into token ids; it never touches the
hot detection loop.

## The arbitrage

Polymarket is an exchange, not a bookmaker: the YES and NO outcome tokens trade on two
separate order books, so their prices are set by supply and demand, not by the identity
`YES + NO = 1`. A **full set** of 1 YES + 1 NO always settles to exactly 1 USDC — via
`redeem` after resolution, or immediately via the Conditional Tokens Framework
`mergePositions`, which burns a matched set back into 1 USDC without waiting.

A set is also **mintable** on demand: `splitPosition` turns 1 USDC into 1 YES + 1 NO. So
par can be arbitraged from either side, and the detector alerts on both directions:

```
mint   ask(YES) + ask(NO) < 1  →  buy both legs      →  merge/redeem for 1  →  edge = 1 − sum
split  bid(YES) + bid(NO) > 1  →  split 1 into a set →  sell both legs      →  edge = sum − 1
```

Worked examples: YES ask `0.24`, NO ask `0.70` — a set costs `0.94` and settles to `1.00`
regardless of outcome, `0.06` per set (**mint**). Or YES bid `0.29`, NO bid `0.72` — mint a
set for `1.00` and dump both legs for `1.01`, `0.01` per set (**split**).

> **Both directions are taker-on-both-legs**, so both pay two taker fees and one CTF gas
> leg. That matters most for **split**: its gross edge is typically 1–2¢ while two taker
> fees on a mid-priced market run well past that, so the split feed is almost entirely
> gated by fees. Free categories (geopolitics/world, `0` bps) are where thin split edges
> actually survive. Alerts for either direction can be switched off from chat
> (`/set mint off`, `/set split off`).

### Does it clear costs?

Each opportunity is reported with the full economics so you can judge it before acting:

- **Order-book depth** — the engine sweeps both ladders together (asks for **mint**, bids
  for **split**) and stacks sets only while the *marginal* set still clears par after its
  own fee, so `size`/`cost`/`net_profit` reflect real executable depth, not just
  top-of-book. This is where a headline 1¢ split edge usually collapses: the best bid is
  good for a handful of shares and the next level is already through par.
- **Fees** — Polymarket's Fee Structure V2 taker fee, applied per side as
  `fee_bps/10000 · min(price, 1−price) · shares`. Both legs take liquidity in either
  direction, so both pay. Per-category rates (`category_fee_bps`, e.g. crypto `350` =
  $1.75/100 shares at price 0.50, sports `250`, geopolitics `0`) are assigned to each
  market at discovery; manual/uncategorised markets fall back to the global `fee_bps`.
  Because the fee peaks at 0.50 and fades toward the tails, an off-center market pays much
  less on its cheap leg than on its dear one.
- **Slippage** — `slippage_bps` pads each buy price and haircuts each sell price.
- **Gas** — `gas_cost`, the fixed USDC cost of the CTF leg (`merge`/`redeem` for **mint**,
  `splitPosition` for **split**), subtracted from net profit; it is what kills thin edges
  on small size. Note that ordinary CLOB trading is relayed and effectively gasless, while
  a direct `splitPosition` call is not necessarily subsidised — set this to a real number
  before trusting a small-size split alert.
- **Capital** — `max_capital` caps the assumed set size. For **split** it caps sets
  directly, since minting a set always costs exactly $1.

An opportunity is flagged `profitable` only when `net_profit ≥ min_profit` after all of
the above. The reported **`avg_edge`** is the volume-weighted edge that actually clears at
`size` (`(payout − cost − fees) / size`); it is the honest headline, while `gross_edge`
(top of book only) is context.

### Signal quality — why the feed is mostly empty

Polymarket's YES and NO order books are **mirror images**: a buy order on YES at price `p`
is the same object as a sell order on NO at `1 − p`, so at any coherent instant
`ask_NO = 1 − bid_YES`, and therefore `ask(YES) + ask(NO) = 1 + spread ≥ 1` while
`bid(YES) + bid(NO) = 1 − spread ≤ 1`. Neither a persistent buy-set arb nor a persistent
sell-set arb *can exist*; either only appears through genuine native crossing (gone in
milliseconds) or, far more often, **stale/desynced snapshots** of the two legs. The
detector therefore drops any snapshot that is internally impossible — a leg whose own book
is crossed, or a snapshot showing a buy-set arb and a sell-set arb at the same time — and
only fetches markets that are `active`, not `closed`, and `acceptingOrders`. After that
filtering the feed is usually near-empty, which is correct: real set-arb on Polymarket is
rare and fleeting.

### Practical analysis

- **Competition.** This is a crowded trade; edges are small and vanish in well under a
  second. This tool is a *detector/alerter* — treat alerts as a live feed of where and how
  often mispricings appear, and as the signal layer you would build an (out-of-scope)
  execution layer on top of.
- **Latency & colocation.** Polymarket sits behind Cloudflare and its matching engine has
  historically been UK-hosted (where Polymarket is geo-blocked), complicating naive
  colocation. A Dublin VPS is a common compromise — but **measure the real path first**
  (`mtr`/`traceroute` to `clob.polymarket.com`) to confirm traffic doesn't egress via the
  US, and check the legal posture for your jurisdiction.
- **Merge vs. wait.** You need not wait for resolution: buy the set, then `mergePositions`
  on Polygon to realize 1 USDC immediately — removing oracle/resolution risk but adding a
  gas leg (`gas_cost`).
- **Taker vs. maker on the split.** The split alert assumes you *hit* the resting bids, so
  it charges two taker fees. Posting limit sells at those prices instead makes you the
  maker — no taker fee, plus a rebate share — but drops the execution guarantee: the order
  may simply not fill while `bid+bid > 1` holds, and you are left holding a set. The
  numbers reported here are the conservative taker case.
- **Residual risks even in the "risk-free" trade.** Partial fills (one leg only), price
  moving between fills (the split direction runs mint → sell → sell non-atomically),
  approvals/USDC bridging friction, market invalidation. None are handled here because the
  project intentionally stops at detection.

## Build

```bash
cargo build --release
cargo test -p arb-core   # arbitrage math unit tests
```

Binaries: `target/release/{detector,telegram-bot}`.

## Configuration

Every parameter is layered. **Precedence (later wins): defaults < YAML < environment <
CLI flags.** YAML holds the baseline, env holds secrets/deploy overrides, CLI flags are
one-offs.

- YAML: `config/detector.yaml`, `config/telegram-bot.yaml` (path via `--config`).
- Env: `DETECTOR_*` / `TELEGRAM_*` (a `.env` file is auto-loaded; see `.env.example`).
- CLI: `--<field>` per field; `--help` lists them.

### detector

| Field | YAML / CLI | Env | Default |
|---|---|---|---|
| Bind address | `bind_addr` | `DETECTOR_BIND_ADDR` | `0.0.0.0:8080` |
| Market websocket URL | `ws_url` | `DETECTOR_WS_URL` | CLOB market ws |
| Gamma API URL | `gamma_url` | `DETECTOR_GAMMA_URL` | gamma-api.polymarket.com |
| Watchlist file | `watch_file` | `DETECTOR_WATCH_FILE` | `watchlist.json` |
| Search result limit | `search_limit` | `DETECTOR_SEARCH_LIMIT` | `8` |
| Category auto-discovery | `auto_discover` | `DETECTOR_AUTO_DISCOVER` | `true` |
| Discovery interval (s) | `discover_interval_secs` | `DETECTOR_DISCOVER_INTERVAL_SECS` | `300` |
| Discovery min volume | `discover_min_volume` | `DETECTOR_DISCOVER_MIN_VOLUME` | `0` |
| Discovery max markets | `discover_max_markets` | `DETECTOR_DISCOVER_MAX_MARKETS` | `100000` |
| HTTP timeout (s) | `http_timeout_secs` | `DETECTOR_HTTP_TIMEOUT_SECS` | `15` |
| Log level | `log_level` | `DETECTOR_LOG_LEVEL` | `info` |
| Fee (bps/side) | `fee_bps` | `DETECTOR_FEE_BPS` | `0` |
| Slippage (bps) | `slippage_bps` | `DETECTOR_SLIPPAGE_BPS` | `0` |
| Gas cost (USDC) | `gas_cost` | `DETECTOR_GAS_COST` | `0` |
| Max capital (USDC) | `max_capital` | `DETECTOR_MAX_CAPITAL` | `1000` |
| Min net profit | `min_profit` | `DETECTOR_MIN_PROFIT` | `0` |
| Min gross edge | `min_edge` | `DETECTOR_MIN_EDGE` | `0` |

### telegram-bot

| Field | YAML / CLI | Env | Default |
|---|---|---|---|
| Bot token *(required)* | `bot_token` | `TELEGRAM_BOT_TOKEN` | — |
| Alert chat id | `chat_id` | `TELEGRAM_CHAT_ID` | — |
| Backend URL | `backend_url` | `TELEGRAM_BACKEND_URL` | `http://localhost:8080` |
| Poll interval (s) | `poll_interval_secs` | `TELEGRAM_POLL_INTERVAL_SECS` | `10` |
| HTTP timeout (s) | `http_timeout_secs` | `TELEGRAM_HTTP_TIMEOUT_SECS` | `15` |
| Alert min edge | `alert_min_edge` | `TELEGRAM_ALERT_MIN_EDGE` | `0.01` |
| Alert min profit | `alert_min_profit` | `TELEGRAM_ALERT_MIN_PROFIT` | `1.0` |
| Only profitable | `only_profitable` | `TELEGRAM_ONLY_PROFITABLE` | `true` |
| Re-alert improvement | `realert_improvement` | `TELEGRAM_REALERT_IMPROVEMENT` | `0.01` |
| Settings file | `settings_file` | `TELEGRAM_SETTINGS_FILE` | `bot-settings.json` |
| Archive file | `archive_file` | `TELEGRAM_ARCHIVE_FILE` | `bot-archive.json` |
| Timezone (hours from UTC) | `tz_offset_hours` | `TELEGRAM_TZ_OFFSET_HOURS` | `0` (UTC) |
| Log level | `log_level` | `TELEGRAM_LOG_LEVEL` | `info` |

The `alert_*` / `only_profitable` values are just the **initial** notification filters;
once running they are changed live from chat (`/set`, `/mute`) and persisted to
`settings_file`.

## Run

```bash
# 1) backend (starts with an empty or persisted watchlist)
./target/release/detector

# 2) telegram bot (token from @BotFather, plus your chat id)
TELEGRAM_BOT_TOKEN=123:abc TELEGRAM_CHAT_ID=123456789 \
  ./target/release/telegram-bot
```

Then, from the chat, add markets to watch — the detector subscribes to their books over
the websocket immediately.

### Telegram commands

| Command | Effect |
|---|---|
| `/add_category <link \| slug>` | Track a whole category (e.g. `polymarket.com/iran`) — every market in it is pulled in and kept in sync |
| `/remove_category <slug>` | Stop tracking a category |
| `/categories` | List tracked categories |
| `/fees` | Configured taker fees per category |
| *(paste a Polymarket link)* | Watch a single market |
| `/add <link \| slug>` | Watch a single market |
| `/remove <slug \| condition_id>` | Unpin a single market |
| `/list` | Watched markets |
| `/search <query>` | Find markets, returns tap-to-copy slugs |
| `/settings` | Show alert filters |
| `/set <key> <value>` | Tune alerts — keys: `enabled`, `only_profitable`, `min_edge`, `min_profit`, `crossings`, `mint`, `split` |
| `/mute`, `/unmute` | Pause / resume alerts |
| `/top` | Quick combined summary of current opportunities |
| `/opportunities` | Each opportunity as its own message with a ✅ *seen* button |
| `/crossings` | The recent micro-crossings as **one digest** with a single ✅ *seen all* button |
| `/archive` | Items you marked as seen, with the time they were filed (bounded, persisted) |
| `/status` | Backend health (ws connected, watched/category counts) |
| `/help`, `/start` | Usage (shows the build's commit hash as `first…last`) |

**Categories** are the main workflow: add one and the detector auto-pulls all its markets
from Polymarket, re-checking every `discover_interval_secs` (default 5 min) to add new
markets and drop resolved ones. Gamma caps offset pagination at ~2000 rows, so a very large
category is limited to its **~2000 highest-volume** active markets (fetched
`order=volumeNum desc`) — the ones actually worth arbing; raise `discover_min_volume` to
focus further. If a category refresh errors partway, that pass keeps existing markets
rather than dropping them. **Notification filters** (`/set`, `/mute`) are held per-bot,
persisted to `settings_file`, and applied to every alert; `mint` and `split` toggle the two
arb directions independently (both on by default). Alerts de-duplicate per market *and*
direction: a market re-alerts on a given direction only when that direction's edge improves
by at least `realert_improvement`.

**Timestamps.** Every alert carries `spotted: <time> (<age> ago)` — when the backend *first*
saw this occurrence of the opportunity, not when the message was sent, so a re-alert still
shows how long the edge has been standing. An opportunity that disappears and comes back
counts as a new occurrence. Micro-crossings show the moment they opened alongside how long
they lived. Times are rendered at `tz_offset_hours` and always carry their zone
(`16:23:41 UTC+3`), with the date added once an item is no longer from today.

**Micro-crossings arrive as a digest**, not one message each: they close in bursts and are
only interesting in aggregate. Each digest (from `/crossings` or from a live poll) gets a
single **✅ Seen all** button that files every crossing in it to the archive at once; a
digest too long for one Telegram message is split, with the button on the last part.

## REST API

| Method / Path | Description |
|---|---|
| `GET /health` | ws connection, watched count, per-direction opportunity counts, last-update age |
| `GET /config` | Effective arbitrage parameters |
| `GET /opportunities` | Current opportunities, both directions (sorted by net profit) |
| `GET /crossings?after_seq=&limit=` | Closed micro-crossings with lifetime, direction and peak edge |
| `GET /categories` | List tracked categories |
| `POST /category` | Track a category — body `{"input":"<link\|slug\|condition_id>"}` |
| `DELETE /category/:slug` | Stop tracking a category |
| `GET /watch` | List watched markets |
| `POST /watch` | Pin a single market — body `{"input":"<link\|slug\|condition_id>"}` |
| `DELETE /watch/:key` | Unpin a market by slug or condition id |
| `GET /search?q=&limit=` | Search markets |

`GET /opportunities` filters: `profitable=true|false`, `min_edge=<f>`, `min_profit=<f>`,
`strategy=mint|split`, `limit=<n>`. Each opportunity carries market metadata plus an `arb`
object (`strategy`, `best_yes`, `best_no`, `sum`, `gross_edge`, `size`, `cost`, `payout`,
`fees`, `gas`, `net_profit`, `roi`, `avg_edge`, `profitable`), plus `first_seen` (unix
seconds, when this occurrence appeared) and `updated_at` (last recompute).

`strategy` is `mint` (best_yes/best_no are **asks**, `cost` is the ladder spend, `payout`
is `size`) or `split` (best_yes/best_no are **bids**, `cost` is `size` — $1 per minted set
— and `payout` is the ladder proceeds). `gross_edge` is `1 − sum` for mint and `sum − 1`
for split, so the same `min_edge` threshold applies to both. The same market can appear
once per direction.

## Docker & deploy

Both services ship as slim images built from one multi-stage `Dockerfile`
(`--build-arg BIN=detector|telegram-bot`). Run the pair with Compose:

```bash
cp .env.example .env      # fill in TELEGRAM_BOT_TOKEN / TELEGRAM_CHAT_ID
docker compose up -d --build
```

The detector's watchlist is persisted to a named volume (`arb-data`), the bot
reaches the detector over the Compose network (`http://detector:8080`), and the
detector's port is published only on `127.0.0.1`.

### CI/CD (GitHub Actions)

- **`.github/workflows/ci.yml`** — on every push/PR: `cargo fmt --check`,
  `clippy -D warnings`, `cargo test`.
- **`.github/workflows/deploy.yml`** — on push to `main`: builds and pushes both
  images to GHCR (`ghcr.io/<owner>/<repo>/{detector,telegram-bot}`), then over
  SSH runs `docker compose pull && up -d` on the VPS. Rust is compiled on GitHub
  runners, so the VPS only pulls images — no heavy build on a small box.

**Repository secrets** to set (Settings → Secrets → Actions):

| Secret | Purpose |
|---|---|
| `VPS_HOST` | VPS address |
| `VPS_USER` | SSH user |
| `VPS_SSH_KEY` | Private key for that user (no passphrase) |
| `VPS_PORT` | SSH port (e.g. `22` or a custom one) |
| `VPS_PATH` | Directory holding `docker-compose.yml` + `.env` |

`GITHUB_TOKEN` is provided automatically and is used to push/pull GHCR images.

**One-time VPS setup:**

```bash
# install Docker Engine + compose plugin, then:
mkdir -p /opt/polymarket-arb && cd /opt/polymarket-arb   # == VPS_PATH
cat > .env <<'EOF'
TELEGRAM_BOT_TOKEN=123:abc
TELEGRAM_CHAT_ID=123456789
# optional tuning, e.g. DETECTOR_GAS_COST=0.02, DETECTOR_MIN_EDGE=0.005
EOF
```

The deploy workflow copies `docker-compose.yml` here and pins the images to the
commit SHA on each run; `.env` stays on the VPS and is never overwritten.

## Layout

```
crates/
  core/          arbitrage math + types (pure, unit-tested)
  detector/
    config.rs    layered config
    gamma.rs     Gamma REST: resolve link/slug, search (cold path)
    ws.rs        CLOB websocket: live books + recompute
    state.rs     watchlist, live books, opportunities, persistence
    main.rs      REST server wiring
  telegram-bot/  standalone alerter + watchlist commands
config/          example YAML for each binary
```

## Scope

This project is deliberately **detection and alerting only**. It reads public market data
and reports opportunities. It does not connect a wallet, hold keys, submit orders, or move
funds, and it is not trading advice.
