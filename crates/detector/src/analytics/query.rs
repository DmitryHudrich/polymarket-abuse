//! The read side: one function per `/analytics` endpoint, each returning plain
//! JSON the bot can render without knowing any SQL.
//!
//! Every value that reaches a query string is either a clamped number or a
//! whitelisted identifier — none of it is interpolated user text.

use anyhow::Result;
use serde_json::{json, Value};

use super::Analytics;

/// Lifetime buckets for the decay curve, in milliseconds. Chosen around the
/// question they answer: under ~250 ms nothing you can build competes; past a
/// few seconds the "arb" is usually a stale book, not a real crossing.
pub const LIVED_BUCKETS_MS: &[u32] = &[50, 100, 250, 500, 1_000, 2_000, 5_000, 10_000, 30_000];

/// Size buckets for the "does my own size eat the edge" chart.
pub const SIZE_BUCKETS: &[f64] = &[10.0, 50.0, 100.0, 500.0, 1_000.0];

/// Only these tables can be named by a caller (`/analytics/export`).
pub const EXPORTABLE: &[&str] = &[
    "opportunities",
    "crossings",
    "book_samples",
    "trades",
    "executions",
];

/// Narrow a caller-supplied strategy to a SQL fragment. Anything unrecognised
/// means "both", so a typo widens the query rather than injecting into it.
fn strategy_filter(strategy: Option<&str>) -> String {
    match strategy {
        Some(s) if s.eq_ignore_ascii_case("mint") => " AND strategy = 'mint'".into(),
        Some(s) if s.eq_ignore_ascii_case("split") => " AND strategy = 'split'".into(),
        _ => String::new(),
    }
}

fn clamp(v: u64, lo: u64, hi: u64) -> u64 {
    v.clamp(lo, hi)
}

/// `countIf(...) AS b0, countIf(...) AS b1, …` over an ordered bucket list,
/// plus the open-ended tail bucket.
fn bucket_counts(column: &str, edges: &[u32]) -> String {
    let mut parts = Vec::with_capacity(edges.len() + 1);
    let mut prev = 0u32;
    for (i, edge) in edges.iter().enumerate() {
        parts.push(format!(
            "countIf({column} >= {prev} AND {column} < {edge}) AS b{i}"
        ));
        prev = *edge;
    }
    parts.push(format!("countIf({column} >= {prev}) AS b{}", edges.len()));
    parts.join(",\n    ")
}

/// Human labels matching [`bucket_counts`], for the chart axis.
pub fn lived_bucket_labels() -> Vec<String> {
    let mut out = Vec::new();
    let mut prev = 0u32;
    for edge in LIVED_BUCKETS_MS {
        out.push(format!("{}–{}", fmt_ms(prev), fmt_ms(*edge)));
        prev = *edge;
    }
    out.push(format!("{}+", fmt_ms(prev)));
    out
}

fn fmt_ms(ms: u32) -> String {
    if ms < 1_000 {
        format!("{ms}ms")
    } else {
        format!("{}s", ms / 1_000)
    }
}

pub fn size_bucket_labels() -> Vec<String> {
    let mut out = Vec::new();
    let mut prev = 0.0f64;
    for edge in SIZE_BUCKETS {
        out.push(format!("{}–{}", fmt_size(prev), fmt_size(*edge)));
        prev = *edge;
    }
    out.push(format!("{}+", fmt_size(prev)));
    out
}

fn fmt_size(v: f64) -> String {
    if v >= 1_000.0 {
        format!("{}k", v / 1_000.0)
    } else {
        format!("{v:.0}")
    }
}

/// `multiIf(...)` mapping a numeric column onto its bucket index.
fn bucket_index(column: &str, edges: &[f64]) -> String {
    let mut s = String::from("multiIf(");
    for (i, edge) in edges.iter().enumerate() {
        s.push_str(&format!("{column} < {edge}, {i}, "));
    }
    s.push_str(&format!("{})", edges.len()));
    s
}

// --- Endpoints -----------------------------------------------------------

