//! Shared runtime state: the user-curated watchlist, live order books fed by
//! the websocket, and the derived opportunities.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use arb_core::{evaluate, sell_side_edge, ArbResult, Level};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, Notify, RwLock};

use crate::config::Config;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Market {
    pub question: String,
    pub slug: String,
    pub condition_id: String,
    pub yes_token: String,
    pub no_token: String,
    pub volume: f64,
}

impl Market {
    pub fn market_url(&self) -> String {
        format!("https://polymarket.com/market/{}", self.slug)
    }
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
    pub arb: ArbResult,
    pub sell_side_edge: Option<f64>,
    pub updated_at: u64,
}

#[derive(Clone)]
pub struct AppState {
    pub cfg: Arc<Config>,
    pub watch: Arc<RwLock<HashMap<String, Market>>>,
    pub token_index: Arc<RwLock<HashMap<String, String>>>,
    pub books: Arc<RwLock<HashMap<String, LiveBook>>>,
    pub opps: Arc<RwLock<HashMap<String, Opportunity>>>,
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
            token_index: Default::default(),
            books: Default::default(),
            opps: Default::default(),
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

        let (yes_asks, no_asks, bid_yes, bid_no) = {
            let books = self.books.read().await;
            let yes = books.get(&market.yes_token);
            let no = books.get(&market.no_token);
            (
                yes.map(LiveBook::asks).unwrap_or_default(),
                no.map(LiveBook::asks).unwrap_or_default(),
                yes.and_then(LiveBook::best_bid),
                no.and_then(LiveBook::best_bid),
            )
        };

        let now = now();
        self.last_update.store(now, Ordering::Relaxed);

        match evaluate(&yes_asks, &no_asks, &self.cfg.arb) {
            Some(arb) => {
                let sse = bid_yes.zip(bid_no).and_then(|(y, n)| sell_side_edge(y, n));
                let opp = Opportunity {
                    question: market.question.clone(),
                    slug: market.slug.clone(),
                    condition_id: market.condition_id.clone(),
                    yes_token: market.yes_token.clone(),
                    no_token: market.no_token.clone(),
                    volume: market.volume,
                    market_url: market.market_url(),
                    arb,
                    sell_side_edge: sse,
                    updated_at: now,
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
    }

    pub async fn conditions_for_tokens(&self, tokens: &[String]) -> HashSet<String> {
        let idx = self.token_index.read().await;
        tokens.iter().filter_map(|t| idx.get(t).cloned()).collect()
    }

    /// Persist the watchlist. Serialized by `persist_lock` so concurrent callers
    /// never write the file at the same time, and the snapshot is taken *after*
    /// acquiring the lock so the last writer always flushes the latest state.
    /// Writes to a temp file and renames, so a reader never sees a torn file.
    pub async fn persist(&self) {
        let _guard = self.persist_lock.lock().await;
        let markets = self.list_markets().await;
        let bytes = match serde_json::to_vec_pretty(&markets) {
            Ok(b) => b,
            Err(e) => return tracing::warn!("cannot serialize watchlist: {e}"),
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
        match serde_json::from_slice::<Vec<Market>>(&bytes) {
            Ok(markets) => {
                let n = markets.len();
                for m in markets {
                    self.add_market(m).await;
                }
                tracing::info!("loaded {n} watched markets from {}", path.display());
            }
            Err(e) => tracing::warn!("cannot parse {}: {e}", path.display()),
        }
    }
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
