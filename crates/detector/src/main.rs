//! Websocket-first, alerts-only backend. It streams Polymarket order books for
//! a user-curated watchlist, runs the YES/NO arbitrage math on every update, and
//! exposes opportunities plus watchlist management over REST. It never places,
//! signs or funds any order.

mod analytics;
mod config;
mod gamma;
mod state;
mod ws;

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::Deserialize;

use crate::analytics::{query as aq, Analytics, ExecutionRow, Row};
use crate::gamma::Gamma;
use crate::state::{now, now_ms, AppState, Market};

#[derive(Clone)]
struct App {
    state: AppState,
    gamma: Arc<Gamma>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();
    let cfg = config::load()?;

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| cfg.log_level.clone().into()),
        )
        .init();

    let cfg = Arc::new(cfg);
    let gamma = Arc::new(Gamma::new(cfg.gamma_url.clone(), cfg.http_timeout_secs)?);
    let analytics = Analytics::start(cfg.analytics.clone(), cfg.http_timeout_secs)?;
    let state = AppState::new(cfg.clone(), analytics);
    state.load_persisted().await;

    if state.analytics.enabled() && cfg.analytics.book_sample_secs > 0 {
        tokio::spawn(book_sample_loop(state.clone()));
    }

    if cfg.auto_discover {
        tracing::info!(
            interval = cfg.discover_interval_secs,
            min_volume = cfg.discover_min_volume,
            "category auto-discovery enabled"
        );
        tokio::spawn(discover_loop(state.clone(), gamma.clone()));
    }

    let app = App {
        state: state.clone(),
        gamma,
    };

    tokio::spawn(ws::run(state.clone()));
    state.wake_resubscribe();

    let router = Router::new()
        .route("/health", get(health))
        .route("/config", get(get_config))
        .route("/opportunities", get(list_opportunities))
        .route("/crossings", get(crossings))
        .route("/watch", get(list_watch).post(add_watch))
        .route("/watch/:key", delete(remove_watch))
        .route("/categories", get(list_categories))
        .route("/category", post(add_category))
        .route("/category/:slug", delete(remove_category))
        .route("/search", get(search))
        .route("/analytics/health", get(an_health))
        .route("/analytics/summary", get(an_summary))
        .route("/analytics/edge_histogram", get(an_edge_histogram))
        .route("/analytics/decay", get(an_decay))
        .route("/analytics/timeline", get(an_timeline))
        .route("/analytics/markets", get(an_markets))
        .route("/analytics/categories", get(an_categories))
        .route("/analytics/heatmap", get(an_heatmap))
        .route("/analytics/size_curve", get(an_size_curve))
        .route("/analytics/liquidity", get(an_liquidity))
        .route("/analytics/executions", get(an_executions))
        .route("/analytics/export", get(an_export))
        .route("/analytics/execution", post(an_log_execution))
        .with_state(app);

    let listener = tokio::net::TcpListener::bind(&cfg.bind_addr).await?;
    tracing::info!("listening on http://{}", cfg.bind_addr);
    axum::serve(listener, router).await?;
    Ok(())
}

#[derive(Debug, Deserialize)]
struct ListQuery {
    profitable: Option<bool>,
    min_edge: Option<f64>,
    min_profit: Option<f64>,
    /// "mint" (buy set below $1) or "split" (mint a set, sell both bids above
    /// $1). Omitted returns both.
    strategy: Option<String>,
    limit: Option<usize>,
}

async fn list_opportunities(
    State(app): State<App>,
    Query(q): Query<ListQuery>,
) -> Json<serde_json::Value> {
    let opps = app.state.all_opportunities().await;
    let mut items: Vec<_> = opps
        .iter()
        .filter(|o| {
            q.strategy
                .as_deref()
                .is_none_or(|s| s.eq_ignore_ascii_case(o.arb.strategy.as_str()))
        })
        .filter(|o| q.profitable.is_none_or(|p| o.arb.profitable == p))
        .filter(|o| q.min_edge.is_none_or(|e| o.arb.gross_edge >= e))
        .filter(|o| q.min_profit.is_none_or(|p| o.arb.net_profit >= p))
        .collect();
    items.sort_by(|a, b| b.arb.net_profit.partial_cmp(&a.arb.net_profit).unwrap());
    if let Some(limit) = q.limit {
        items.truncate(limit);
    }
    Json(serde_json::json!({
        "ws_connected": app.state.ws_connected.load(Ordering::Relaxed),
        "count": items.len(),
        "opportunities": items,
    }))
}