/// Headline numbers, per strategy: how often an edge appeared, how big it was,
/// what it would have been worth, and how long it survived.
pub async fn summary(a: &Analytics, window_secs: u64, strategy: Option<&str>) -> Result<Value> {
    let db = a.db();
    let w = clamp(window_secs, 60, 400 * 86_400);
    let sf = strategy_filter(strategy);

    let opps = a
        .query(&format!(
            "SELECT
    strategy,
    count()                                     AS occurrences,
    uniq(condition_id)                          AS markets,
    countIf(profitable = 1)                     AS profitable,
    round(avg(avg_edge), 5)                     AS edge_avg,
    round(quantile(0.5)(avg_edge), 5)           AS edge_p50,
    round(quantile(0.9)(avg_edge), 5)           AS edge_p90,
    round(max(avg_edge), 5)                     AS edge_max,
    round(avg(gross_edge), 5)                   AS gross_avg,
    round(avg(gross_edge - avg_edge), 5)        AS depth_haircut,
    round(sum(net_profit), 2)                   AS net_total,
    round(avg(net_profit), 2)                   AS net_avg,
    round(avg(size), 1)                         AS size_avg,
    round(avg(capital_locked), 2)               AS capital_avg,
    round(avg(fees), 4)                         AS fee_avg,
    round(avg(gas), 4)                          AS gas_avg,
    round(avg(detect_latency_us))               AS latency_us_avg,
    round(quantile(0.9)(detect_latency_us))     AS latency_us_p90,
    round(avg(secs_to_resolution))              AS secs_to_resolution_avg
FROM {db}.opportunities
WHERE event = 'open' AND ts >= now() - INTERVAL {w} SECOND{sf}
GROUP BY strategy
ORDER BY strategy"
        ))
        .await?;

    let lived = a
        .query(&format!(
            "SELECT
    strategy,
    count()                             AS closed,
    round(avg(lived_ms))                AS lived_avg,
    round(quantile(0.5)(lived_ms))      AS lived_p50,
    round(quantile(0.9)(lived_ms))      AS lived_p90,
    max(lived_ms)                       AS lived_max,
    countIf(lived_ms < 250)             AS under_250ms,
    countIf(lived_ms < 1000)            AS under_1s,
    round(avg(time_to_peak_ms))         AS time_to_peak_avg,
    round(avg(peak_avg_edge), 5)        AS peak_edge_avg,
    round(avg(reopen_gap_ms))           AS reopen_gap_avg
FROM {db}.crossings
WHERE closed_at >= now() - INTERVAL {w} SECOND{sf}
GROUP BY strategy
ORDER BY strategy"
        ))
        .await?;

    let coverage = a
        .query(&format!(
            "SELECT
    (SELECT count() FROM {db}.book_samples WHERE ts >= now() - INTERVAL {w} SECOND)   AS book_samples,
    (SELECT uniq(condition_id) FROM {db}.book_samples WHERE ts >= now() - INTERVAL {w} SECOND) AS markets_sampled,
    (SELECT count() FROM {db}.trades WHERE ts >= now() - INTERVAL {w} SECOND)         AS trades,
    (SELECT round(sum(notional), 2) FROM {db}.trades WHERE ts >= now() - INTERVAL {w} SECOND) AS traded_usd,
    (SELECT count() FROM {db}.executions WHERE ts >= now() - INTERVAL {w} SECOND)     AS executions"
        ))
        .await?;

    Ok(json!({
        "window_secs": w,
        "by_strategy": opps,
        "lifetime": lived,
        "coverage": coverage.first().cloned().unwrap_or(json!({})),
    }))
}

