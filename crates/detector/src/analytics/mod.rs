//! The analytics sink: a fire-and-forget ClickHouse writer plus the read side
//! the `/analytics` endpoints are built on.
//!
//! The one rule this module obeys: **it must never slow down or break
//! detection.** Recording is a `try_send` onto a bounded queue — if ClickHouse
//! is down, slow, or simply not configured, rows are counted and dropped and the
//! websocket loop never notices. Analytics is a diary, not a dependency.

pub mod query;
pub mod rows;
mod schema;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::sync::mpsc;

pub use rows::{
    liquidity_tier, BookSampleRow, CrossingRow, ExecutionRow, OpportunityRow, Row, TradeRow,
};

#[derive(Debug, Clone)]
pub struct AnalyticsConfig {
    /// ClickHouse HTTP endpoint. Empty disables analytics entirely.
    pub url: String,
    pub database: String,
    pub user: String,
    pub password: String,
    /// Minimum gap between two `update` rows for the same live occurrence. A
    /// standing arb can be re-evaluated hundreds of times a second; without this
    /// one busy market would dominate the whole dataset.
    pub sample_ms: u64,
    /// Period of the whole-watchlist book probe. 0 disables it.
    pub book_sample_secs: u64,
    /// Rolling window behind `trades_5m` / `traded_5m_usd`.
    pub trade_window_secs: u64,
    /// Order-book levels captured per side on each opportunity row.
    pub depth_levels: usize,
    pub batch_rows: usize,
    pub flush_secs: u64,
    pub queue_cap: usize,
    pub ttl_days: u32,
}

impl Default for AnalyticsConfig {
    fn default() -> Self {
        Self {
            url: String::new(),
            database: "arb".into(),
            user: "default".into(),
            password: String::new(),
            sample_ms: 500,
            book_sample_secs: 300,
            trade_window_secs: 300,
            depth_levels: 5,
            batch_rows: 500,
            flush_secs: 2,
            queue_cap: 50_000,
            ttl_days: 90,
        }
    }
}

pub struct Analytics {
    cfg: AnalyticsConfig,
    http: reqwest::Client,
    tx: Option<mpsc::Sender<Row>>,
    ready: Arc<AtomicBool>,
    written: Arc<AtomicU64>,
    dropped: Arc<AtomicU64>,
    failed: Arc<AtomicU64>,
}

