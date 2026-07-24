# Polymarket YES/NO arbitrage alerter

Alerts-only tooling for the classic Polymarket YES/NO mint arbitrage. It **detects**
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

That gives a risk-free edge whenever both legs can be **bought** below par:

```
ask(YES) + ask(NO) < 1   →   buy both   →   redeem/merge for 1   →   profit = 1 − sum
```

Worked example (from the brief): YES ask `0.24`, NO ask `0.70`. Sum is `0.94`, so a set
costs `0.94` and settles to `1.00` regardless of outcome — profit `0.06` per set.

> **Trigger direction.** The alert condition is `ask(YES) + ask(NO) < 1`, i.e. **sum below
> 1**, not above. `bid(YES) + bid(NO) > 1` is the *mirror* case, realizable only if you
> already hold a set to sell/merge; the detector surfaces it as an informational
> `sell_side_edge` field, but the primary alert is the buy-both case.

### Does it clear costs?

Each opportunity is reported with the full economics so you can judge it before acting:

- **Order-book depth** — the engine sweeps both ask ladders together and stacks sets only
  while the *marginal* set stays below 1, so `size`/`cost`/`net_profit` reflect real
  executable depth, not just top-of-book.
- **Fees** — `fee_bps`, applied per side as `rate · min(price, 1−price) · shares`
  (Polymarket's CLOB fee is currently 0).
- **Slippage** — `slippage_bps` pads each buy price.
- **Gas** — `gas_cost`, the fixed USDC cost of the closing `merge`/`redeem`, subtracted
  from net profit; it is what kills thin edges on small size.
- **Capital** — `max_capital` caps the assumed set size.

An opportunity is flagged `profitable` only when `net_profit ≥ min_profit` after all of
the above.

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
- **Residual risks even in the "risk-free" trade.** Partial fills (one leg only), price
  moving between fills, approvals/USDC bridging friction, market invalidation. None are
  handled here because the project intentionally stops at detection.

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
| *(paste a Polymarket link)* | Watch a single market |
| `/add <link \| slug>` | Watch a single market |
| `/remove <slug \| condition_id>` | Unpin a single market |
| `/list` | Watched markets |
| `/search <query>` | Find markets, returns tap-to-copy slugs |
| `/settings` | Show alert filters |
| `/set <key> <value>` | Tune alerts — keys: `enabled`, `only_profitable`, `min_edge`, `min_profit` |
| `/mute`, `/unmute` | Pause / resume alerts |
| `/top` | Current best opportunities |
| `/status` | Backend health (ws connected, watched/category counts) |
| `/help`, `/start` | Usage |

**Categories** are the main workflow: add one and the detector auto-pulls all its markets
from Polymarket, re-checking every `discover_interval_secs` (default 5 min) to add new
markets and drop resolved ones. **Notification filters** (`/set`, `/mute`) are held per-bot,
persisted to `settings_file`, and applied to every alert. Alerts de-duplicate per market:
a market re-alerts only when its edge improves by at least `realert_improvement`.

## REST API

| Method / Path | Description |
|---|---|
| `GET /health` | ws connection, watched count, last-update age |
| `GET /config` | Effective arbitrage parameters |
| `GET /opportunities` | Current opportunities (sorted by net profit) |
| `GET /categories` | List tracked categories |
| `POST /category` | Track a category — body `{"input":"<link\|slug\|condition_id>"}` |
| `DELETE /category/:slug` | Stop tracking a category |
| `GET /watch` | List watched markets |
| `POST /watch` | Pin a single market — body `{"input":"<link\|slug\|condition_id>"}` |
| `DELETE /watch/:key` | Unpin a market by slug or condition id |
| `GET /search?q=&limit=` | Search markets |

`GET /opportunities` filters: `profitable=true|false`, `min_edge=<f>`, `min_profit=<f>`,
`limit=<n>`. Each opportunity carries market metadata plus an `arb` object
(`best_yes`, `best_no`, `sum`, `gross_edge`, `size`, `cost`, `fees`, `gas`, `net_profit`,
`roi`, `profitable`) and an optional `sell_side_edge`.

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