/// Distribution of realised edge at the moment of the signal, plus the
/// cumulative count at or above each bucket — the curve you read to pick a
/// cut-off ("arb from 0.5% or from 2%?").
pub async fn edge_histogram(
    a: &Analytics,
    window_secs: u64,
    strategy: Option<&str>,
    bucket: f64,
    bins: u64,
) -> Result<Value> {
    let db = a.db();
    let w = clamp(window_secs, 60, 400 * 86_400);
    let sf = strategy_filter(strategy);
    let bucket = if bucket.is_finite() {
        bucket.clamp(0.0005, 0.05)
    } else {
        0.0025
    };
    let bins = clamp(bins, 4, 40);

    let rows = a
        .query(&format!(
            "SELECT
    strategy,
    toUInt32(least(floor(greatest(avg_edge, 0) / {bucket}), {last})) AS bin,
    count()                     AS n,
    round(sum(net_profit), 2)   AS net,
    round(avg(size), 1)         AS size_avg,
    countIf(profitable = 1)     AS profitable
FROM {db}.opportunities
WHERE event = 'open' AND ts >= now() - INTERVAL {w} SECOND{sf}
GROUP BY strategy, bin
ORDER BY strategy, bin",
            last = bins - 1
        ))
        .await?;

    Ok(json!({
        "window_secs": w,
        "bucket": bucket,
        "bins": bins,
        "rows": rows,
    }))
}

/// How long an edge stays visible. The histogram is the raw shape; the survival
/// curve (share still open at each bucket edge) is what says whether you can
/// technically compete at all.
pub async fn decay(a: &Analytics, window_secs: u64, strategy: Option<&str>) -> Result<Value> {
    let db = a.db();
    let w = clamp(window_secs, 60, 400 * 86_400);
    let sf = strategy_filter(strategy);
    let counts = bucket_counts("lived_ms", LIVED_BUCKETS_MS);

    let rows = a
        .query(&format!(
            "SELECT
    strategy,
    count()                         AS n,
    {counts},
    round(quantile(0.5)(lived_ms))  AS p50,
    round(quantile(0.9)(lived_ms))  AS p90,
    round(quantile(0.99)(lived_ms)) AS p99,
    round(avg(peak_avg_edge), 5)    AS edge_avg
FROM {db}.crossings
WHERE closed_at >= now() - INTERVAL {w} SECOND{sf}
GROUP BY strategy
ORDER BY strategy"
        ))
        .await?;

    Ok(json!({
        "window_secs": w,
        "buckets_ms": LIVED_BUCKETS_MS,
        "labels": lived_bucket_labels(),
        "rows": rows,
    }))
}

/// Opportunity count and average edge over time, on one shared time axis.
pub async fn timeline(
    a: &Analytics,
    window_secs: u64,
    bucket_secs: u64,
    strategy: Option<&str>,
) -> Result<Value> {
    let db = a.db();
    let w = clamp(window_secs, 60, 400 * 86_400);
    let b = clamp(bucket_secs, 60, 86_400);
    let sf = strategy_filter(strategy);

    let rows = a
        .query(&format!(
            "SELECT
    toUnixTimestamp(toStartOfInterval(ts, INTERVAL {b} SECOND)) AS t,
    strategy,
    count()                     AS n,
    uniq(condition_id)          AS markets,
    round(avg(avg_edge), 5)     AS edge_avg,
    round(max(avg_edge), 5)     AS edge_max,
    round(sum(net_profit), 2)   AS net
FROM {db}.opportunities
WHERE event = 'open' AND ts >= now() - INTERVAL {w} SECOND{sf}
GROUP BY t, strategy
ORDER BY t"
        ))
        .await?;

    Ok(json!({ "window_secs": w, "bucket_secs": b, "rows": rows }))
}

