//! Polymarket CLOB market websocket client. Maintains live order books for the
//! watched tokens and re-runs the arbitrage math on every update. On watchlist
//! changes it reconnects with the new asset set.

use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

use crate::state::AppState;

pub async fn run(state: AppState) {
    let mut backoff = 1u64;
    loop {
        let tokens = state.watched_tokens().await;
        if tokens.is_empty() {
            state.ws_connected.store(false, Ordering::Relaxed);
            // Nothing to watch yet; wait until something is added.
            state.resubscribe.notified().await;
            continue;
        }

        match stream(&state, tokens).await {
            Ok(()) => backoff = 1,
            Err(e) => {
                state.ws_connected.store(false, Ordering::Relaxed);
                tracing::warn!("websocket dropped: {e}; reconnecting in {backoff}s");
                tokio::time::sleep(Duration::from_secs(backoff)).await;
                backoff = (backoff * 2).min(30);
            }
        }
    }
}

/// Connect, subscribe, and pump messages until an error or a watchlist change.
async fn stream(state: &AppState, tokens: Vec<String>) -> Result<()> {
    let (ws, _) = connect_async(&state.cfg.ws_url)
        .await
        .context("connect failed")?;
    let (mut write, mut read) = ws.split();

    let sub = serde_json::json!({ "assets_ids": tokens, "type": "market" });
    write
        .send(Message::Text(sub.to_string()))
        .await
        .context("subscribe failed")?;
    state.ws_connected.store(true, Ordering::Relaxed);
    tracing::info!(
        "subscribed to {} tokens",
        state.watched_tokens().await.len()
    );

    let mut ping = tokio::time::interval(Duration::from_secs(10));
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            _ = state.resubscribe.notified() => {
                let _ = write.send(Message::Close(None)).await;
                return Ok(());
            }
            _ = ping.tick() => {
                write.send(Message::Ping(Vec::new())).await.context("ping failed")?;
            }
            msg = read.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => handle_text(state, &text).await,
                    Some(Ok(Message::Ping(p))) => write.send(Message::Pong(p)).await.context("pong failed")?,
                    Some(Ok(Message::Close(_))) | None => {
                        anyhow::bail!("connection closed by server");
                    }
                    Some(Ok(_)) => {}
                    Some(Err(e)) => return Err(e).context("read error"),
                }
            }
        }
    }
}

async fn handle_text(state: &AppState, text: &str) {
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        return;
    };
    match value {
        Value::Array(items) => {
            for item in items {
                dispatch(state, item).await;
            }
        }
        other => dispatch(state, other).await,
    }
}

async fn dispatch(state: &AppState, item: Value) {
    match item.get("event_type").and_then(Value::as_str) {
        Some("book") => apply_book(state, &item).await,
        Some("price_change") => apply_price_change(state, &item).await,
        _ => {}
    }
}

async fn apply_book(state: &AppState, item: &Value) {
    let Some(asset_id) = item.get("asset_id").and_then(Value::as_str) else {
        return;
    };
    let bids = parse_levels(item.get("bids"));
    let asks = parse_levels(item.get("asks"));

    {
        let mut books = state.books.write().await;
        books
            .entry(asset_id.to_string())
            .or_default()
            .replace(bids, asks);
    }
    recompute_token(state, asset_id).await;
}

async fn apply_price_change(state: &AppState, item: &Value) {
    let Some(changes) = item.get("price_changes").and_then(Value::as_array) else {
        return;
    };

    let mut touched: Vec<String> = Vec::new();
    {
        let mut books = state.books.write().await;
        for ch in changes {
            let (Some(asset_id), Some(price), Some(size), Some(side)) = (
                ch.get("asset_id").and_then(Value::as_str),
                ch.get("price").and_then(Value::as_str),
                ch.get("size")
                    .and_then(Value::as_str)
                    .and_then(|s| s.parse::<f64>().ok()),
                ch.get("side").and_then(Value::as_str),
            ) else {
                continue;
            };
            if let Some(book) = books.get_mut(asset_id) {
                book.set(side.eq_ignore_ascii_case("BUY"), price.to_string(), size);
            }
            touched.push(asset_id.to_string());
        }
    }

    for cond in state.conditions_for_tokens(&touched).await {
        state.recompute(&cond).await;
    }
}

async fn recompute_token(state: &AppState, asset_id: &str) {
    let cond = state.token_index.read().await.get(asset_id).cloned();
    if let Some(cond) = cond {
        state.recompute(&cond).await;
    }
}

fn parse_levels(v: Option<&Value>) -> Vec<(String, f64)> {
    let Some(arr) = v.and_then(Value::as_array) else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|l| {
            let price = l.get("price")?.as_str()?.to_string();
            let size = l.get("size")?.as_str()?.parse::<f64>().ok()?;
            Some((price, size))
        })
        .collect()
}
