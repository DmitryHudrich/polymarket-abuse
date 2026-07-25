//! ClickHouse DDL, applied idempotently by the detector on every boot so the
//! schema travels with the binary rather than with the container's init scripts
//! (which only ever run on a *fresh* volume).
//!
//! Layout follows the shape ClickHouse rewards: a few wide, denormalised event
//! tables instead of a normalised graph, partitioned by day and ordered by
//! `(market, time)` so any per-market time slice is a short range scan.
//!
//! Five tables, each answering a different question:
//!
//! | table | one row per | answers |
//! |---|---|---|
//! | `opportunities` | observation of a live arb | edge distribution, costs, capital |
//! | `crossings` | arb that has *closed* | edge decay, how fast the market eats it |
//! | `book_samples` | periodic per-market probe | baseline spread/depth, liquidity |
//! | `trades` | observed trade print | real traded volume per token |
//! | `executions` | leg of an actually-placed trade | slippage, latency, toxic fills |

/// Every statement, in dependency order. `{db}` is the database name and
/// `{ttl}` the retention in days; both are substituted before execution.
pub const STATEMENTS: &[&str] = &[
    "CREATE DATABASE IF NOT EXISTS {db}",
    OPPORTUNITIES,
    CROSSINGS,
    BOOK_SAMPLES,
    TRADES,
    EXECUTIONS,
    HOURLY,
    HOURLY_MV,
    MARKET_DAILY,
    MARKET_DAILY_MV,
    BOOK_HOURLY,
    BOOK_HOURLY_MV,
];

/// The wide event table. One row every time the detector *sees* an arb: `open`
/// when an occurrence appears, `update` while it stands (throttled — see
/// `sample_ms`), `close` when it goes away. Market state is captured alongside
/// the economics, so a row is self-contained: you never need a join to ask
/// "what did the book look like when this edge existed".
const OPPORTUNITIES: &str = r#"
CREATE TABLE IF NOT EXISTS {db}.opportunities
(
    ts                  DateTime64(3),
    event               LowCardinality(String),   -- open | update | close
    opportunity_id      String,                   -- ties one occurrence's rows together
    strategy            LowCardinality(String),   -- mint | split
    condition_id        String,
    slug                String,
    question            String,
    category            LowCardinality(String),

    -- Layer 1: market state at the moment of the signal.
    yes_bid             Float64,
    yes_ask             Float64,
    no_bid              Float64,
    no_ask              Float64,
    yes_spread          Float64,
    no_spread           Float64,
    ask_sum             Float64,                  -- implied sum for mint
    bid_sum             Float64,                  -- implied sum for split
    yes_ask_prices      Array(Float64),           -- top levels, best first
    yes_ask_sizes       Array(Float64),
    yes_bid_prices      Array(Float64),
    yes_bid_sizes       Array(Float64),
    no_ask_prices       Array(Float64),
    no_ask_sizes        Array(Float64),
    no_bid_prices       Array(Float64),
    no_bid_sizes        Array(Float64),
    yes_depth_usd       Float64,                  -- notional across the captured levels
    no_depth_usd        Float64,
    yes_levels          UInt16,                   -- levels present in the full book
    no_levels           UInt16,
    book_age_ms         UInt32,                   -- since the older leg last changed
    trades_5m           UInt32,                   -- prints seen on both legs, last 5 min
    traded_5m_usd       Float64,
    market_volume       Float64,
    liquidity_tier      LowCardinality(String),

    -- Layer 4: edge, and Layer 3: the costs it has to clear.
    gross_edge          Float64,                  -- top of book, before the sweep
    avg_edge            Float64,                  -- volume-weighted, what actually clears
    size                Float64,
    cost                Float64,
    payout              Float64,
    fees                Float64,
    gas                 Float64,
    net_profit          Float64,
    roi                 Float64,
    profitable          UInt8,
    depth_cost_usd      Float64,                  -- (gross_edge - avg_edge) * size
    fee_bps             Float64,                  -- modelled params in force
    slippage_bps        Float64,
    gas_cost            Float64,
    max_capital         Float64,

    -- Layer 5: capital and its opportunity cost.
    capital_locked      Float64,
    secs_to_resolution  Int64,

    -- Layer 2/6: how fast we saw it, and how long it has been standing.
    detect_latency_us   UInt32,                   -- ws frame -> opportunity stored
    age_ms              UInt32,                   -- since this occurrence opened
    observation         UInt32                    -- 1-based index within the occurrence
)
ENGINE = MergeTree
PARTITION BY toYYYYMMDD(ts)
ORDER BY (condition_id, ts)
TTL toDateTime(ts) + INTERVAL {ttl} DAY
"#;

