//! Shared runtime state: the user-curated watchlist, live order books fed by
//! the websocket, and the derived opportunities.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use arb_core::{evaluate, ArbResult, Level};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, Notify, RwLock};

use crate::config::Config;

/// How many closed micro-crossings to retain for `/crossings`.
const CROSSING_LOG_CAP: usize = 500;

/// A coherent buy-set arb (`ask_YES + ask_NO < 1`) currently open on a market —
/// a "micro-crossing" of native liquidity through the mirror. Tracked so we can
/// measure how long it stays visible.
struct OpenCrossing {
    opened: Instant,
    opened_at: u64,
    question: String,
    market_url: String,
    peak_avg_edge: f64,
    peak_net: f64,
    peak_size: f64,
}

/// A micro-crossing after it disappeared, with the lifetime it was visible.
#[derive(Debug, Clone, Serialize)]
pub struct ClosedCrossing {
    pub seq: u64,
    pub condition_id: String,
    pub question: String,
    pub market_url: String,
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
    #[serde(default)]
    pub end_date: Option<String>,
    /// Taker fee (bps per side) for this market's category, assigned at discovery.
    /// `None` falls back to the global `fee_bps`.
    #[serde(default)]
    pub fee_bps: Option<f64>,
}

impl Market {
    pub fn market_url(&self) -> String {
        format!("https://polymarket.com/market/{}", self.slug)
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
}

impl LiveBook {
    pub fn replace(&mut self, bids: Vec<(String, f64)>, asks: Vec<(String, f64)>) {
        self.bids = bids.into_iter().filter(|(_, s)| *s > 0.0).collect();
        self.asks = asks.into_iter().filter(|(_, s)| *s > 0.0).collect();
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
    }

    fn levels(side: &HashMap<String, f64>) -> Vec<Level> {
        side.iter()
            .filter_map(|(p, s)| p.parse::<f64>().ok().map(|p| Level::new(p, *s)))
            .collect()
    }

    pub fn asks(&self) -> Vec<Level> {
        Self::levels(&self.asks)
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
/// treating a computed edge as real.
struct BookSnap {
    yes_asks: Vec<Level>,
    no_asks: Vec<Level>,
    yes_bid: Option<f64>,
    yes_ask: Option<f64>,
    no_bid: Option<f64>,
    no_ask: Option<f64>,
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
    pub end_date: Option<String>,
    pub arb: ArbResult,
    pub updated_at: u64,
}

#[derive(Clone)]
pub struct AppState {
    pub cfg: Arc<Config>,
    pub watch: Arc<RwLock<HashMap<String, Market>>>,
    pub categories: Arc<RwLock<HashMap<String, Category>>>,
    pub manual: Arc<RwLock<HashMap<String, Market>>>,
    pub token_index: Arc<RwLock<HashMap<String, String>>>,
    pub books: Arc<RwLock<HashMap<String, LiveBook>>>,
    pub opps: Arc<RwLock<HashMap<String, Opportunity>>>,
    crossings_open: Arc<RwLock<HashMap<String, OpenCrossing>>>,
    crossings_log: Arc<RwLock<VecDeque<ClosedCrossing>>>,
    crossing_seq: Arc<AtomicU64>,
    pub resubscribe: Arc<Notify>,
    pub ws_connected: Arc<AtomicBool>,
    pub last_update: Arc<AtomicU64>,
    persist_lock: Arc<Mutex<()>>,
}

impl AppState {
    pub fn new(cfg: Arc<Config>) -> Self {
        Self {
            cfg,
            watch: Default::default(),
            categories: Default::default(),
            manual: Default::default(),
            token_index: Default::default(),
            books: Default::default(),
            opps: Default::default(),
            crossings_open: Default::default(),
            crossings_log: Default::default(),
            crossing_seq: Arc::new(AtomicU64::new(0)),
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

    /// Add a market and its token routing, seeding empty books. Returns false
    /// if it was already watched.
    pub async fn add_market(&self, m: Market) -> bool {
        let mut watch = self.watch.write().await;
        if watch.contains_key(&m.condition_id) {
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

    /// Re-run the arbitrage math for one market from its current live books and
    /// store or clear the resulting opportunity.
    pub async fn recompute(&self, condition_id: &str) {
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
                yes_bid: yes.and_then(LiveBook::best_bid),
                yes_ask: yes.and_then(LiveBook::best_ask),
                no_bid: no.and_then(LiveBook::best_bid),
                no_ask: no.and_then(LiveBook::best_ask),
            }
        };

        self.last_update.store(now(), Ordering::Relaxed);

        let arb = if snap.is_coherent() {
            let params = arb_core::ArbParams {
                fee_bps: market.fee_bps.unwrap_or(self.cfg.arb.fee_bps),
                ..self.cfg.arb
            };
            evaluate(&snap.yes_asks, &snap.no_asks, &params)
        } else {
            None
        };

        match &arb {
            Some(arb) => {
                let opp = Opportunity {
                    question: market.question.clone(),
                    slug: market.slug.clone(),
                    condition_id: market.condition_id.clone(),
                    yes_token: market.yes_token.clone(),
                    no_token: market.no_token.clone(),
                    volume: market.volume,
                    market_url: market.market_url(),
                    end_date: market.end_date.clone(),
                    arb: arb.clone(),
                    updated_at: now(),
                };
                self.opps
                    .write()
                    .await
                    .insert(condition_id.to_string(), opp);
            }
            None => {
                self.opps.write().await.remove(condition_id);
            }
        }

        self.note_crossing(&market, arb.as_ref()).await;
    }

    /// Track the lifetime of a coherent buy-set arb. Called on every recompute:
    /// an active arb opens/updates a crossing; its absence closes it and records
    /// how long it was visible.
    async fn note_crossing(&self, market: &Market, arb: Option<&ArbResult>) {
        let mut open = self.crossings_open.write().await;
        match arb {
            Some(a) => {
                let e = open
                    .entry(market.condition_id.clone())
                    .or_insert_with(|| OpenCrossing {
                        opened: Instant::now(),
                        opened_at: now(),
                        question: market.question.clone(),
                        market_url: market.market_url(),
                        peak_avg_edge: f64::MIN,
                        peak_net: f64::MIN,
                        peak_size: 0.0,
                    });
                if a.avg_edge > e.peak_avg_edge {
                    e.peak_avg_edge = a.avg_edge;
                    e.peak_size = a.size;
                }
                e.peak_net = e.peak_net.max(a.net_profit);
            }
            None => {
                if let Some(e) = open.remove(&market.condition_id) {
                    let seq = self.crossing_seq.fetch_add(1, Ordering::Relaxed) + 1;
                    let closed = ClosedCrossing {
                        seq,
                        condition_id: market.condition_id.clone(),
                        question: e.question,
                        market_url: e.market_url,
                        lived_ms: e.opened.elapsed().as_millis() as u64,
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
                }
            }
        }
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

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
