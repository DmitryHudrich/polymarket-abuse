//! Websocket-first, alerts-only backend. It streams Polymarket order books for
//! a user-curated watchlist, runs the YES/NO arbitrage math on every update, and
//! exposes opportunities plus watchlist management over REST. It never places,
//! signs or funds any order.

mod config;
mod gamma;
mod state;
mod ws;

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::Deserialize;

use crate::gamma::Gamma;
use crate::state::{now, AppState, Market};

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
    let state = AppState::new(cfg.clone());
    state.load_persisted().await;

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
    limit: Option<usize>,
}

async fn list_opportunities(
    State(app): State<App>,
    Query(q): Query<ListQuery>,
) -> Json<serde_json::Value> {
    let opps = app.state.opps.read().await;
    let mut items: Vec<_> = opps
        .values()
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
        "last_update": last,
        "age_secs": age,
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
                    // A market in several watched categories keeps the highest fee.
                    let prev = desired.get(&m.condition_id).and_then(|e| e.fee_bps);
                    m.fee_bps = Some(prev.map_or(fee, |p| p.max(fee)));
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

async fn get_config(State(app): State<App>) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "ws_url": app.state.cfg.ws_url,
        "search_limit": app.state.cfg.search_limit,
        "arb": app.state.cfg.arb,
        "category_fee_bps": app.state.cfg.category_fee_bps,
    }))
}
