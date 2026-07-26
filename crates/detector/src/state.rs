//! Shared runtime state: the user-curated watchlist, live order books fed by
//! the websocket, and the derived opportunities.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use arb_core::{evaluate, evaluate_split, ArbResult, Level, Strategy};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, Notify, RwLock};

use crate::analytics::{
    liquidity_tier, Analytics, BookSampleRow, CrossingRow, OpportunityRow, Row, TradeRow,
};
use crate::config::Config;

/// How many closed micro-crossings to retain for `/crossings`.
const CROSSING_LOG_CAP: usize = 500;

/// A coherent set arb currently open on a market — buy-set (`ask_YES + ask_NO
/// < 1`) or sell-set (`bid_YES + bid_NO > 1`) — a "micro-crossing" of native
/// liquidity through the mirror. Tracked so we can measure how long it stays
/// visible.
struct OpenCrossing {
    /// Stable id for this occurrence, shared by every analytics row it produces
    /// and by any execution later logged against it.
    opportunity_id: String,
    opened: Instant,
    opened_at: u64,
    opened_at_ms: u64,
    question: String,
    market_url: String,
    strategy: Strategy,
    peak_avg_edge: f64,
    peak_gross_edge: f64,
    peak_net: f64,
    peak_size: f64,
    peak_profitable: bool,
    first_avg_edge: f64,
    last_avg_edge: f64,
    /// Book updates this occurrence survived, and when it was at its best.
    observations: u32,
    time_to_peak_ms: u32,
    /// When the last analytics `update` row went out, so a busy market can't
    /// flood the event table (see `analytics.sample_ms`).
    last_sampled: Instant,
}

/// A micro-crossing after it disappeared, with the lifetime it was visible.
#[derive(Debug, Clone, Serialize)]
pub struct ClosedCrossing {
    pub seq: u64,
    pub condition_id: String,
    pub question: String,
    pub market_url: String,
    pub strategy: Strategy,
    pub lived_ms: u64,
    pub peak_avg_edge: f64,
    pub peak_net: f64,
    pub peak_size: f64,
    pub opened_at: u64,
    pub closed_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Market {
    pub question: String,
    pub slug: String,
    pub condition_id: String,
    pub yes_token: String,
    pub no_token: String,
    pub volume: f64,
    /// Trading window as reported by Gamma, ISO 8601. `start_date` is when the
    /// market opened, `end_date` when it closes/resolves.
    #[serde(default)]
    pub start_date: Option<String>,
    #[serde(default)]
    pub end_date: Option<String>,
    /// Taker fee (bps per side) for this market's category, assigned at discovery.
    /// `None` falls back to the global `fee_bps`.
    #[serde(default)]
    pub fee_bps: Option<f64>,
    /// Slug of the watched category this market came from, assigned at
    /// discovery. Carried into analytics so edges can be sliced by category;
    /// pinned markets have none.
    #[serde(default)]
    pub category: Option<String>,
}

impl Market {
    pub fn market_url(&self) -> String {
        format!("https://polymarket.com/market/{}", self.slug)
    }

    /// Category slug for analytics; uncategorised markets group under `manual`
    /// rather than an empty string, which reads badly on a chart axis.
    pub fn category_label(&self) -> String {
        self.category.clone().unwrap_or_else(|| "manual".into())
    }

    /// Seconds until the market closes, or 0 when Gamma gave no end date. This
    /// is the opportunity cost of capital parked in a set you hold to
    /// resolution instead of merging.
    pub fn secs_to_resolution(&self) -> i64 {
        self.end_date
            .as_deref()
            .and_then(arb_core::time::parse_iso8601)
            .map_or(0, |end| end as i64 - now() as i64)
    }
}

/// A watched Polymarket category (tag). Every active market carrying this tag is
/// pulled in and kept in sync by the discovery loop.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Category {
    pub slug: String,
    pub label: String,
    pub tag_id: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Persisted {
    #[serde(default)]
    categories: Vec<Category>,
    #[serde(default)]
    manual: Vec<Market>,
}

/// A single side's price levels keyed by the exact price string sent by the
/// exchange. A `price_change` sets the aggregate size at a level; size 0 clears.
#[derive(Debug, Default)]
pub struct LiveBook {
    bids: HashMap<String, f64>,
    asks: HashMap<String, f64>,
    /// Unix millis of the last change, and a monotonic change counter. Both are
    /// analytics only: the first gives every signal a book age, the second an
    /// update rate — the closest thing to "how busy is this market" that the
    /// book channel can tell us.
    updated_ms: u64,
    updates: u64,
}

impl LiveBook {
    pub fn replace(&mut self, bids: Vec<(String, f64)>, asks: Vec<(String, f64)>) {
        self.bids = bids.into_iter().filter(|(_, s)| *s > 0.0).collect();
        self.asks = asks.into_iter().filter(|(_, s)| *s > 0.0).collect();
        self.touch();
    }

    pub fn set(&mut self, is_bid: bool, price: String, size: f64) {
        let side = if is_bid {
            &mut self.bids
        } else {
            &mut self.asks
        };
        if size > 0.0 {
            side.insert(price, size);
        } else {
            side.remove(&price);
        }
        self.touch();
    }

    fn touch(&mut self) {
        self.updated_ms = now_ms();
        self.updates = self.updates.wrapping_add(1);
    }

    pub fn updated_ms(&self) -> u64 {
        self.updated_ms
    }

    pub fn updates(&self) -> u64 {
        self.updates
    }

    fn levels(side: &HashMap<String, f64>) -> Vec<Level> {
        side.iter()
            .filter_map(|(p, s)| p.parse::<f64>().ok().map(|p| Level::new(p, *s)))
            .collect()
    }

    pub fn asks(&self) -> Vec<Level> {
        Self::levels(&self.asks)
    }