/// Which markets actually produce edges, and whether they are worth watching:
/// frequency, average edge, and how long their edges survive.
pub async fn markets(a: &Analytics, window_secs: u64, limit: u64) -> Result<Value> {
    let db = a.db();
    let w = clamp(window_secs, 60, 400 * 86_400);
    let n = clamp(limit, 1, 100);

    let rows = a
        .query(&format!(
            "SELECT
    o.condition_id      AS condition_id,
    o.slug              AS slug,
    o.question          AS question,
    o.category          AS category,
    o.liquidity_tier    AS liquidity_tier,
    o.market_volume     AS market_volume,
    o.occurrences       AS occurrences,
    o.profitable        AS profitable,
    o.edge_avg          AS edge_avg,
    o.edge_max          AS edge_max,
    o.net_total         AS net_total,
    o.size_avg          AS size_avg,
    c.lived_avg         AS lived_avg,
    c.lived_p50         AS lived_p50
FROM
(
    SELECT
        condition_id,
        any(slug)                   AS slug,
        any(question)               AS question,
        any(category)               AS category,
        any(liquidity_tier)         AS liquidity_tier,
        round(any(market_volume))   AS market_volume,
        count()                     AS occurrences,
        countIf(profitable = 1)     AS profitable,
        round(avg(avg_edge), 5)     AS edge_avg,
        round(max(avg_edge), 5)     AS edge_max,
        round(sum(net_profit), 2)   AS net_total,
        round(avg(size), 1)         AS size_avg
    FROM {db}.opportunities
    WHERE event = 'open' AND ts >= now() - INTERVAL {w} SECOND
    GROUP BY condition_id
) AS o
LEFT JOIN
(
    SELECT
        condition_id,
        round(avg(lived_ms))            AS lived_avg,
        round(quantile(0.5)(lived_ms))  AS lived_p50
    FROM {db}.crossings
    WHERE closed_at >= now() - INTERVAL {w} SECOND
    GROUP BY condition_id
) AS c USING (condition_id)
ORDER BY occurrences DESC, edge_avg DESC
LIMIT {n}"
        ))
        .await?;

    Ok(json!({ "window_secs": w, "rows": rows }))
}

/// Rollup by market category and liquidity tier — where the edges live.
pub async fn categories(a: &Analytics, window_secs: u64) -> Result<Value> {
    let db = a.db();
    let w = clamp(window_secs, 60, 400 * 86_400);

    let by_category = a
        .query(&format!(
            "SELECT
    category,
    strategy,
    count()                     AS occurrences,
    uniq(condition_id)          AS markets,
    countIf(profitable = 1)     AS profitable,
    round(avg(avg_edge), 5)     AS edge_avg,
    round(sum(net_profit), 2)   AS net_total,
    round(avg(fee_bps))         AS fee_bps
FROM {db}.opportunities
WHERE event = 'open' AND ts >= now() - INTERVAL {w} SECOND
GROUP BY category, strategy
ORDER BY occurrences DESC"
        ))
        .await?;

    let by_tier = a
        .query(&format!(
            "SELECT
    liquidity_tier,
    count()                     AS occurrences,
    uniq(condition_id)          AS markets,
    round(avg(avg_edge), 5)     AS edge_avg,
    round(avg(size), 1)         AS size_avg,
    round(sum(net_profit), 2)   AS net_total
FROM {db}.opportunities
WHERE event = 'open' AND ts >= now() - INTERVAL {w} SECOND
GROUP BY liquidity_tier
ORDER BY occurrences DESC"
        ))
        .await?;

    Ok(json!({ "window_secs": w, "by_category": by_category, "by_tier": by_tier }))
}

/// When edges appear: weekday × hour of day, at the caller's UTC offset.
pub async fn heatmap(a: &Analytics, window_secs: u64, tz_offset_minutes: i64) -> Result<Value> {
    let db = a.db();
    let w = clamp(window_secs, 3_600, 400 * 86_400);
    let off = tz_offset_minutes.clamp(-14 * 60, 14 * 60);

    let rows = a
        .query(&format!(
            "SELECT
    toDayOfWeek(local)          AS dow,
    toHour(local)               AS hour,
    count()                     AS n,
    round(avg(avg_edge), 5)     AS edge_avg,
    round(sum(net_profit), 2)   AS net
FROM
(
    SELECT ts + INTERVAL {off} MINUTE AS local, avg_edge, net_profit
    FROM {db}.opportunities
    WHERE event = 'open' AND ts >= now() - INTERVAL {w} SECOND
)
GROUP BY dow, hour
ORDER BY dow, hour"
        ))
        .await?;

    Ok(json!({ "window_secs": w, "tz_offset_minutes": off, "rows": rows }))
}