#[derive(Debug, Deserialize)]
struct AddRequest {
    input: String,
}

async fn add_watch(
    State(app): State<App>,
    Json(req): Json<AddRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let markets = app
        .gamma
        .resolve(&req.input)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;
    if markets.is_empty() {
        return Err((
            StatusCode::NOT_FOUND,
            "no tradable YES/NO market found for input".into(),
        ));
    }

    let mut added = Vec::new();
    for m in markets {
        let url = m.market_url();
        let (question, condition_id) = (m.question.clone(), m.condition_id.clone());
        if app.state.add_manual_market(m).await {
            added.push(serde_json::json!({
                "question": question,
                "condition_id": condition_id,
                "market_url": url,
            }));
        }
    }

    if !added.is_empty() {
        app.state.persist().await;
        app.state.wake_resubscribe();
    }
    Ok(Json(
        serde_json::json!({ "added": added.len(), "markets": added }),
    ))
}

async fn remove_watch(
    State(app): State<App>,
    Path(key): Path<String>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    match app.state.remove_manual(&key).await {
        Some(m) => {
            app.state.persist().await;
            app.state.wake_resubscribe();
            Ok(Json(serde_json::json!({
                "removed": m.condition_id,
                "question": m.question,
            })))
        }
        None => Err(StatusCode::NOT_FOUND),
    }
}

async fn add_category(
    State(app): State<App>,
    Json(req): Json<AddRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let category = app
        .gamma
        .resolve_category(&req.input)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?
        .ok_or((
            StatusCode::NOT_FOUND,
            "no Polymarket category found for input".into(),
        ))?;

    let added = app.state.add_category(category.clone()).await;
    if added {
        app.state.persist().await;
        run_discovery_once(&app.state, &app.gamma).await;
    }
    let markets = app.state.list_markets().await.len();
    Ok(Json(serde_json::json!({
        "added": added,
        "category": { "slug": category.slug, "label": category.label },
        "watched_markets": markets,
    })))
}

async fn remove_category(
    State(app): State<App>,
    Path(slug): Path<String>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    match app.state.remove_category(&slug).await {
        Some(c) => {
            app.state.persist().await;
            run_discovery_once(&app.state, &app.gamma).await;
            Ok(Json(
                serde_json::json!({ "removed": c.slug, "label": c.label }),
            ))
        }
        None => Err(StatusCode::NOT_FOUND),
    }
}

async fn list_categories(State(app): State<App>) -> Json<serde_json::Value> {
    let cats = app.state.list_categories().await;
    let items: Vec<_> = cats
        .iter()
        .map(|c| serde_json::json!({ "slug": c.slug, "label": c.label, "tag_id": c.tag_id }))
        .collect();
    Json(serde_json::json!({ "count": items.len(), "categories": items }))
}

#[derive(Debug, Deserialize)]
struct CrossingsQuery {
    after_seq: Option<u64>,
    limit: Option<usize>,
}

async fn crossings(
    State(app): State<App>,
    Query(q): Query<CrossingsQuery>,
) -> Json<serde_json::Value> {
    let (last_seq, items) = app
        .state
        .crossings_since(q.after_seq.unwrap_or(0), q.limit.unwrap_or(50))
        .await;
    Json(serde_json::json!({
        "last_seq": last_seq,
        "count": items.len(),
        "crossings": items,
    }))
}