    pub fn bids(&self) -> Vec<Level> {
        Self::levels(&self.bids)
    }

    pub fn best_bid(&self) -> Option<f64> {
        self.bids
            .keys()
            .filter_map(|p| p.parse::<f64>().ok())
            .reduce(f64::max)
    }

    pub fn best_ask(&self) -> Option<f64> {
        self.asks
            .keys()
            .filter_map(|p| p.parse::<f64>().ok())
            .reduce(f64::min)
    }
}

/// A same-instant view of both legs used to sanity-check the data before
/// treating a computed edge as real, and — once it passes — to describe the
/// market state that produced the signal.
struct BookSnap {
    yes_asks: Vec<Level>,
    no_asks: Vec<Level>,
    yes_bids: Vec<Level>,
    no_bids: Vec<Level>,
    yes_bid: Option<f64>,
    yes_ask: Option<f64>,
    no_bid: Option<f64>,
    no_ask: Option<f64>,
    /// Unix millis each leg last changed; the older of the two is the snapshot's
    /// age. A stale leg is exactly how a phantom arb appears.
    yes_updated_ms: u64,
    no_updated_ms: u64,
}

/// One side of one book, cut to the top `n` levels in trading order (asks
/// ascending, bids descending) and split into the parallel price/size arrays
/// ClickHouse stores.
struct Ladder {
    prices: Vec<f64>,
    sizes: Vec<f64>,
    /// Notional across the captured levels — the money actually resting there.
    depth_usd: f64,
    /// Levels in the *whole* book, not just the captured ones.
    levels: u16,
}

impl Ladder {
    fn top(levels: &[Level], n: usize, descending: bool) -> Self {
        let mut sorted: Vec<Level> = levels.iter().copied().filter(|l| l.size > 0.0).collect();
        sorted.sort_by(|a, b| {
            if descending {
                b.price.partial_cmp(&a.price).unwrap()
            } else {
                a.price.partial_cmp(&b.price).unwrap()
            }
        });
        let total = sorted.len() as u16;
        sorted.truncate(n);
        Self {
            depth_usd: sorted.iter().map(|l| l.price * l.size).sum(),
            prices: sorted.iter().map(|l| l.price).collect(),
            sizes: sorted.iter().map(|l| l.size).collect(),
            levels: total,
        }
    }
}

impl BookSnap {
    /// The four ladders as stored: YES asks, YES bids, NO asks, NO bids.
    fn ladders(&self, n: usize) -> [Ladder; 4] {
        [
            Ladder::top(&self.yes_asks, n, false),
            Ladder::top(&self.yes_bids, n, true),
            Ladder::top(&self.no_asks, n, false),
            Ladder::top(&self.no_bids, n, true),
        ]
    }

    fn spread(bid: Option<f64>, ask: Option<f64>) -> f64 {
        match (bid, ask) {
            (Some(b), Some(a)) => a - b,
            _ => 0.0,
        }
    }