/// One row per *closed* micro-crossing — the edge-decay table. `lived_ms` is the
/// headline: it says whether you could technically have competed at all.
const CROSSINGS: &str = r#"
CREATE TABLE IF NOT EXISTS {db}.crossings
(
    closed_at           DateTime64(3),
    opened_at           DateTime64(3),
    seq                 UInt64,
    opportunity_id      String,
    condition_id        String,
    slug                String,
    question            String,
    category            LowCardinality(String),
    strategy            LowCardinality(String),

    lived_ms            UInt32,                   -- how long the edge was visible
    observations        UInt32,                   -- book updates it survived
    time_to_peak_ms     UInt32,                   -- how long until it was at its best

    peak_avg_edge       Float64,
    peak_gross_edge     Float64,
    peak_net            Float64,
    peak_size           Float64,
    first_avg_edge      Float64,
    last_avg_edge       Float64,
    peak_profitable     UInt8,

    market_volume       Float64,
    liquidity_tier      LowCardinality(String),
    reopen_gap_ms       UInt32                    -- since the last one here closed; 0 = first
)
ENGINE = MergeTree
PARTITION BY toYYYYMMDD(closed_at)
ORDER BY (condition_id, closed_at)
TTL toDateTime(closed_at) + INTERVAL {ttl} DAY
"#;

/// Periodic probe of every watched market, arb or no arb. This is the baseline
/// the opportunity rows are read against: without it you only ever see the
/// moments something happened, and can't tell a quiet market from a tight one.
const BOOK_SAMPLES: &str = r#"
CREATE TABLE IF NOT EXISTS {db}.book_samples
(
    ts                  DateTime64(3),
    condition_id        String,
    slug                String,
    category            LowCardinality(String),
    market_volume       Float64,
    liquidity_tier      LowCardinality(String),

    yes_bid             Float64,
    yes_ask             Float64,
    no_bid              Float64,
    no_ask              Float64,
    yes_spread          Float64,
    no_spread           Float64,
    ask_sum             Float64,
    bid_sum             Float64,
    yes_depth_usd       Float64,
    no_depth_usd        Float64,
    yes_levels          UInt16,
    no_levels           UInt16,

    book_updates        UInt32,                   -- since the previous sample
    trades_5m           UInt32,
    traded_5m_usd       Float64,
    secs_to_resolution  Int64
)
ENGINE = MergeTree
PARTITION BY toYYYYMMDD(ts)
ORDER BY (condition_id, ts)
TTL toDateTime(ts) + INTERVAL {ttl} DAY
"#;

/// Trade prints seen on the market websocket. Only populated when the CLOB emits
/// `last_trade_price` on the channel we already subscribe to — no extra
/// subscription, so if the feed stops carrying it this table simply stays empty
/// and the `traded_5m_usd` columns read 0.
const TRADES: &str = r#"
CREATE TABLE IF NOT EXISTS {db}.trades
(
    ts                  DateTime64(3),
    condition_id        String,
    token_id            String,
    outcome             LowCardinality(String),   -- yes | no
    side                LowCardinality(String),
    price               Float64,
    size                Float64,
    notional            Float64
)
ENGINE = MergeTree
PARTITION BY toYYYYMMDD(ts)
ORDER BY (condition_id, ts)
TTL toDateTime(ts) + INTERVAL {ttl} DAY
"#;

/// Realised fills. **This project never places an order, so nothing here writes
/// to this table** — it is the landing zone for an out-of-scope execution layer,
/// filled over `POST /analytics/execution`. It exists now so that the day one
/// arrives, quoted-vs-filled, latency, gas and toxic fills line up against the
/// signals in `opportunities` by `opportunity_id` with no migration.
const EXECUTIONS: &str = r#"
CREATE TABLE IF NOT EXISTS {db}.executions
(
    ts                  DateTime64(3),
    opportunity_id      String,                   -- joins to opportunities
    condition_id        String,
    strategy            LowCardinality(String),
    leg                 LowCardinality(String),   -- yes | no | ctf
    status              LowCardinality(String),   -- filled | partial | rejected | expired

    signal_ts           DateTime64(3),
    submit_ts           DateTime64(3),
    confirm_ts          DateTime64(3),
    signal_to_submit_ms UInt32,                   -- the number that decides if you can compete
    submit_to_confirm_ms UInt32,

    quoted_price        Float64,
    filled_price        Float64,
    slippage_price      Float64,
    slippage_usd        Float64,
    requested_size      Float64,
    filled_size         Float64,
    fill_ratio          Float64,

    fee_usd             Float64,
    gas_price_gwei      Float64,
    gas_used            UInt64,
    gas_cost_usd        Float64,

    tx_hash             String,
    block_number        UInt64,
    realized_edge       Float64,
    net_pnl             Float64,
    toxic_fill          UInt8,                    -- this leg filled, the other did not
    capital_locked      Float64,
    note                String
)
ENGINE = MergeTree
PARTITION BY toYYYYMMDD(ts)
ORDER BY (condition_id, ts)
TTL toDateTime(ts) + INTERVAL {ttl} DAY
"#;

// --- Rollups -------------------------------------------------------------
//
// At this project's event rate the opportunity rollups are headroom rather than
// a present need (real set-arb is rare, so `opportunities` stays small). The one
// that earns its keep immediately is `book_hourly`: a full watchlist sampled
// every few minutes is hundreds of thousands of rows a day, and the liquidity
// charts read it instead of the raw table.