async fn list_watch(State(app): State<App>) -> Json<serde_json::Value> {
    let markets = app.state.list_markets().await;
    let items: Vec<_> = markets
        .iter()
        .map(|m| {
            serde_json::json!({
                "question": m.question,
                "slug": m.slug,
                "condition_id": m.condition_id,
                "volume": m.volume,
                "market_url": m.market_url(),
                "start_date": m.start_date,
                "end_date": m.end_date,
            })
        })
        .collect();
    Json(serde_json::json!({ "count": items.len(), "markets": items }))
}

#[derive(Debug, Deserialize)]
struct SearchQuery {
    q: String,
    limit: Option<usize>,
}

async fn search(
    State(app): State<App>,
    Query(q): Query<SearchQuery>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let limit = q.limit.unwrap_or(app.state.cfg.search_limit);
    let markets = app
        .gamma
        .search(&q.q, limit)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;
    let items: Vec<_> = markets
        .iter()
        .map(|m| {
            serde_json::json!({
                "question": m.question,
                "slug": m.slug,
                "condition_id": m.condition_id,
                "volume": m.volume,
                "market_url": m.market_url(),
                "start_date": m.start_date,
                "end_date": m.end_date,
            })
        })
        .collect();
    Ok(Json(
        serde_json::json!({ "count": items.len(), "results": items }),
    ))
}

async fn health(State(app): State<App>) -> Json<serde_json::Value> {
    let last = app.state.last_update.load(Ordering::Relaxed);
    let age = (last > 0).then(|| now().saturating_sub(last));
    Json(serde_json::json!({
        "status": "ok",
        "ws_connected": app.state.ws_connected.load(Ordering::Relaxed),
        "watched": app.state.list_markets().await.len(),
        "mint_opportunities": app.state.opps.read().await.len(),
        "split_opportunities": app.state.split_opps.read().await.len(),
        "last_update": last,
        "age_secs": age,
        "analytics": app.state.analytics.stats(),
    }))
}

async fn discover_loop(state: AppState, gamma: Arc<Gamma>) {
    let interval = state.cfg.discover_interval_secs.max(30);
    let mut ticker = tokio::time::interval(std::time::Duration::from_secs(interval));
    loop {
        ticker.tick().await;
        run_discovery_once(&state, &gamma).await;
    }
}

/// Rebuild the desired watchlist from all watched categories plus pinned
/// markets, then reconcile. If a category fetch fails, skip removals this pass
/// so a transient error can't wipe markets.
async fn run_discovery_once(state: &AppState, gamma: &Gamma) {
    let categories = state.list_categories().await;
    let mut desired: HashMap<String, Market> = state
        .manual_markets()
        .await
        .into_iter()
        .map(|m| (m.condition_id.clone(), m))
        .collect();

    let mut all_ok = true;
    for c in &categories {
        let fee = state
            .cfg
            .category_fee_bps
            .get(&c.slug)
            .copied()
            .unwrap_or(state.cfg.arb.fee_bps);
        match gamma
            .markets_by_tag(
                &c.tag_id,
                state.cfg.discover_min_volume,
                state.cfg.discover_max_markets,
            )
            .await
        {
            Ok(markets) => {
                for mut m in markets {
                    // A market in several watched categories keeps the highest
                    // fee, and is attributed to whichever of them charges it.
                    let prev = desired
                        .get(&m.condition_id)
                        .and_then(|e| e.fee_bps.map(|f| (f, e.category.clone())));
                    match prev {
                        Some((prev_fee, prev_cat)) if prev_fee >= fee => {
                            m.fee_bps = Some(prev_fee);
                            m.category = prev_cat;
                        }
                        _ => {
                            m.fee_bps = Some(fee);
                            m.category = Some(c.slug.clone());
                        }
                    }
                    desired.insert(m.condition_id.clone(), m);
                }
            }
            Err(e) => {
                all_ok = false;
                tracing::warn!("discovery for category '{}' failed: {e}", c.slug);
            }
        }
    }

    let markets: Vec<_> = desired.into_values().collect();
    let n = markets.len();
    if state.sync_watchlist(markets, all_ok).await {
        state.wake_resubscribe();
        tracing::info!(
            "discovery synced {n} markets across {} categories",
            categories.len()
        );
    }
}