/// How the realised edge holds up as size grows. `depth_haircut` is
/// `gross_edge - avg_edge`: the part of the headline edge your own sweep down
/// the ladder gives back, before fees and gas.
pub async fn size_curve(a: &Analytics, window_secs: u64, strategy: Option<&str>) -> Result<Value> {
    let db = a.db();
    let w = clamp(window_secs, 60, 400 * 86_400);
    let sf = strategy_filter(strategy);
    let idx = bucket_index("size", SIZE_BUCKETS);

    let rows = a
        .query(&format!(
            "SELECT
    strategy,
    toUInt32({idx})                             AS bucket,
    count()                                     AS n,
    round(avg(size), 1)                         AS size_avg,
    round(avg(gross_edge), 5)                   AS gross_avg,
    round(avg(avg_edge), 5)                     AS edge_avg,
    round(avg(gross_edge - avg_edge), 5)        AS depth_haircut,
    round(avg(depth_cost_usd), 2)               AS depth_cost_usd,
    round(avg(net_profit), 2)                   AS net_avg,
    round(countIf(profitable = 1) / count(), 4) AS profitable_rate
FROM {db}.opportunities
WHERE event = 'open' AND ts >= now() - INTERVAL {w} SECOND{sf}
GROUP BY strategy, bucket
ORDER BY strategy, bucket"
        ))
        .await?;

    Ok(json!({
        "window_secs": w,
        "buckets": SIZE_BUCKETS,
        "labels": size_bucket_labels(),
        "rows": rows,
    }))
}

/// Baseline market state: how tight the books are and how close they run to par
/// even when no arb clears. Reads the hourly rollup, not the raw samples.
pub async fn liquidity(a: &Analytics, window_secs: u64, limit: u64) -> Result<Value> {
    let db = a.db();
    let w = clamp(window_secs, 3_600, 400 * 86_400);
    let n = clamp(limit, 1, 100);

    let series = a
        .query(&format!(
            "SELECT
    toUnixTimestamp(hour)                       AS t,
    sum(samples)                                AS samples,
    round(avgMerge(yes_spread_avg), 5)          AS yes_spread,
    round(avgMerge(no_spread_avg), 5)           AS no_spread,
    round(min(ask_sum_min), 5)                  AS ask_sum_min,
    round(max(bid_sum_max), 5)                  AS bid_sum_max,
    round(avgMerge(depth_avg), 2)               AS depth_avg,
    sum(updates)                                AS updates,
    round(sum(traded_usd), 2)                   AS traded_usd
FROM {db}.book_hourly
WHERE hour >= now() - INTERVAL {w} SECOND
GROUP BY t
ORDER BY t"
        ))
        .await?;

    let tightest = a
        .query(&format!(
            "SELECT
    condition_id,
    any(category)                       AS category,
    round(avgMerge(yes_spread_avg), 5)  AS yes_spread,
    round(min(ask_sum_min), 5)          AS ask_sum_min,
    round(max(bid_sum_max), 5)          AS bid_sum_max,
    round(avgMerge(depth_avg), 2)       AS depth_avg,
    sum(updates)                        AS updates
FROM {db}.book_hourly
WHERE hour >= now() - INTERVAL {w} SECOND
GROUP BY condition_id
ORDER BY ask_sum_min ASC
LIMIT {n}"
        ))
        .await?;

    Ok(json!({ "window_secs": w, "series": series, "tightest": tightest }))
}

