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
   ┌────────────────────────────────────────────────────┐        ┌──────────────┐
   │ detector  — live books → arb math → opportunities   │───────▶│  ClickHouse  │
   └───────────────┬──────────────────────┬─────────────┘  batch │  (own        │
       REST /watch,/search (manage)   REST /opportunities   write │   container) │
                   │                      │   /analytics/* ◀──────└──────────────┘
                   ▼                      ▼         │        query
              ┌──────────────┐   Bot API   ┌──────────┐
              │ telegram-bot │ ───────────▶│ Telegram │
              │  + charts    │   photos    └──────────┘
              └──────────────┘
```

The detector owns ClickHouse; the bot never talks to it, so store credentials
stay in one process and the bot keeps talking only to the backend and the
Telegram API. Writes are fire-and-forget — analytics is a diary, never a
dependency, and a ClickHouse that is down, slow or simply not configured cannot
delay or break an alert.

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
  execution layer on top of. Don't take that on faith either: `/decay` measures it on your
  own feed, and a median lifetime in the low hundreds of milliseconds is the answer.
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

## Analytics

Alerts tell you what is happening now; they cannot tell you *why the last month
looked the way it did*. Every signal is therefore also written to ClickHouse, and
the bot renders that history as charts in chat.

### What is measured, and what cannot be

The metrics split into layers, and it matters which of them this project is in a
position to observe. **It never places an order**, so anything that only exists
once you have traded is modelled, not realised:

| Layer | Recorded | How |
|---|---|---|
| **Market state at the signal** | ✅ measured | both books' top *N* levels each side, per-leg spreads, `ask_sum`/`bid_sum`, resting notional, level counts, snapshot age |
| **Traded volume** | ⚠️ best effort | trade prints seen on the market channel (`last_trade_price`), as a rolling 5-minute count and notional. If the feed stops carrying them this reads 0 — no separate subscription is made |
| **Edge** | ✅ measured | `gross_edge` (top of book) and `avg_edge` (what actually clears at size), per occurrence |
| **Edge decay** | ✅ measured | every crossing's lifetime, time-to-peak, observation count, and the gap since the same market last produced one |
| **Costs** | ⚠️ modelled | the fee/slippage/gas *parameters in force* are stored on every row, so a later re-read knows what the numbers assumed. Real gas prices and real fills are not observable from here |
| **Capital & risk** | ⚠️ modelled | capital the trade would lock, and seconds to resolution (the opportunity cost of holding a set instead of merging) |
| **Detection latency** | ✅ measured | websocket frame → opportunity stored, per signal. This is the detector's own share of signal-to-action, not a round trip to an exchange |
| **Execution, slippage, toxic fills** | ❌ not observable | no order is ever placed |

That last row is the honest one. Quoted-vs-filled price, block confirmation
times, real gas, partial and toxic fills are properties of *executing*, and this
repo stops at detection. The `executions` table exists and is fully specified
anyway, so an out-of-scope execution layer can `POST /analytics/execution` and
have its fills line up against the signals that produced them via
`opportunity_id`, with no migration and no schema archaeology. Until then it
stays empty and every execution view says so.

### Storage

One database, `arb`, in its own container. The schema is applied by the detector
on every boot (`CREATE … IF NOT EXISTS`), so it travels with the binary rather
than with a container init script that only runs on a fresh volume.

| Table | One row per | Ordered by |
|---|---|---|
| `opportunities` | observation of a live arb (`open` / `update` / `close`) | `(condition_id, ts)` |
| `crossings` | arb that has **closed**, with its lifetime | `(condition_id, closed_at)` |
| `book_samples` | periodic probe of a watched market, arb or not | `(condition_id, ts)` |
| `trades` | observed trade print | `(condition_id, ts)` |
| `executions` | leg of a real trade — **empty here** | `(condition_id, ts)` |

Wide and denormalised, the shape ClickHouse rewards: an `opportunities` row is
self-contained, so "what did the book look like when this edge existed" never
needs a join. Everything is `PARTITION BY toYYYYMMDD(...)` with a TTL
(`analytics_ttl_days`, default 90) that drops whole partitions.

Three `AggregatingMergeTree` rollups sit on top — `hourly`, `market_daily`,
`book_hourly`. Be clear about which of these earns its keep: at this project's
event rate the opportunity rollups are headroom, because real set-arb is rare
and `opportunities` stays small. **`book_hourly` is the one that matters now** —
a full watchlist sampled every few minutes is hundreds of thousands of rows a
day, and the liquidity views read the rollup instead of the raw table.

Two things about the write path are deliberate:

- **A standing arb is throttled.** It is re-evaluated on every book delta, so
  only one `update` row per `analytics_sample_ms` (default 500) is kept while it
  stands. Without this a single busy market would be the entire dataset. The
  `open` and `close` of every occurrence are always recorded, and `open` is what
  every query counts as "an occurrence".
- **Describing the market state is skipped unless a row will come of it.**
  Cutting four ladders costs a sort and a lock, so it only happens when an arb is
  live or has just closed. Every other recompute — the overwhelming majority —
  never touches analytics at all.

### In chat

`/analytics` opens a menu; every view is also a command, and any of them takes a
window (`24h`, `7d`, `30d`, `90d`):

| Command | Chart | The question it answers |
|---|---|---|
| `/report [w]` | dashboard | everything at a glance, one image |
| `/edge [w]` | edge distribution + retention curve | **should I arb from 0.5% or from 2%?** The lower panel is the share of occurrences a given cut-off keeps, so a cliff in it is a threshold worth sitting just below |
| `/decay [w]` | lifetime histogram + survival curve | **can I technically compete at all?** If the mass sits left of a few hundred ms, it is a latency race, not an edge you can size into |
| `/timeline [w]` | count and average edge over time | when the feed was busy, and whether the edges were fat or thin |
| `/markets [w]` | top markets by occurrence | which markets actually produce edges — with median lifetime beside the count, because frequency alone is not opportunity |
| `/heatmap [w]` | weekday × hour | when opportunities appear, in your timezone |
| `/size [w]` | realised vs top-of-book edge by size | **does my own size eat the edge?** The bar is what clears; the gap up to the marker is your own sweep down the ladder |
| `/stats [w]` | — | the same numbers as text |
| `/export <table> [w]` | — | raw rows as a CSV file |

Each chart arrives as a PNG with a window switcher underneath, and a caption
carrying the same headline numbers it plots — a picture is not a readable data
source on its own, so nothing here is reachable only by looking at colour.
`/stats` and `/export` are the full text and machine-readable paths.

Charts are rendered in-process to PNG with the two Noto Sans faces vendored in
`crates/telegram-bot/assets/` (OFL, licence alongside them) compiled into the
binary. No browser, no network, and no fonts or fontconfig in the runtime image.

### Reading the charts against the trade

The honest use of this data is mostly negative — it is very good at telling you
an edge was never takeable:

- A median lifetime well under a second, with the mass of the decay curve in the
  first buckets, means the edge closed before any non-colocated executor could
  have acted. Frequency in that regime is not opportunity.
- A retention curve that falls off a cliff early means most of the feed is
  marginal; raising the cut-off costs little and each surviving signal has to
  carry only one fixed gas leg.
- A large depth haircut in the size chart means the headline edge is
  top-of-book fiction at the size you would actually want.

## Build

```bash
cargo build --release
cargo test --all   # arbitrage math, analytics queries, chart rendering
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
| ClickHouse URL *(empty = analytics off)* | `clickhouse_url` | `DETECTOR_CLICKHOUSE_URL` | *(empty)* |
| ClickHouse database | `clickhouse_db` | `DETECTOR_CLICKHOUSE_DB` | `arb` |
| ClickHouse user / password | `clickhouse_user` / `clickhouse_password` | `DETECTOR_CLICKHOUSE_USER` / `_PASSWORD` | `default` / *(empty)* |
| Live-arb sample gap (ms) | `analytics_sample_ms` | `DETECTOR_ANALYTICS_SAMPLE_MS` | `500` |
| Book probe interval (s, 0=off) | `analytics_book_sample_secs` | `DETECTOR_ANALYTICS_BOOK_SAMPLE_SECS` | `300` |
| Trade window (s) | `analytics_trade_window_secs` | `DETECTOR_ANALYTICS_TRADE_WINDOW_SECS` | `300` |
| Book levels captured per side | `analytics_depth_levels` | `DETECTOR_ANALYTICS_DEPTH_LEVELS` | `5` |
| Insert batch / flush (s) | `analytics_batch_rows` / `analytics_flush_secs` | `DETECTOR_ANALYTICS_BATCH_ROWS` / `_FLUSH_SECS` | `500` / `2` |
| Queue cap (rows) | `analytics_queue_cap` | `DETECTOR_ANALYTICS_QUEUE_CAP` | `50000` |
| Retention (days) | `analytics_ttl_days` | `DETECTOR_ANALYTICS_TTL_DAYS` | `90` |

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
| `/crossings` | Micro-crossings you have not filed yet, as **one digest** with a single ✅ *seen all* button |
| `/archive` | Items you marked as seen, with the time they were filed (bounded, persisted) |
| `/status` | Backend health (ws connected, watched/category counts) |
| `/analytics [window]` | Menu of every analytics view, with a window switcher |
| `/report`, `/edge`, `/decay`, `/timeline`, `/markets`, `/heatmap`, `/size` | The charts, each also accepting a window (`/edge 30d`) |
| `/stats [window]` | The analytics summary as text |
| `/export <table> [window]` | Raw rows as CSV (`opportunities`, `crossings`, `book_samples`, `trades`, `executions`) |
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
shows how long the edge has been standing. Alerts also show the market's **trading window**
(`market opened:` / `market closes:` with time-to-close), and `/list` / `/search` append a
compact `12 Jan 2026 → 3 Nov 2026` to each market. Both dates come from Gamma and are
refreshed on every discovery pass. An opportunity that disappears and comes back
counts as a new occurrence. Micro-crossings show the moment they opened alongside how long
they lived. Times are rendered at `tz_offset_hours` and always carry their zone
(`16:23:41 UTC+3`), with the date added once an item is no longer from today.

**Micro-crossings arrive as a digest**, not one message each: they close in bursts and are
only interesting in aggregate. Each digest (from `/crossings` or from a live poll) gets a
single **✅ Seen all** button that files every crossing in it to the archive at once; a
digest too long for one Telegram message is split, with the button on the last part.

Filing is also what makes a crossing *go away*: pressing ✅ advances an acknowledgement
cursor (the highest backend `seq` in the batch, persisted in `settings_file`), and
`/crossings` lists only what came after it. Without that cursor the command re-lists the
backend's whole retained log every time, so the same crossings keep reappearing after you
have marked them seen. The cursor covers every crossing the bot fetched for that batch —
including ones the digest truncated or the filters hid — so acknowledging never leaves
older entries stranded behind it.

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
| `GET /analytics/health` | Analytics state: enabled, schema ready, rows written/dropped |
| `GET /analytics/summary` | Per-strategy headline numbers plus lifetimes and coverage |
| `GET /analytics/edge_histogram` | Edge distribution, bucketed |
| `GET /analytics/decay` | Crossing lifetimes, bucketed, with quantiles |
| `GET /analytics/timeline` | Count and average edge per time bucket |
| `GET /analytics/markets` | Per-market frequency, edge and median lifetime |
| `GET /analytics/categories` | Rollup by market category and liquidity tier |
| `GET /analytics/heatmap` | Weekday × hour counts, at a `tz` offset |
| `GET /analytics/size_curve` | Realised vs top-of-book edge by position size |
| `GET /analytics/liquidity` | Baseline spread/depth from the hourly book rollup |
| `GET /analytics/executions` | Realised execution quality — empty unless something posts to it |
| `GET /analytics/export?table=` | Raw rows as CSV |
| `POST /analytics/execution` | Log one leg of a real trade (for an out-of-scope executor) |

Analytics endpoints take `window=` (`30m`, `24h`, `7d`, `90d`, or bare seconds)
and, where it applies, `strategy=mint|split`, `limit=`, `bucket=`, `bins=`,
`bucket_secs=`, `tz=` (minutes east of UTC). They answer `503` while analytics is
disabled or still connecting, so the bot can say so plainly. Every value that
reaches a query is a clamped number or a whitelisted identifier — none of it is
interpolated caller text.

`GET /opportunities` filters: `profitable=true|false`, `min_edge=<f>`, `min_profit=<f>`,
`strategy=mint|split`, `limit=<n>`. Each opportunity carries market metadata plus an `arb`
object (`strategy`, `best_yes`, `best_no`, `sum`, `gross_edge`, `size`, `cost`, `payout`,
`fees`, `gas`, `net_profit`, `roi`, `avg_edge`, `profitable`), plus `start_date`/`end_date`
(ISO 8601 trading window), `first_seen` (unix seconds, when this occurrence appeared) and
`updated_at` (last recompute). `GET /watch` and `GET /search` carry the same two dates.

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

**ClickHouse runs as its own service** (`clickhouse`, volume `clickhouse-data`,
port published on loopback only). Compose points the detector at it and creates
the `arb` database; the detector applies the schema itself and retries until the
server answers, so `docker compose up` in any order works and a ClickHouse that
never comes up costs you analytics, not alerts. Change the credentials by setting
`DETECTOR_CLICKHOUSE_USER`/`_PASSWORD` in `.env` — the defaults are `arb`/`arb`
and are only safe because nothing is exposed off-host.

Stock ClickHouse assumes it owns the machine and will reserve most of the RAM
for caches, which is wrong on a box also running the detector and the bot.
`clickhouse/config.d/low-resources.xml` caps it at ~1 GiB, trims the background
pools, and drops the system logs nothing here reads. Raise those if you give
analytics its own host.

Ad-hoc SQL from the box, when you want something the charts do not cover:

```bash
curl 'http://localhost:8123/?user=arb&password=arb' \
  -d "SELECT strategy, count(), round(quantile(0.5)(lived_ms)) AS median_ms
      FROM arb.crossings WHERE closed_at >= now() - INTERVAL 7 DAY
      GROUP BY strategy"
```

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
    time.rs      calendar arithmetic shared by both binaries
  detector/
    config.rs    layered config
    gamma.rs     Gamma REST: resolve link/slug, search (cold path)
    ws.rs        CLOB websocket: live books + recompute
    state.rs     watchlist, live books, opportunities, persistence
    analytics/   ClickHouse: schema, batched writer, query layer
    main.rs      REST server wiring
  telegram-bot/
    analytics.rs fetch aggregates, caption them, push to chat
    charts/      PNG rendering (canvas primitives + the chart set)
    assets/      vendored Noto Sans faces (OFL)
    main.rs      alerter + watchlist commands
config/          example YAML for each binary
clickhouse/      container config (resource caps for a small VPS)
```

## Scope

This project is deliberately **detection, alerting and analysis only**. It reads public
market data, reports opportunities, and keeps a history of them so you can judge whether
the trade is worth building. It does not connect a wallet, hold keys, submit orders, or
move funds, and it is not trading advice.

The analytics is scoped the same way. It measures what a detector can see — book state,
edge, how long the edge lived, how fast it was spotted — and is explicit that costs and
capital are *modelled* from the configured parameters, not realised. Slippage, gas,
confirmation latency and toxic fills belong to execution; the `executions` table is
specified and reachable over `POST /analytics/execution` so an external executor can fill
it in, and it stays empty until one does.