    /// Milliseconds since the *older* leg last changed.
    fn age_ms(&self) -> u32 {
        let oldest = self.yes_updated_ms.min(self.no_updated_ms);
        if oldest == 0 {
            return 0;
        }
        now_ms().saturating_sub(oldest).min(u32::MAX as u64) as u32
    }
}

impl BookSnap {
    /// Reject snapshots that cannot exist on a live exchange, which on Polymarket
    /// (where the YES and NO books are mirror images) means the two legs came
    /// from different moments — the usual source of phantom arbs.
    fn is_coherent(&self) -> bool {
        let (Some(yb), Some(ya), Some(nb), Some(na)) =
            (self.yes_bid, self.yes_ask, self.no_bid, self.no_ask)
        else {
            return false;
        };
        // Neither leg's own book may be crossed or locked.
        if yb >= ya || nb >= na {
            return false;
        }
        // A coherent instant cannot offer both a buy-set arb (asks sum < 1) and a
        // sell-set arb (bids sum > 1) at once; both positive = desynced legs.
        let buy_edge = 1.0 - (ya + na);
        let sell_edge = (yb + nb) - 1.0;
        !(buy_edge > 0.0 && sell_edge > 0.0)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Opportunity {
    pub question: String,
    pub slug: String,
    pub condition_id: String,
    pub yes_token: String,
    pub no_token: String,
    pub volume: f64,
    pub market_url: String,
    pub start_date: Option<String>,
    pub end_date: Option<String>,
    pub arb: ArbResult,
    /// When this occurrence of the opportunity first appeared. Preserved across
    /// recomputes and reset only when it disappears and later comes back.
    pub first_seen: u64,
    pub updated_at: u64,
}

/// A trade print seen on the market websocket, kept only long enough to answer
/// "how much traded on this market in the last few minutes".
#[derive(Debug, Clone, Copy)]
struct TradePrint {
    at: Instant,
    notional: f64,
}

/// **Lock order: `watch` → `token_index` → `books` → `opps` / `split_opps`.**
///
/// A task that needs two of these at once takes them in that order. A task that
/// needs them the other way round releases the first guard before taking the
/// second — see `record_trade`. This is not a stylistic rule: `record_trade`
/// once held `token_index` while reaching for `watch` at the same time as
/// `add_market` held `watch` while reaching for `token_index`, and one
/// discovery pass racing one trade print was enough to wedge every task in the
/// process, `/health` included.
///
/// The remaining locks (`categories`, `manual`, `crossings_*`, `trades`) are
/// leaves: nothing is held across their acquisition, so they stay out of the
/// order.
#[derive(Clone)]
pub struct AppState {
    pub cfg: Arc<Config>,
    pub analytics: Arc<Analytics>,
    pub watch: Arc<RwLock<HashMap<String, Market>>>,
    pub categories: Arc<RwLock<HashMap<String, Category>>>,
    pub manual: Arc<RwLock<HashMap<String, Market>>>,
    pub token_index: Arc<RwLock<HashMap<String, String>>>,
    pub books: Arc<RwLock<HashMap<String, LiveBook>>>,
    /// Live buy-set opportunities (`Strategy::Mint`), keyed by condition id.
    pub opps: Arc<RwLock<HashMap<String, Opportunity>>>,
    /// Live split-and-sell opportunities (`Strategy::Split`), keyed by condition id.
    pub split_opps: Arc<RwLock<HashMap<String, Opportunity>>>,
    crossings_open: Arc<RwLock<HashMap<String, OpenCrossing>>>,
    crossings_log: Arc<RwLock<VecDeque<ClosedCrossing>>>,
    crossing_seq: Arc<AtomicU64>,
    /// When the last crossing on each `condition:strategy` closed, so a reopen
    /// can be timed. A short gap is the signature of a market being worked.
    crossing_closed_at: Arc<RwLock<HashMap<String, Instant>>>,
    /// Recent trade prints per token, pruned to `analytics.trade_window_secs`.
    trades: Arc<RwLock<HashMap<String, VecDeque<TradePrint>>>>,
    pub resubscribe: Arc<Notify>,
    pub ws_connected: Arc<AtomicBool>,
    pub last_update: Arc<AtomicU64>,
    persist_lock: Arc<Mutex<()>>,
}

impl AppState {
    pub fn new(cfg: Arc<Config>, analytics: Arc<Analytics>) -> Self {
        Self {
            cfg,
            analytics,
            watch: Default::default(),
            categories: Default::default(),
            manual: Default::default(),
            token_index: Default::default(),
            books: Default::default(),
            opps: Default::default(),
            split_opps: Default::default(),
            crossings_open: Default::default(),
            crossings_log: Default::default(),
            crossing_seq: Arc::new(AtomicU64::new(0)),
            crossing_closed_at: Default::default(),
            trades: Default::default(),
            resubscribe: Arc::new(Notify::new()),
            ws_connected: Arc::new(AtomicBool::new(false)),
            last_update: Arc::new(AtomicU64::new(0)),
            persist_lock: Arc::new(Mutex::new(())),
        }
    }

    /// Signal the websocket task to reconnect with the current token set.
    pub fn wake_resubscribe(&self) {
        self.resubscribe.notify_one();
    }

    pub async fn watched_tokens(&self) -> Vec<String> {
        let watch = self.watch.read().await;
        let mut out = Vec::with_capacity(watch.len() * 2);
        for m in watch.values() {
            out.push(m.yes_token.clone());
            out.push(m.no_token.clone());
        }
        out
    }

    /// Add a market and its token routing, seeding empty books. Returns false if
    /// it was already watched, in which case its mutable metadata (volume, dates,
    /// question, fee) is refreshed from `m` — token ids and books are immutable
    /// for a condition and are left untouched, so a discovery pass keeps the
    /// watchlist current without disturbing the live stream.
    pub async fn add_market(&self, m: Market) -> bool {
        let mut watch = self.watch.write().await;
        if let Some(existing) = watch.get_mut(&m.condition_id) {
            existing.question = m.question;
            existing.slug = m.slug;
            existing.volume = m.volume;
            existing.start_date = m.start_date;
            existing.end_date = m.end_date;
            existing.fee_bps = m.fee_bps;
            existing.category = m.category;
            return false;
        }
        let mut idx = self.token_index.write().await;
        idx.insert(m.yes_token.clone(), m.condition_id.clone());
        idx.insert(m.no_token.clone(), m.condition_id.clone());
        let mut books = self.books.write().await;
        books.entry(m.yes_token.clone()).or_default();
        books.entry(m.no_token.clone()).or_default();
        watch.insert(m.condition_id.clone(), m);
        true
    }

    /// Remove a market by condition id or slug. Returns the removed market.
    pub async fn remove_market(&self, key: &str) -> Option<Market> {
        let mut watch = self.watch.write().await;
        let cond = watch
            .values()
            .find(|m| m.condition_id == key || m.slug == key)
            .map(|m| m.condition_id.clone())?;
        let m = watch.remove(&cond)?;
        let mut idx = self.token_index.write().await;
        idx.remove(&m.yes_token);
        idx.remove(&m.no_token);
        let mut books = self.books.write().await;
        books.remove(&m.yes_token);
        books.remove(&m.no_token);
        self.opps.write().await.remove(&cond);
        self.split_opps.write().await.remove(&cond);
        Some(m)
    }

    pub async fn list_markets(&self) -> Vec<Market> {
        self.watch.read().await.values().cloned().collect()
    }

    /// Reconcile the watchlist toward `markets`: add missing ones, and (only when
    /// `remove_stale`) drop those no longer present. `remove_stale` is passed
    /// false when a category fetch failed, to avoid dropping markets we simply
    /// could not refresh this cycle. Returns true if anything changed.
    pub async fn sync_watchlist(&self, markets: Vec<Market>, remove_stale: bool) -> bool {
        let desired: HashSet<String> = markets.iter().map(|m| m.condition_id.clone()).collect();
        let mut changed = false;

        for m in markets {
            if self.add_market(m).await {
                changed = true;
            }
        }

        if remove_stale {
            let stale: Vec<String> = {
                let watch = self.watch.read().await;
                watch
                    .keys()
                    .filter(|c| !desired.contains(*c))
                    .cloned()
                    .collect()
            };
            for cond in stale {
                if self.remove_market(&cond).await.is_some() {
                    changed = true;
                }
            }
        }
        changed
    }

    pub async fn add_category(&self, c: Category) -> bool {
        let mut cats = self.categories.write().await;
        if cats.contains_key(&c.slug) {
            return false;
        }
        cats.insert(c.slug.clone(), c);
        true
    }

    pub async fn remove_category(&self, slug: &str) -> Option<Category> {
        self.categories.write().await.remove(slug)
    }

    pub async fn list_categories(&self) -> Vec<Category> {
        self.categories.read().await.values().cloned().collect()
    }

    /// Add a market pinned by the user (via `/add`). Kept in `manual` so the
    /// discovery loop always includes it in the desired set.
    pub async fn add_manual_market(&self, m: Market) -> bool {
        self.manual
            .write()
            .await
            .insert(m.condition_id.clone(), m.clone());
        self.add_market(m).await
    }

    /// Remove a pinned market by condition id or slug (also drops it from the
    /// live watch until the next discovery pass, if it belongs to a category).
    pub async fn remove_manual(&self, key: &str) -> Option<Market> {
        let cond = {
            let man = self.manual.read().await;
            man.values()
                .find(|m| m.condition_id == key || m.slug == key)
                .map(|m| m.condition_id.clone())
        };
        if let Some(cond) = cond {
            self.manual.write().await.remove(&cond);
            return self.remove_market(&cond).await;
        }
        self.remove_market(key).await
    }

    pub async fn manual_markets(&self) -> Vec<Market> {
        self.manual.read().await.values().cloned().collect()
    }

    /// Re-run both arbitrage directions for one market from its current live
    /// books and store or clear the resulting opportunities.
    ///
    /// `received` is when the websocket frame behind this recompute arrived; the
    /// gap to the end of this function is the detector's own share of the
    /// signal-to-action latency, and is recorded per signal.
    pub async fn recompute(&self, condition_id: &str, received: Instant) {
        let market = {
            let watch = self.watch.read().await;
            match watch.get(condition_id) {
                Some(m) => m.clone(),
                None => return,
            }
        };

        let snap = {
            let books = self.books.read().await;
            let yes = books.get(&market.yes_token);
            let no = books.get(&market.no_token);
            BookSnap {
                yes_asks: yes.map(LiveBook::asks).unwrap_or_default(),
                no_asks: no.map(LiveBook::asks).unwrap_or_default(),
                yes_bids: yes.map(LiveBook::bids).unwrap_or_default(),
                no_bids: no.map(LiveBook::bids).unwrap_or_default(),
                yes_bid: yes.and_then(LiveBook::best_bid),
                yes_ask: yes.and_then(LiveBook::best_ask),
                no_bid: no.and_then(LiveBook::best_bid),
                no_ask: no.and_then(LiveBook::best_ask),
                yes_updated_ms: yes.map(LiveBook::updated_ms).unwrap_or(0),
                no_updated_ms: no.map(LiveBook::updated_ms).unwrap_or(0),
            }
        };

        self.last_update.store(now(), Ordering::Relaxed);

        let params = arb_core::ArbParams {
            fee_bps: market.fee_bps.unwrap_or(self.cfg.arb.fee_bps),
            ..self.cfg.arb
        };
        let (mint, split) = if snap.is_coherent() {
            (
                evaluate(&snap.yes_asks, &snap.no_asks, &params),
                evaluate_split(&snap.yes_bids, &snap.no_bids, &params),
            )
        } else {
            (None, None)
        };

        self.store_opp(&market, &self.opps, mint.as_ref()).await;
        self.store_opp(&market, &self.split_opps, split.as_ref())
            .await;

        // Describing the market state costs a ladder sort and a lock, so it is
        // only done when a row will actually come of it: an arb is live, or one
        // was live and has just gone. Every other recompute — the overwhelming
        // majority — skips this entirely and analytics stays off the hot path.
        let ctx = if self.analytics.enabled()
            && (mint.is_some() || split.is_some() || self.has_open_crossing(condition_id).await)
        {
            let (trades, traded_usd) = self.trade_activity(&market).await;
            Some(SignalContext::build(
                &market, &snap, &params, received, trades, traded_usd,
            ))
        } else {
            None
        };

        self.note_crossing(&market, Strategy::Mint, mint.as_ref(), ctx.as_ref())
            .await;
        self.note_crossing(&market, Strategy::Split, split.as_ref(), ctx.as_ref())
            .await;
    }

    async fn has_open_crossing(&self, condition_id: &str) -> bool {
        let open = self.crossings_open.read().await;
        [Strategy::Mint, Strategy::Split]
            .iter()
            .any(|s| open.contains_key(&crossing_key(condition_id, *s)))
    }

    /// Record a trade print seen on the market channel, and keep the rolling
    /// per-token window that feeds `traded_5m_usd`.
    pub async fn record_trade(&self, token_id: &str, price: f64, size: f64, side: &str) {
        let notional = price * size;
        let window = Duration::from_secs(self.analytics.config().trade_window_secs.max(1));
        {
            let mut trades = self.trades.write().await;
            let q = trades.entry(token_id.to_string()).or_default();
            q.push_back(TradePrint {
                at: Instant::now(),
                notional,
            });
            prune(q, window);
        }

        if !self.analytics.enabled() {
            return;
        }
        // Two lookups, two separate guards, `watch` last: `add_market` and
        // `remove_market` hold `watch` while they reach for `token_index`, so
        // holding `token_index` across the `watch` acquisition here would close
        // a deadlock cycle. See the lock order on `AppState`.
        let condition_id = {
            let idx = self.token_index.read().await;
            match idx.get(token_id) {
                Some(cond) => cond.clone(),
                None => return,
            }
        };
        let outcome = {
            let watch = self.watch.read().await;
            match watch.get(&condition_id) {
                Some(m) if m.yes_token == token_id => "yes",
                Some(_) => "no",
                None => return,
            }
        };
        self.analytics.record(Row::Trade(Box::new(TradeRow {
            ts: arb_core::time::clickhouse_datetime(now_ms()),
            condition_id,
            token_id: token_id.to_string(),
            outcome,
            side: side.to_string(),
            price,
            size,
            notional,
        })));
    }

    /// Trade prints and notional across both legs inside the rolling window.
    async fn trade_activity(&self, market: &Market) -> (u32, f64) {
        let window = Duration::from_secs(self.analytics.config().trade_window_secs.max(1));
        let mut trades = self.trades.write().await;
        let mut count = 0u32;
        let mut usd = 0.0;
        for token in [&market.yes_token, &market.no_token] {
            if let Some(q) = trades.get_mut(token) {
                prune(q, window);
                count += q.len() as u32;
                usd += q.iter().map(|t| t.notional).sum::<f64>();
            }
        }
        (count, usd)
    }

    /// Probe every watched market's book, arb or not. This is the baseline the
    /// opportunity rows are read against — without it the dataset only contains
    /// the moments something happened, and a quiet market is indistinguishable
    /// from a market that never had a chance.
    pub async fn sample_books(&self, prev_updates: &mut HashMap<String, u64>) {
        if !self.analytics.enabled() {
            return;
        }
        let depth = self.analytics.config().depth_levels;
        let markets = self.list_markets().await;
        let ts = arb_core::time::clickhouse_datetime(now_ms());

        for market in markets {
            let (snap, updates) = {
                let books = self.books.read().await;
                let yes = books.get(&market.yes_token);
                let no = books.get(&market.no_token);
                let updates = yes.map(LiveBook::updates).unwrap_or(0)
                    + no.map(LiveBook::updates).unwrap_or(0);
                let snap = BookSnap {
                    yes_asks: yes.map(LiveBook::asks).unwrap_or_default(),
                    no_asks: no.map(LiveBook::asks).unwrap_or_default(),
                    yes_bids: yes.map(LiveBook::bids).unwrap_or_default(),
                    no_bids: no.map(LiveBook::bids).unwrap_or_default(),
                    yes_bid: yes.and_then(LiveBook::best_bid),
                    yes_ask: yes.and_then(LiveBook::best_ask),
                    no_bid: no.and_then(LiveBook::best_bid),
                    no_ask: no.and_then(LiveBook::best_ask),
                    yes_updated_ms: yes.map(LiveBook::updated_ms).unwrap_or(0),
                    no_updated_ms: no.map(LiveBook::updated_ms).unwrap_or(0),
                };
                (snap, updates)
            };

            // A market whose books have never ticked has nothing to say.
            if snap.yes_updated_ms == 0 && snap.no_updated_ms == 0 {
                continue;
            }
            let since = updates.saturating_sub(
                prev_updates
                    .insert(market.condition_id.clone(), updates)
                    .unwrap_or(0),
            );
            let [ya, yb, na, nb] = snap.ladders(depth);
            let (trades_5m, traded_5m_usd) = self.trade_activity(&market).await;

            self.analytics
                .record(Row::BookSample(Box::new(BookSampleRow {
                    ts: ts.clone(),
                    condition_id: market.condition_id.clone(),
                    slug: market.slug.clone(),
                    category: market.category_label(),
                    market_volume: market.volume,
                    liquidity_tier: liquidity_tier(market.volume),
                    yes_bid: snap.yes_bid.unwrap_or(0.0),
                    yes_ask: snap.yes_ask.unwrap_or(0.0),
                    no_bid: snap.no_bid.unwrap_or(0.0),
                    no_ask: snap.no_ask.unwrap_or(0.0),
                    yes_spread: BookSnap::spread(snap.yes_bid, snap.yes_ask),
                    no_spread: BookSnap::spread(snap.no_bid, snap.no_ask),
                    ask_sum: snap.yes_ask.unwrap_or(0.0) + snap.no_ask.unwrap_or(0.0),
                    bid_sum: snap.yes_bid.unwrap_or(0.0) + snap.no_bid.unwrap_or(0.0),
                    yes_depth_usd: ya.depth_usd + yb.depth_usd,
                    no_depth_usd: na.depth_usd + nb.depth_usd,
                    yes_levels: ya.levels + yb.levels,
                    no_levels: na.levels + nb.levels,
                    book_updates: since.min(u32::MAX as u64) as u32,
                    trades_5m,
                    traded_5m_usd,
                    secs_to_resolution: market.secs_to_resolution(),
                })));
        }

        // Markets that left the watchlist shouldn't keep a counter alive.
        let live: HashSet<String> = self.watch.read().await.keys().cloned().collect();
        prev_updates.retain(|k, _| live.contains(k));
    }

    /// Insert or clear one market's entry in an opportunity map.
    async fn store_opp(
        &self,
        market: &Market,
        map: &RwLock<HashMap<String, Opportunity>>,
        arb: Option<&ArbResult>,
    ) {
        // One write lock for read-then-insert so a concurrent recompute of the
        // same market can't reset `first_seen`.
        let mut map = map.write().await;
        match arb {
            Some(arb) => {
                let ts = now();
                let first_seen = map
                    .get(&market.condition_id)
                    .map_or(ts, |prev| prev.first_seen);
                let opp = Opportunity {
                    question: market.question.clone(),
                    slug: market.slug.clone(),
                    condition_id: market.condition_id.clone(),
                    yes_token: market.yes_token.clone(),
                    no_token: market.no_token.clone(),
                    volume: market.volume,
                    market_url: market.market_url(),
                    start_date: market.start_date.clone(),
                    end_date: market.end_date.clone(),
                    arb: arb.clone(),
                    first_seen,
                    updated_at: ts,
                };
                map.insert(market.condition_id.clone(), opp);
            }
            None => {
                map.remove(&market.condition_id);
            }
        }
    }

    /// Track the lifetime of one coherent set arb. Called on every recompute for
    /// each strategy: an active arb opens/updates a crossing; its absence closes
    /// it and records how long it was visible.
    ///
    /// This is also where the analytics event stream comes from — `open` when an
    /// occurrence appears, `update` while it stands (throttled to
    /// `analytics.sample_ms`, or one busy market would be the whole dataset),
    /// `close` when it goes, plus the closed-crossing row that carries its
    /// lifetime.
    async fn note_crossing(
        &self,
        market: &Market,
        strategy: Strategy,
        arb: Option<&ArbResult>,
        ctx: Option<&SignalContext>,
    ) {
        let key = crossing_key(&market.condition_id, strategy);
        let sample_gap = Duration::from_millis(self.analytics.config().sample_ms);
        let mut open = self.crossings_open.write().await;
        match arb {
            Some(a) => {
                let mut event = "update";
                let e = open.entry(key).or_insert_with(|| {
                    event = "open";
                    let opened_at_ms = now_ms();
                    OpenCrossing {
                        opportunity_id: format!(
                            "{}:{}:{opened_at_ms}",
                            market.condition_id,
                            strategy.as_str()
                        ),
                        opened: Instant::now(),
                        opened_at: now(),
                        opened_at_ms,
                        question: market.question.clone(),
                        market_url: market.market_url(),
                        strategy,
                        peak_avg_edge: f64::MIN,
                        peak_gross_edge: f64::MIN,
                        peak_net: f64::MIN,
                        peak_size: 0.0,
                        peak_profitable: false,
                        first_avg_edge: a.avg_edge,
                        last_avg_edge: a.avg_edge,
                        observations: 0,
                        time_to_peak_ms: 0,
                        // Back-dated so the `open` row is never throttled.
                        last_sampled: Instant::now() - sample_gap,
                    }
                });

                if a.avg_edge > e.peak_avg_edge {
                    e.peak_avg_edge = a.avg_edge;
                    e.peak_size = a.size;
                    e.peak_gross_edge = a.gross_edge;
                    e.peak_profitable = a.profitable;
                    e.time_to_peak_ms = e.opened.elapsed().as_millis().min(u32::MAX as u128) as u32;
                }
                e.peak_net = e.peak_net.max(a.net_profit);
                e.last_avg_edge = a.avg_edge;
                e.observations += 1;

                if let Some(ctx) = ctx {
                    if event == "open" || e.last_sampled.elapsed() >= sample_gap {
                        e.last_sampled = Instant::now();
                        self.analytics.record(Row::Opportunity(Box::new(ctx.row(
                            event,
                            strategy,
                            Some(a),
                            e,
                        ))));
                    }
                }
            }
            None => {
                if let Some(e) = open.remove(&key) {
                    let seq = self.crossing_seq.fetch_add(1, Ordering::Relaxed) + 1;
                    let lived_ms = e.opened.elapsed().as_millis() as u64;
                    let closed = ClosedCrossing {
                        seq,
                        condition_id: market.condition_id.clone(),
                        question: e.question.clone(),
                        market_url: e.market_url.clone(),
                        strategy: e.strategy,
                        lived_ms,
                        peak_avg_edge: e.peak_avg_edge,
                        peak_net: e.peak_net,
                        peak_size: e.peak_size,
                        opened_at: e.opened_at,
                        closed_at: now(),
                    };
                    let mut log = self.crossings_log.write().await;
                    log.push_back(closed);
                    while log.len() > CROSSING_LOG_CAP {
                        log.pop_front();
                    }
                    drop(log);

                    if let Some(ctx) = ctx {
                        // The close row carries the book as it looks now — the
                        // state that killed the edge — with no arb attached.
                        self.analytics.record(Row::Opportunity(Box::new(
                            ctx.row("close", strategy, None, &e),
                        )));
                        let gap = self.note_reopen_gap(&key).await;
                        self.analytics.record(Row::Crossing(Box::new(
                            ctx.crossing_row(seq, &e, lived_ms, gap),
                        )));
                    }
                }
            }
        }
    }

    /// Milliseconds since the previous crossing on this market and direction
    /// closed — how quickly an edge here comes back. `0` means this is the first
    /// one we have seen.
    async fn note_reopen_gap(&self, key: &str) -> u32 {
        let mut seen = self.crossing_closed_at.write().await;
        seen.insert(key.to_string(), Instant::now())
            .map_or(0, |prev| {
                prev.elapsed().as_millis().min(u32::MAX as u128) as u32
            })
    }

    /// Every live opportunity across both strategies.
    pub async fn all_opportunities(&self) -> Vec<Opportunity> {
        let mint = self.opps.read().await;
        let split = self.split_opps.read().await;
        mint.values().chain(split.values()).cloned().collect()
    }

    /// Closed crossings with `seq > after_seq`, oldest first, plus the newest seq
    /// so the caller can poll incrementally.
    pub async fn crossings_since(
        &self,
        after_seq: u64,
        limit: usize,
    ) -> (u64, Vec<ClosedCrossing>) {
        let log = self.crossings_log.read().await;
        let last = self.crossing_seq.load(Ordering::Relaxed);
        let items: Vec<ClosedCrossing> = log
            .iter()
            .filter(|c| c.seq > after_seq)
            .take(limit)
            .cloned()
            .collect();
        (last, items)
    }

    pub async fn conditions_for_tokens(&self, tokens: &[String]) -> HashSet<String> {
        let idx = self.token_index.read().await;
        tokens.iter().filter_map(|t| idx.get(t).cloned()).collect()
    }

    /// Persist watched categories and pinned markets. Serialized by `persist_lock`
    /// so concurrent callers never write at once, with the snapshot taken after
    /// the lock so the last writer flushes the latest state; written to a temp
    /// file and renamed so a reader never sees a torn file.
    pub async fn persist(&self) {
        let _guard = self.persist_lock.lock().await;
        let data = Persisted {
            categories: self.categories.read().await.values().cloned().collect(),
            manual: self.manual.read().await.values().cloned().collect(),
        };
        let bytes = match serde_json::to_vec_pretty(&data) {
            Ok(b) => b,
            Err(e) => return tracing::warn!("cannot serialize state: {e}"),
        };
        let path = self.cfg.watch_file.clone();
        let mut tmp = path.clone().into_os_string();
        tmp.push(".tmp");
        let tmp = std::path::PathBuf::from(tmp);
        if let Err(e) = tokio::fs::write(&tmp, bytes).await {
            return tracing::warn!("cannot write {}: {e}", tmp.display());
        }
        if let Err(e) = tokio::fs::rename(&tmp, &path).await {
            tracing::warn!("cannot rename into {}: {e}", path.display());
        }
    }

    pub async fn load_persisted(&self) {
        let path = &self.cfg.watch_file;
        let bytes = match tokio::fs::read(path).await {
            Ok(b) => b,
            Err(_) => return,
        };
        if let Ok(data) = serde_json::from_slice::<Persisted>(&bytes) {
            for c in data.categories.iter().cloned() {
                self.add_category(c).await;
            }
            for m in data.manual {
                self.add_manual_market(m).await;
            }
            tracing::info!(
                "loaded {} categories, {} pinned markets from {}",
                self.categories.read().await.len(),
                self.manual.read().await.len(),
                path.display()
            );
        } else if let Ok(markets) = serde_json::from_slice::<Vec<Market>>(&bytes) {
            // Backward compatibility with the pre-category file format.
            for m in markets {
                self.add_manual_market(m).await;
            }
            tracing::info!(
                "migrated {} markets from legacy {}",
                self.manual.read().await.len(),
                path.display()
            );
        } else {
            tracing::warn!("cannot parse {}", path.display());
        }
    }
}

/// Everything about the market at the instant of a signal that does not depend
/// on which direction is being evaluated. Built once per recompute and turned
/// into a row per direction, so the expensive part — cutting four ladders — is
/// paid once.
struct SignalContext {
    ts: String,
    condition_id: String,
    slug: String,
    question: String,
    category: String,
    market_volume: f64,
    liquidity_tier: &'static str,

    yes_bid: f64,
    yes_ask: f64,
    no_bid: f64,
    no_ask: f64,
    yes_spread: f64,
    no_spread: f64,
    ask_sum: f64,
    bid_sum: f64,
    ladders: [Ladder; 4],
    book_age_ms: u32,
    trades_5m: u32,
    traded_5m_usd: f64,

    fee_bps: f64,
    slippage_bps: f64,
    gas_cost: f64,
    max_capital: f64,
    secs_to_resolution: i64,
    detect_latency_us: u32,
}

impl SignalContext {
    fn build(
        market: &Market,
        snap: &BookSnap,
        params: &arb_core::ArbParams,
        received: Instant,
        trades_5m: u32,
        traded_5m_usd: f64,
    ) -> Self {
        let (yes_bid, yes_ask) = (snap.yes_bid.unwrap_or(0.0), snap.yes_ask.unwrap_or(0.0));
        let (no_bid, no_ask) = (snap.no_bid.unwrap_or(0.0), snap.no_ask.unwrap_or(0.0));
        Self {
            ts: arb_core::time::clickhouse_datetime(now_ms()),
            condition_id: market.condition_id.clone(),
            slug: market.slug.clone(),
            question: market.question.clone(),
            category: market.category_label(),
            market_volume: market.volume,
            liquidity_tier: liquidity_tier(market.volume),
            yes_bid,
            yes_ask,
            no_bid,
            no_ask,
            yes_spread: BookSnap::spread(snap.yes_bid, snap.yes_ask),
            no_spread: BookSnap::spread(snap.no_bid, snap.no_ask),
            ask_sum: yes_ask + no_ask,
            bid_sum: yes_bid + no_bid,
            ladders: snap.ladders(5),
            book_age_ms: snap.age_ms(),
            trades_5m,
            traded_5m_usd,
            fee_bps: params.fee_bps,
            slippage_bps: params.slippage_bps,
            gas_cost: params.gas_cost,
            max_capital: params.max_capital,
            secs_to_resolution: market.secs_to_resolution(),
            detect_latency_us: received.elapsed().as_micros().min(u32::MAX as u128) as u32,
        }
    }

    /// One wide event row. `arb` is `None` for the `close` event, where the book
    /// state is still worth having but there is no longer an edge to describe.
    fn row(
        &self,
        event: &'static str,
        strategy: Strategy,
        arb: Option<&ArbResult>,
        open: &OpenCrossing,
    ) -> OpportunityRow {
        let [ya, yb, na, nb] = &self.ladders;
        let z = |f: fn(&ArbResult) -> f64| arb.map_or(0.0, f);
        OpportunityRow {
            ts: self.ts.clone(),
            event,
            opportunity_id: open.opportunity_id.clone(),
            strategy: strategy.as_str(),
            condition_id: self.condition_id.clone(),
            slug: self.slug.clone(),
            question: self.question.clone(),
            category: self.category.clone(),

            yes_bid: self.yes_bid,
            yes_ask: self.yes_ask,
            no_bid: self.no_bid,
            no_ask: self.no_ask,
            yes_spread: self.yes_spread,
            no_spread: self.no_spread,
            ask_sum: self.ask_sum,
            bid_sum: self.bid_sum,
            yes_ask_prices: ya.prices.clone(),
            yes_ask_sizes: ya.sizes.clone(),
            yes_bid_prices: yb.prices.clone(),
            yes_bid_sizes: yb.sizes.clone(),
            no_ask_prices: na.prices.clone(),
            no_ask_sizes: na.sizes.clone(),
            no_bid_prices: nb.prices.clone(),
            no_bid_sizes: nb.sizes.clone(),
            yes_depth_usd: ya.depth_usd + yb.depth_usd,
            no_depth_usd: na.depth_usd + nb.depth_usd,
            yes_levels: ya.levels + yb.levels,
            no_levels: na.levels + nb.levels,
            book_age_ms: self.book_age_ms,
            trades_5m: self.trades_5m,
            traded_5m_usd: self.traded_5m_usd,
            market_volume: self.market_volume,
            liquidity_tier: self.liquidity_tier,

            gross_edge: z(|a| a.gross_edge),
            avg_edge: z(|a| a.avg_edge),
            size: z(|a| a.size),
            cost: z(|a| a.cost),
            payout: z(|a| a.payout),
            fees: z(|a| a.fees),
            gas: z(|a| a.gas),
            net_profit: z(|a| a.net_profit),
            roi: z(|a| a.roi),
            profitable: u8::from(arb.is_some_and(|a| a.profitable)),
            // What sweeping down the ladder gives back versus the headline
            // top-of-book edge — the "my own size ate it" number, in dollars.
            depth_cost_usd: z(|a| (a.gross_edge - a.avg_edge) * a.size),
            fee_bps: self.fee_bps,
            slippage_bps: self.slippage_bps,
            gas_cost: self.gas_cost,
            max_capital: self.max_capital,

            capital_locked: z(|a| a.cost),
            secs_to_resolution: self.secs_to_resolution,

            detect_latency_us: self.detect_latency_us,
            age_ms: open.opened.elapsed().as_millis().min(u32::MAX as u128) as u32,
            observation: open.observations,
        }
    }

    fn crossing_row(
        &self,
        seq: u64,
        open: &OpenCrossing,
        lived_ms: u64,
        reopen_gap_ms: u32,
    ) -> CrossingRow {
        CrossingRow {
            closed_at: self.ts.clone(),
            opened_at: arb_core::time::clickhouse_datetime(open.opened_at_ms),
            seq,
            opportunity_id: open.opportunity_id.clone(),
            condition_id: self.condition_id.clone(),
            slug: self.slug.clone(),
            question: self.question.clone(),
            category: self.category.clone(),
            strategy: open.strategy.as_str(),
            lived_ms: lived_ms.min(u32::MAX as u64) as u32,
            observations: open.observations,
            time_to_peak_ms: open.time_to_peak_ms,
            peak_avg_edge: finite(open.peak_avg_edge),
            peak_gross_edge: finite(open.peak_gross_edge),
            peak_net: finite(open.peak_net),
            peak_size: open.peak_size,
            first_avg_edge: open.first_avg_edge,
            last_avg_edge: open.last_avg_edge,
            peak_profitable: u8::from(open.peak_profitable),
            market_volume: self.market_volume,
            liquidity_tier: self.liquidity_tier,
            reopen_gap_ms,
        }
    }
}

/// Peaks start at `f64::MIN` so the first sample always wins; a crossing that
/// somehow closed without one must not put that sentinel into ClickHouse.
fn finite(v: f64) -> f64 {
    if v.is_finite() && v > f64::MIN {
        v
    } else {
        0.0
    }
}

fn crossing_key(condition_id: &str, strategy: Strategy) -> String {
    format!("{condition_id}:{}", strategy.as_str())
}

/// Drop trade prints that have aged out of the rolling window.
fn prune(q: &mut VecDeque<TradePrint>, window: Duration) {
    while q.front().is_some_and(|t| t.at.elapsed() > window) {
        q.pop_front();
    }
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Unix milliseconds — the resolution ClickHouse stores and the one every
/// sub-second measurement here (book age, crossing lifetime) needs.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analytics::AnalyticsConfig;

    fn market(i: usize) -> Market {
        Market {
            question: format!("question {i}"),
            slug: format!("slug-{i}"),
            condition_id: format!("cond-{i}"),
            yes_token: format!("yes-{i}"),
            no_token: format!("no-{i}"),
            volume: 0.0,
            start_date: None,
            end_date: None,
            fee_bps: None,
            category: None,
        }
    }

    /// A sink pointed at a port nothing listens on: `enabled()` is true, so the
    /// trade path runs past its early return, and every flush fails harmlessly.
    fn state() -> AppState {
        let analytics = Analytics::start(
            AnalyticsConfig {
                url: "http://127.0.0.1:1/".into(),
                ..Default::default()
            },
            1,
        )
        .expect("analytics sink");
        AppState::new(Arc::new(Config::default()), analytics)
    }

    /// A discovery pass and a trade print must not be able to wedge each other.
    /// These two paths touch `watch` and `token_index` from opposite ends, and
    /// when they took them in opposite orders a single interleaving deadlocked
    /// every task in the process. A deadlock cannot be asserted on directly, so
    /// this test can only fail by hanging — which is why it carries its own
    /// timeout rather than waiting for the harness to give up.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_discovery_pass_and_a_trade_print_do_not_deadlock() {
        let state = state();
        // Seed the token the trade prints reference, so they reach the `watch`
        // lookup instead of returning early on an unknown token.
        state.add_market(market(0)).await;

        let writer = tokio::spawn({
            let state = state.clone();
            async move {
                for i in 0..5_000 {
                    state.add_market(market(i)).await;
                }
            }
        });
        let trades = tokio::spawn({
            let state = state.clone();
            async move {
                for _ in 0..5_000 {
                    state.record_trade("yes-0", 0.5, 10.0, "BUY").await;
                }
            }
        });

        tokio::time::timeout(Duration::from_secs(30), async {
            writer.await.unwrap();
            trades.await.unwrap();
        })
        .await
        .expect("deadlock: watch and token_index acquired in conflicting orders");
    }
}