impl Analytics {
    /// Build the sink and, when a URL is configured, start the schema bootstrap
    /// and the batching flusher. Returns immediately either way — a ClickHouse
    /// that is still booting is retried in the background.
    pub fn start(cfg: AnalyticsConfig, http_timeout_secs: u64) -> Result<Arc<Self>> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(http_timeout_secs.max(5)))
            .build()
            .context("analytics http client")?;

        if cfg.url.is_empty() {
            tracing::info!("analytics disabled (no clickhouse_url configured)");
            return Ok(Arc::new(Self {
                cfg,
                http,
                tx: None,
                ready: Arc::new(AtomicBool::new(false)),
                written: Default::default(),
                dropped: Default::default(),
                failed: Default::default(),
            }));
        }

        let (tx, rx) = mpsc::channel(cfg.queue_cap.max(1_000));
        let this = Arc::new(Self {
            cfg,
            http,
            tx: Some(tx),
            ready: Arc::new(AtomicBool::new(false)),
            written: Default::default(),
            dropped: Default::default(),
            failed: Default::default(),
        });

        tokio::spawn(bootstrap(this.clone()));
        tokio::spawn(flush_loop(this.clone(), rx));
        tracing::info!(url = %this.cfg.url, db = %this.cfg.database, "analytics enabled");
        Ok(this)
    }

    pub fn enabled(&self) -> bool {
        self.tx.is_some()
    }

    /// True once the schema has been applied — i.e. reads and writes will land.
    pub fn ready(&self) -> bool {
        self.ready.load(Ordering::Relaxed)
    }

    pub fn config(&self) -> &AnalyticsConfig {
        &self.cfg
    }

    /// Enqueue a row. Never blocks and never fails loudly: a full queue (or a
    /// disabled sink) increments a counter and moves on.
    pub fn record(&self, row: Row) {
        let Some(tx) = &self.tx else { return };
        if tx.try_send(row).is_err() {
            let n = self.dropped.fetch_add(1, Ordering::Relaxed) + 1;
            if n % 1_000 == 1 {
                tracing::warn!("analytics queue full; {n} rows dropped so far");
            }
        }
    }

    pub fn stats(&self) -> serde_json::Value {
        serde_json::json!({
            "enabled": self.enabled(),
            "ready": self.ready(),
            "rows_written": self.written.load(Ordering::Relaxed),
            "rows_dropped": self.dropped.load(Ordering::Relaxed),
            "flush_failures": self.failed.load(Ordering::Relaxed),
            "database": self.cfg.database,
            "sample_ms": self.cfg.sample_ms,
            "book_sample_secs": self.cfg.book_sample_secs,
            "ttl_days": self.cfg.ttl_days,
        })
    }

    /// Run a statement that returns nothing (DDL).
    async fn execute(&self, sql: &str) -> Result<()> {
        let resp = self
            .request()
            .body(sql.to_string())
            .send()
            .await
            .context("clickhouse unreachable")?;
        self.check(resp).await.map(|_| ())
    }

    /// Run a `SELECT` and return the rows of its `FORMAT JSON` envelope.
    pub async fn query(&self, sql: &str) -> Result<Vec<serde_json::Value>> {
        anyhow::ensure!(self.enabled(), "analytics is not configured");
        let resp = self
            .request()
            .body(format!("{sql}\nFORMAT JSON"))
            .send()
            .await
            .context("clickhouse unreachable")?;
        let body = self.check(resp).await?;
        let v: serde_json::Value = serde_json::from_str(&body).context("clickhouse decode")?;
        Ok(v.get("data")
            .and_then(|d| d.as_array())
            .cloned()
            .unwrap_or_default())
    }

    /// Run a `SELECT` and return it as CSV with a header row.
    pub async fn query_csv(&self, sql: &str) -> Result<String> {
        anyhow::ensure!(self.enabled(), "analytics is not configured");
        let resp = self
            .request()
            .body(format!("{sql}\nFORMAT CSVWithNames"))
            .send()
            .await
            .context("clickhouse unreachable")?;
        self.check(resp).await
    }

    fn request(&self) -> reqwest::RequestBuilder {
        self.http
            .post(&self.cfg.url)
            .header("X-ClickHouse-User", &self.cfg.user)
            .header("X-ClickHouse-Key", &self.cfg.password)
            .header("X-ClickHouse-Format", "JSON")
    }

    /// ClickHouse reports errors in the body, not only the status line, so read
    /// it either way and surface the first line — the rest is a stack trace.
    async fn check(&self, resp: reqwest::Response) -> Result<String> {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if status.is_success() {
            return Ok(body);
        }
        let detail = body.lines().next().unwrap_or("").trim();
        anyhow::bail!("clickhouse {status}: {detail}")
    }

    /// Insert pre-serialised `JSONEachRow` lines into one table.
    async fn insert(&self, table: &str, lines: &str) -> Result<()> {
        let sql = format!(
            "INSERT INTO {}.{table} FORMAT JSONEachRow",
            self.cfg.database
        );
        let url = format!("{}?query={}", self.cfg.url, urlencode(&sql));
        let resp = self
            .http
            .post(&url)
            .header("X-ClickHouse-User", &self.cfg.user)
            .header("X-ClickHouse-Key", &self.cfg.password)
            .body(lines.to_string())
            .send()
            .await
            .context("clickhouse unreachable")?;
        self.check(resp).await.map(|_| ())
    }

    pub fn db(&self) -> &str {
        &self.cfg.database
    }
}

/// Apply the schema, retrying until ClickHouse answers — on a fresh
/// `docker compose up` the server is usually a few seconds behind us.
async fn bootstrap(a: Arc<Analytics>) {
    let mut backoff = 1u64;
    loop {
        match apply_schema(&a).await {
            Ok(()) => {
                a.ready.store(true, Ordering::Relaxed);
                tracing::info!(db = %a.cfg.database, "analytics schema ready");
                return;
            }
            Err(e) => {
                // `{:#}` so the ClickHouse error itself shows, not just the
                // statement we wrapped it in.
                tracing::warn!("analytics schema not applied: {e:#}; retrying in {backoff}s");
                tokio::time::sleep(Duration::from_secs(backoff)).await;
                backoff = (backoff * 2).min(60);
            }
        }
    }
}

async fn apply_schema(a: &Analytics) -> Result<()> {
    for stmt in schema::STATEMENTS {
        let sql = stmt
            .replace("{db}", &a.cfg.database)
            .replace("{ttl}", &a.cfg.ttl_days.to_string());
        a.execute(&sql)
            .await
            .with_context(|| format!("statement: {}", first_line(&sql)))?;
    }
    Ok(())
}