/// Realised execution quality. Empty until an execution layer posts to
/// `/analytics/execution` — this project only ever detects.
pub async fn executions(a: &Analytics, window_secs: u64) -> Result<Value> {
    let db = a.db();
    let w = clamp(window_secs, 60, 400 * 86_400);

    let rows = a
        .query(&format!(
            "SELECT
    strategy,
    leg,
    status,
    count()                                     AS n,
    round(avg(signal_to_submit_ms))             AS submit_avg_ms,
    round(quantile(0.9)(signal_to_submit_ms))   AS submit_p90_ms,
    round(avg(submit_to_confirm_ms))            AS confirm_avg_ms,
    round(avg(slippage_price), 5)               AS slippage_price_avg,
    round(sum(slippage_usd), 2)                 AS slippage_usd_total,
    round(avg(fill_ratio), 4)                   AS fill_ratio_avg,
    round(sum(fee_usd), 2)                      AS fee_total,
    round(sum(gas_cost_usd), 2)                 AS gas_total,
    round(avg(gas_price_gwei), 2)               AS gas_price_avg,
    round(sum(net_pnl), 2)                      AS net_pnl,
    countIf(toxic_fill = 1)                     AS toxic_fills
FROM {db}.executions
WHERE ts >= now() - INTERVAL {w} SECOND
GROUP BY strategy, leg, status
ORDER BY n DESC"
        ))
        .await?;

    Ok(json!({ "window_secs": w, "rows": rows }))
}

/// Raw rows as CSV, for anyone who would rather use their own tooling.
pub async fn export(a: &Analytics, table: &str, window_secs: u64, limit: u64) -> Result<String> {
    let db = a.db();
    let table = EXPORTABLE
        .iter()
        .find(|t| t.eq_ignore_ascii_case(table))
        .ok_or_else(|| {
            anyhow::anyhow!("unknown table '{table}'; one of: {}", EXPORTABLE.join(", "))
        })?;
    let w = clamp(window_secs, 60, 400 * 86_400);
    let n = clamp(limit, 1, 1_000_000);
    let time_col = if *table == "crossings" {
        "closed_at"
    } else {
        "ts"
    };

    a.query_csv(&format!(
        "SELECT * FROM {db}.{table}
WHERE {time_col} >= now() - INTERVAL {w} SECOND
ORDER BY {time_col} DESC
LIMIT {n}"
    ))
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strategy_is_whitelisted_not_interpolated() {
        assert_eq!(strategy_filter(Some("mint")), " AND strategy = 'mint'");
        assert_eq!(strategy_filter(Some("SPLIT")), " AND strategy = 'split'");
        // Anything else widens to "both" instead of reaching the query.
        assert_eq!(strategy_filter(Some("'; DROP TABLE arb.trades --")), "");
        assert_eq!(strategy_filter(None), "");
    }

    #[test]
    fn bucket_counts_cover_every_value_exactly_once() {
        let sql = bucket_counts("lived_ms", &[50, 100]);
        assert!(sql.contains("countIf(lived_ms >= 0 AND lived_ms < 50) AS b0"));
        assert!(sql.contains("countIf(lived_ms >= 50 AND lived_ms < 100) AS b1"));
        assert!(sql.contains("countIf(lived_ms >= 100) AS b2"));
    }

    #[test]
    fn labels_line_up_one_per_bucket_column() {
        // The chart maps b0..bN onto these labels positionally — an off-by-one
        // here would silently mislabel the whole decay curve.
        assert_eq!(lived_bucket_labels().len(), LIVED_BUCKETS_MS.len() + 1);
        assert_eq!(size_bucket_labels().len(), SIZE_BUCKETS.len() + 1);
        assert_eq!(lived_bucket_labels()[0], "0ms–50ms");
        assert_eq!(lived_bucket_labels().last().unwrap(), "30s+");
        assert_eq!(size_bucket_labels().last().unwrap(), "1k+");
    }

    #[test]
    fn size_buckets_map_onto_their_index() {
        let sql = bucket_index("size", &[10.0, 50.0]);
        assert_eq!(sql, "multiIf(size < 10, 0, size < 50, 1, 2)");
    }

    #[test]
    fn windows_and_limits_are_clamped() {
        assert_eq!(clamp(0, 60, 100), 60);
        assert_eq!(clamp(u64::MAX, 60, 100), 100);
    }
}
