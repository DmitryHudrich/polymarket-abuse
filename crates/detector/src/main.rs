//! Websocket-first, alerts-only backend. It streams Polymarket order books for
//! a user-curated watchlist, runs the YES/NO arbitrage math on every update, and
//! exposes opportunities plus watchlist management over REST. It never places,
//! signs or funds any order.

mod config;
mod gamma;
mod state;
mod ws;

use std::sync::atomic::Ordering;
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{delete, get};
use axum::{Json, Router};
use serde::Deserialize;

use crate::gamma::Gamma;
use crate::state::{now, AppState};

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
        .route("/watch", get(list_watch).post(add_watch))
        .route("/watch/:key", delete(remove_watch))
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
        if app.state.add_market(m).await {
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
    match app.state.remove_market(&key).await {
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

async fn get_config(State(app): State<App>) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "ws_url": app.state.cfg.ws_url,
        "search_limit": app.state.cfg.search_limit,
        "arb": app.state.cfg.arb,
    }))
}