const HOURLY: &str = r#"
CREATE TABLE IF NOT EXISTS {db}.hourly
(
    hour            DateTime,
    strategy        LowCardinality(String),
    category        LowCardinality(String),
    occurrences     SimpleAggregateFunction(sum, UInt64),
    profitable      SimpleAggregateFunction(sum, UInt64),
    markets         AggregateFunction(uniq, String),
    edge_q          AggregateFunction(quantiles(0.5, 0.9, 0.99), Float64),
    edge_avg        AggregateFunction(avg, Float64),
    edge_max        SimpleAggregateFunction(max, Float64),
    net_sum         SimpleAggregateFunction(sum, Float64),
    size_avg        AggregateFunction(avg, Float64)
)
ENGINE = AggregatingMergeTree
PARTITION BY toYYYYMM(hour)
ORDER BY (hour, strategy, category)
"#;

const HOURLY_MV: &str = r#"
CREATE MATERIALIZED VIEW IF NOT EXISTS {db}.hourly_mv TO {db}.hourly AS
SELECT
    toStartOfHour(ts)                         AS hour,
    strategy,
    category,
    count()                                   AS occurrences,
    countIf(profitable = 1)                   AS profitable,
    uniqState(condition_id)                   AS markets,
    quantilesState(0.5, 0.9, 0.99)(avg_edge)  AS edge_q,
    avgState(avg_edge)                        AS edge_avg,
    max(avg_edge)                             AS edge_max,
    sum(net_profit)                           AS net_sum,
    avgState(size)                            AS size_avg
FROM {db}.opportunities
WHERE event = 'open'
GROUP BY hour, strategy, category
"#;

const MARKET_DAILY: &str = r#"
CREATE TABLE IF NOT EXISTS {db}.market_daily
(
    day             Date,
    condition_id    String,
    slug            String,
    strategy        LowCardinality(String),
    category        LowCardinality(String),
    occurrences     SimpleAggregateFunction(sum, UInt64),
    profitable      SimpleAggregateFunction(sum, UInt64),
    edge_avg        AggregateFunction(avg, Float64),
    edge_max        SimpleAggregateFunction(max, Float64),
    net_sum         SimpleAggregateFunction(sum, Float64),
    net_max         SimpleAggregateFunction(max, Float64),
    size_avg        AggregateFunction(avg, Float64)
)
ENGINE = AggregatingMergeTree
PARTITION BY toYYYYMM(day)
ORDER BY (day, condition_id, strategy)
"#;

const MARKET_DAILY_MV: &str = r#"
CREATE MATERIALIZED VIEW IF NOT EXISTS {db}.market_daily_mv TO {db}.market_daily AS
SELECT
    toDate(ts)              AS day,
    condition_id,
    any(slug)               AS slug,
    strategy,
    any(category)           AS category,
    count()                 AS occurrences,
    countIf(profitable = 1) AS profitable,
    avgState(avg_edge)      AS edge_avg,
    max(avg_edge)           AS edge_max,
    sum(net_profit)         AS net_sum,
    max(net_profit)         AS net_max,
    avgState(size)          AS size_avg
FROM {db}.opportunities
WHERE event = 'open'
GROUP BY day, condition_id, strategy
"#;

const BOOK_HOURLY: &str = r#"
CREATE TABLE IF NOT EXISTS {db}.book_hourly
(
    hour            DateTime,
    condition_id    String,
    category        LowCardinality(String),
    samples         SimpleAggregateFunction(sum, UInt64),
    yes_spread_avg  AggregateFunction(avg, Float64),
    no_spread_avg   AggregateFunction(avg, Float64),
    ask_sum_min     SimpleAggregateFunction(min, Float64),
    bid_sum_max     SimpleAggregateFunction(max, Float64),
    depth_avg       AggregateFunction(avg, Float64),
    updates         SimpleAggregateFunction(sum, UInt64),
    traded_usd      SimpleAggregateFunction(sum, Float64)
)
ENGINE = AggregatingMergeTree
PARTITION BY toYYYYMM(hour)
ORDER BY (hour, condition_id)
"#;

const BOOK_HOURLY_MV: &str = r#"
CREATE MATERIALIZED VIEW IF NOT EXISTS {db}.book_hourly_mv TO {db}.book_hourly AS
SELECT
    toStartOfHour(ts)                       AS hour,
    condition_id,
    any(category)                           AS category,
    count()                                 AS samples,
    avgState(yes_spread)                    AS yes_spread_avg,
    avgState(no_spread)                     AS no_spread_avg,
    min(ask_sum)                            AS ask_sum_min,
    max(bid_sum)                            AS bid_sum_max,
    avgState(yes_depth_usd + no_depth_usd)  AS depth_avg,
    sum(book_updates)                       AS updates,
    sum(traded_5m_usd)                      AS traded_usd
FROM {db}.book_samples
GROUP BY hour, condition_id
"#;