fn first_line(sql: &str) -> String {
    sql.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .to_string()
}

/// Drain the queue into per-table batches, flushing on size or on a timer.
/// A failed flush drops its batch: retrying would grow unbounded behind a
/// ClickHouse that is down, and stale analytics is worth less than a live feed.
async fn flush_loop(a: Arc<Analytics>, mut rx: mpsc::Receiver<Row>) {
    use std::collections::HashMap;

    let mut pending: HashMap<&'static str, String> = HashMap::new();
    let mut count = 0usize;
    let mut ticker = tokio::time::interval(Duration::from_secs(a.cfg.flush_secs.max(1)));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            row = rx.recv() => {
                let Some(row) = row else { return };
                match row.to_json() {
                    Ok(json) => {
                        let buf = pending.entry(row.table()).or_default();
                        buf.push_str(&json);
                        buf.push('\n');
                        count += 1;
                    }
                    Err(e) => tracing::warn!("analytics row not serializable: {e}"),
                }
                // Also flushed when not ready, so the pre-schema buffer gets its
                // ceiling checked rather than growing until the next tick.
                if count >= a.cfg.batch_rows {
                    flush(&a, &mut pending, &mut count).await;
                }
            }
            _ = ticker.tick() => {
                if count > 0 {
                    flush(&a, &mut pending, &mut count).await;
                }
            }
        }
    }
}

async fn flush(
    a: &Analytics,
    pending: &mut std::collections::HashMap<&'static str, String>,
    count: &mut usize,
) {
    // Writing before the schema exists would fail every batch, so rows are held
    // until bootstrap lands. That buffer needs its own ceiling: the channel's
    // bound only covers rows still in transit, so a ClickHouse that never comes
    // up would otherwise grow this map without limit.
    if !a.ready() {
        if *count >= a.cfg.queue_cap {
            let lost = *count as u64;
            pending.clear();
            *count = 0;
            a.dropped.fetch_add(lost, Ordering::Relaxed);
            tracing::warn!("clickhouse still unavailable; dropped {lost} buffered rows");
        }
        return;
    }
    let batches: Vec<(&'static str, String)> = pending.drain().collect();
    *count = 0;
    let mut ok = 0u64;
    for (table, lines) in batches {
        let n = lines.lines().count() as u64;
        match a.insert(table, &lines).await {
            Ok(()) => ok += n,
            Err(e) => {
                a.failed.fetch_add(1, Ordering::Relaxed);
                a.dropped.fetch_add(n, Ordering::Relaxed);
                tracing::warn!("analytics insert into {table} failed ({n} rows lost): {e}");
            }
        }
    }
    a.written.fetch_add(ok, Ordering::Relaxed);
}

pub fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_substitutes_every_placeholder() {
        for stmt in schema::STATEMENTS {
            let sql = stmt.replace("{db}", "arb").replace("{ttl}", "90");
            assert!(!sql.contains("{db}"), "unsubstituted db in: {stmt}");
            assert!(!sql.contains("{ttl}"), "unsubstituted ttl in: {stmt}");
            assert!(
                sql.contains("IF NOT EXISTS"),
                "schema must be re-runnable: {}",
                first_line(&sql)
            );
        }
    }

    #[test]
    fn a_disabled_sink_swallows_rows_without_a_channel() {
        let a = Analytics {
            cfg: AnalyticsConfig::default(),
            http: reqwest::Client::new(),
            tx: None,
            ready: Arc::new(AtomicBool::new(false)),
            written: Default::default(),
            dropped: Default::default(),
            failed: Default::default(),
        };
        assert!(!a.enabled());
        a.record(Row::Trade(Box::new(TradeRow {
            ts: String::new(),
            condition_id: String::new(),
            token_id: String::new(),
            outcome: "yes",
            side: String::new(),
            price: 0.0,
            size: 0.0,
            notional: 0.0,
        })));
        // Nothing queued, nothing counted as dropped — there is no sink at all.
        assert_eq!(a.dropped.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn queries_are_url_escaped_for_the_http_interface() {
        assert_eq!(urlencode("INSERT INTO a.b"), "INSERT%20INTO%20a.b");
        assert_eq!(urlencode("a=1&b=2"), "a%3D1%26b%3D2");
    }
}