/// Probe every watched market on a timer, so the dataset has a baseline and not
/// just the rare moments an arb existed.
async fn book_sample_loop(state: AppState) {
    let secs = state.cfg.analytics.book_sample_secs.max(10);
    let mut prev_updates = HashMap::new();
    let mut ticker = tokio::time::interval(std::time::Duration::from_secs(secs));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    tracing::info!(every_secs = secs, "book sampling enabled");
    loop {
        ticker.tick().await;
        state.sample_books(&mut prev_updates).await;
    }
}

// --- Analytics ------------------------------------------------------------
//
// Thin HTTP over `analytics::query`. The detector owns ClickHouse — the bot
// never talks to it — so credentials stay in one process and the bot keeps
// talking only to this API and to Telegram.

#[derive(Debug, Default, Deserialize)]
struct AnalyticsQuery {
    /// `30m`, `24h`, `7d`, `90d`, or bare seconds. Defaults per endpoint.
    window: Option<String>,
    strategy: Option<String>,
    bucket: Option<f64>,
    bins: Option<u64>,
    bucket_secs: Option<u64>,
    limit: Option<u64>,
    /// Minutes east of UTC, for the hour-of-day breakdown.
    tz: Option<i64>,
    table: Option<String>,
}

/// `45s` / `30m` / `24h` / `7d` / `2w`, or a plain number of seconds.
fn parse_window(s: &str) -> Option<u64> {
    let s = s.trim();
    let (digits, mult) = match s.chars().last()? {
        's' | 'S' => (&s[..s.len() - 1], 1),
        'm' | 'M' => (&s[..s.len() - 1], 60),
        'h' | 'H' => (&s[..s.len() - 1], 3_600),
        'd' | 'D' => (&s[..s.len() - 1], 86_400),
        'w' | 'W' => (&s[..s.len() - 1], 7 * 86_400),
        _ => (s, 1),
    };
    digits.trim().parse::<u64>().ok()?.checked_mul(mult)
}

impl AnalyticsQuery {
    fn secs(&self, default: u64) -> u64 {
        self.window
            .as_deref()
            .and_then(parse_window)
            .unwrap_or(default)
    }
}

/// 503 while analytics is off or ClickHouse has not answered yet, 502 when a
/// query itself fails — the bot renders either as a plain message.
fn an_error(e: anyhow::Error) -> (StatusCode, String) {
    (StatusCode::BAD_GATEWAY, e.to_string())
}

fn an_ready(app: &App) -> Result<(), (StatusCode, String)> {
    if !app.state.analytics.enabled() {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "analytics is disabled — set clickhouse_url (DETECTOR_CLICKHOUSE_URL)".into(),
        ));
    }
    if !app.state.analytics.ready() {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "analytics is still connecting to ClickHouse".into(),
        ));
    }
    Ok(())
}

type AnResult = Result<Json<serde_json::Value>, (StatusCode, String)>;

async fn an_health(State(app): State<App>) -> Json<serde_json::Value> {
    Json(app.state.analytics.stats())
}

async fn an_summary(State(app): State<App>, Query(q): Query<AnalyticsQuery>) -> AnResult {
    an_ready(&app)?;
    aq::summary(&app.state.analytics, q.secs(86_400), q.strategy.as_deref())
        .await
        .map(Json)
        .map_err(an_error)
}

async fn an_edge_histogram(State(app): State<App>, Query(q): Query<AnalyticsQuery>) -> AnResult {
    an_ready(&app)?;
    aq::edge_histogram(
        &app.state.analytics,
        q.secs(7 * 86_400),
        q.strategy.as_deref(),
        q.bucket.unwrap_or(0.0025),
        q.bins.unwrap_or(20),
    )
    .await
    .map(Json)
    .map_err(an_error)
}

async fn an_decay(State(app): State<App>, Query(q): Query<AnalyticsQuery>) -> AnResult {
    an_ready(&app)?;
    aq::decay(
        &app.state.analytics,
        q.secs(7 * 86_400),
        q.strategy.as_deref(),
    )
    .await
    .map(Json)
    .map_err(an_error)
}

async fn an_timeline(State(app): State<App>, Query(q): Query<AnalyticsQuery>) -> AnResult {
    an_ready(&app)?;
    let window = q.secs(24 * 3_600);
    // ~48 points on the axis whatever the window, rounded to a whole minute.
    let bucket = q.bucket_secs.unwrap_or_else(|| (window / 48).max(60));
    aq::timeline(&app.state.analytics, window, bucket, q.strategy.as_deref())
        .await
        .map(Json)
        .map_err(an_error)
}

async fn an_markets(State(app): State<App>, Query(q): Query<AnalyticsQuery>) -> AnResult {
    an_ready(&app)?;
    aq::markets(
        &app.state.analytics,
        q.secs(7 * 86_400),
        q.limit.unwrap_or(15),
    )
    .await
    .map(Json)
    .map_err(an_error)
}

async fn an_categories(State(app): State<App>, Query(q): Query<AnalyticsQuery>) -> AnResult {
    an_ready(&app)?;
    aq::categories(&app.state.analytics, q.secs(7 * 86_400))
        .await
        .map(Json)
        .map_err(an_error)
}

async fn an_heatmap(State(app): State<App>, Query(q): Query<AnalyticsQuery>) -> AnResult {
    an_ready(&app)?;
    aq::heatmap(&app.state.analytics, q.secs(30 * 86_400), q.tz.unwrap_or(0))
        .await
        .map(Json)
        .map_err(an_error)
}

async fn an_size_curve(State(app): State<App>, Query(q): Query<AnalyticsQuery>) -> AnResult {
    an_ready(&app)?;
    aq::size_curve(
        &app.state.analytics,
        q.secs(30 * 86_400),
        q.strategy.as_deref(),
    )
    .await
    .map(Json)
    .map_err(an_error)
}

async fn an_liquidity(State(app): State<App>, Query(q): Query<AnalyticsQuery>) -> AnResult {
    an_ready(&app)?;
    aq::liquidity(
        &app.state.analytics,
        q.secs(7 * 86_400),
        q.limit.unwrap_or(15),
    )
    .await
    .map(Json)
    .map_err(an_error)
}

async fn an_executions(State(app): State<App>, Query(q): Query<AnalyticsQuery>) -> AnResult {
    an_ready(&app)?;
    aq::executions(&app.state.analytics, q.secs(7 * 86_400))
        .await
        .map(Json)
        .map_err(an_error)
}

async fn an_export(
    State(app): State<App>,
    Query(q): Query<AnalyticsQuery>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    an_ready(&app)?;
    let table = q.table.as_deref().unwrap_or("opportunities");
    let csv = aq::export(
        &app.state.analytics,
        table,
        q.secs(7 * 86_400),
        q.limit.unwrap_or(50_000),
    )
    .await
    .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    Ok((
        [(axum::http::header::CONTENT_TYPE, "text/csv; charset=utf-8")],
        csv,
    ))
}

/// Log a leg of a real trade. Nothing in this repo calls it — the project stops
/// at detection — but an execution layer built on these alerts can POST here to
/// put realised slippage, latency, gas and fills next to the signals that
/// produced them, joined on `opportunity_id`.
async fn an_log_execution(
    State(app): State<App>,
    Json(mut row): Json<ExecutionRow>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    an_ready(&app)?;
    row.derive(now_ms());
    app.state
        .analytics
        .record(Row::Execution(Box::new(row.clone())));
    Ok(Json(serde_json::json!({
        "logged": true,
        "opportunity_id": row.opportunity_id,
        "ts": row.ts,
    })))
}

async fn get_config(State(app): State<App>) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "ws_url": app.state.cfg.ws_url,
        "search_limit": app.state.cfg.search_limit,
        "arb": app.state.cfg.arb,
        "category_fee_bps": app.state.cfg.category_fee_bps,
    }))
}
