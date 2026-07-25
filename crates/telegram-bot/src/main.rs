//! Standalone Telegram alerter. It polls the detector REST API and pushes
//! "arbitrage available" messages to a chat, and answers a few read-only
//! commands. It talks only to the backend and the Telegram Bot API — it has no
//! access to Polymarket or to any funds.

mod config;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, RwLock};

use crate::config::Config;

/// Cap on retained pending (unacknowledged) batches and archived items.
const ARCHIVE_CAP: usize = 1000;

/// Target size for one chat message. Telegram's hard limit is 4096 characters,
/// so a digest longer than this is split across several messages.
const MESSAGE_CHARS: usize = 3_500;

/// How many recent micro-crossings `/crossings` shows in one digest.
const CROSSINGS_SHOWN: usize = 25;

/// A signal the user has acknowledged with the "seen" button, kept for `/archive`.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ArchivedItem {
    kind: String,
    text: String,
    created_at: u64,
}

impl ArchivedItem {
    fn new(kind: &str, text: String) -> Self {
        Self {
            kind: kind.to_string(),
            text,
            created_at: now(),
        }
    }
}

/// A batch parked behind a ✅ button: the items it files, plus the HTML of the
/// message the button sits on — `callback_query.message.text` comes back as
/// plain text, so editing from it would strip all formatting.
struct PendingBatch {
    items: Vec<ArchivedItem>,
    message_html: String,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn default_true() -> bool {
    true
}

/// Runtime-tunable alert filters, changed from chat via `/set` and persisted.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Settings {
    enabled: bool,
    only_profitable: bool,
    min_edge: f64,
    min_profit: f64,
    /// Push a message each time a micro-crossing closes, with how long it lived.
    #[serde(default)]
    crossings: bool,
    /// Alert on buy-set arbs: `ask(YES) + ask(NO) < 1`, merge the set for $1.
    #[serde(default = "default_true")]
    mint: bool,
    /// Alert on split-and-sell arbs: split $1 into a set, sell both legs into
    /// bids summing above $1.
    #[serde(default = "default_true")]
    split: bool,
}

impl Settings {
    fn from_config(cfg: &Config) -> Self {
        Self {
            enabled: true,
            only_profitable: cfg.only_profitable,
            min_edge: cfg.alert_min_edge,
            min_profit: cfg.alert_min_profit,
            crossings: false,
            mint: true,
            split: true,
        }
    }

    /// Whether alerts for this strategy are switched on.
    fn allows(&self, strategy: &str) -> bool {
        match strategy {
            "split" => self.split,
            _ => self.mint,
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
struct Crossing {
    question: String,
    market_url: String,
    #[serde(default = "mint_strategy")]
    strategy: String,
    lived_ms: u64,
    peak_avg_edge: f64,
    peak_net: f64,
    peak_size: f64,
    #[serde(default)]
    opened_at: u64,
}

fn mint_strategy() -> String {
    "mint".to_string()
}

#[derive(Debug, Deserialize, Default)]
struct CrossingsResponse {
    #[serde(default)]
    last_seq: u64,
    #[serde(default)]
    crossings: Vec<Crossing>,
}

#[derive(Debug, Deserialize, Clone)]
struct Arb {
    #[serde(default = "mint_strategy")]
    strategy: String,
    best_yes: f64,
    best_no: f64,
    sum: f64,
    gross_edge: f64,
    size: f64,
    cost: f64,
    #[serde(default)]
    payout: f64,
    fees: f64,
    gas: f64,
    net_profit: f64,
    roi: f64,
    #[serde(default)]
    avg_edge: f64,
    profitable: bool,
}

#[derive(Debug, Deserialize, Clone)]
struct Opportunity {
    question: String,
    condition_id: String,
    volume: f64,
    market_url: String,
    /// Trading window from Gamma, ISO 8601 (absent on an older backend).
    #[serde(default)]
    start_date: Option<String>,
    #[serde(default)]
    end_date: Option<String>,
    arb: Arb,
    /// When the backend first saw this occurrence (0 on an older backend).
    #[serde(default)]
    first_seen: u64,
}

#[derive(Debug, Deserialize)]
struct OpportunitiesResponse {
    #[serde(default)]
    opportunities: Vec<Opportunity>,
}

struct Bot {
    http: reqwest::Client,
    updates_http: reqwest::Client,
    cfg: Config,
    api_base: String,
    settings: RwLock<Settings>,
    settings_path: PathBuf,
    archive_path: PathBuf,
    /// Batches shown with a "seen" button, keyed by the id in their
    /// callback_data. One press files the whole batch.
    pending: RwLock<HashMap<u64, PendingBatch>>,
    archive: RwLock<Vec<ArchivedItem>>,
    item_seq: AtomicU64,
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    let cfg = config::load()?;

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| cfg.log_level.clone().into()),
        )
        .init();

    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(cfg.http_timeout_secs))
        .build()?;
    let updates_http = reqwest::Client::builder()
        .timeout(Duration::from_secs(cfg.http_timeout_secs + 40))
        .build()?;

    let api_base = format!("https://api.telegram.org/bot{}", cfg.bot_token);
    let settings_path = cfg.settings_file.clone();
    let settings = load_settings(&settings_path, &cfg);
    let archive_path = cfg.archive_file.clone();
    let archive = load_archive(&archive_path);
    let bot = Arc::new(Bot {
        http,
        updates_http,
        api_base,
        settings: RwLock::new(settings),
        settings_path,
        archive_path,
        pending: RwLock::new(HashMap::new()),
        archive: RwLock::new(archive),
        item_seq: AtomicU64::new(0),
        cfg,
    });

    tracing::info!(backend = %bot.cfg.backend_url, "telegram alerter started");

    let alerter = tokio::spawn(alert_loop(bot.clone()));
    let crossings = tokio::spawn(crossings_loop(bot.clone()));
    let commands = tokio::spawn(command_loop(bot.clone()));

    let _ = tokio::try_join!(alerter, crossings, commands)?;
    Ok(())
}

async fn alert_loop(bot: Arc<Bot>) {
    if bot.cfg.chat_id.is_empty() {
        tracing::warn!("chat_id is empty; alerts disabled, commands still work");
        return;
    }
    let seen: Mutex<HashMap<String, f64>> = Mutex::new(HashMap::new());
    let mut ticker = tokio::time::interval(Duration::from_secs(bot.cfg.poll_interval_secs.max(1)));

    loop {
        ticker.tick().await;
        let s = bot.settings.read().await.clone();
        if !s.enabled {
            continue;
        }
        let resp = match bot.fetch_opportunities().await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("backend poll failed: {e}");
                continue;
            }
        };

        let mut seen = seen.lock().await;
        for o in &resp.opportunities {
            if !s.allows(&o.arb.strategy) {
                continue;
            }
            if s.only_profitable && !o.arb.profitable {
                continue;
            }
            if o.arb.gross_edge < s.min_edge || o.arb.net_profit < s.min_profit {
                continue;
            }
            // Both directions can be live on the same market, so the re-alert
            // memory is keyed per strategy, not per market.
            let key = format!("{}:{}", o.condition_id, o.arb.strategy);
            let fresh = match seen.get(&key).copied() {
                None => true,
                Some(p) => o.arb.gross_edge - p >= bot.cfg.realert_improvement,
            };
            if !fresh {
                continue;
            }
            let text = format_alert(o, bot.cfg.tz_offset_minutes);
            if let Err(e) = bot.send(&bot.cfg.chat_id, &text).await {
                tracing::warn!("send alert failed: {e}");
            } else {
                seen.insert(key, o.arb.gross_edge);
            }
        }
    }
}

async fn crossings_loop(bot: Arc<Bot>) {
    if bot.cfg.chat_id.is_empty() {
        return;
    }
    let mut last_seq = u64::MAX; // MAX = uninitialized; first poll just syncs the cursor
    let mut ticker = tokio::time::interval(Duration::from_secs(bot.cfg.poll_interval_secs.max(1)));
    loop {
        ticker.tick().await;
        let after = if last_seq == u64::MAX { 0 } else { last_seq };
        let resp = match bot.fetch_crossings(after, 200).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("crossings poll failed: {e}");
                continue;
            }
        };
        let first_run = last_seq == u64::MAX;
        last_seq = resp.last_seq;
        if first_run {
            continue; // don't replay the backlog that existed before we started
        }
        let s = bot.settings.read().await.clone();
        if !s.enabled || !s.crossings {
            continue;
        }
        // Crossings close in bursts, so everything this poll picked up goes out
        // as one digest behind a single ✅ button instead of a message each.
        let items: Vec<ArchivedItem> = resp
            .crossings
            .iter()
            .rev()
            .filter(|c| s.allows(&c.strategy) && c.peak_avg_edge >= s.min_edge)
            .map(|c| ArchivedItem::new("crossing", format_crossing(c, bot.cfg.tz_offset_minutes)))
            .collect();
        if !items.is_empty() {
            bot.send_digest(&bot.cfg.chat_id, "⚡ <b>Micro-crossings</b>", items, "")
                .await;
        }
    }
}

async fn command_loop(bot: Arc<Bot>) {
    let mut offset: i64 = 0;
    loop {
        let updates = match bot.get_updates(offset).await {
            Ok(u) => u,
            Err(e) => {
                tracing::warn!("getUpdates failed: {e}");
                tokio::time::sleep(Duration::from_secs(3)).await;
                continue;
            }
        };
        for u in updates {
            offset = u.update_id + 1;
            if let Some(cq) = u.callback_query {
                bot.handle_callback(cq).await;
                continue;
            }
            let Some(msg) = u.message else { continue };
            let Some(text) = msg.text else { continue };
            let chat_id = msg.chat.id.to_string();
            if let Some(reply) = bot.handle_command(&chat_id, text.trim()).await {
                if let Err(e) = bot.send(&chat_id, &reply).await {
                    tracing::warn!("command reply failed: {e}");
                }
            }
        }
    }
}

impl Bot {
    async fn fetch_opportunities(&self) -> Result<OpportunitiesResponse> {
        let url = format!("{}/opportunities", self.cfg.backend_url);
        let r = self
            .http
            .get(&url)
            .send()
            .await
            .context("backend request failed")?
            .error_for_status()
            .context("backend error status")?
            .json()
            .await
            .context("backend decode failed")?;
        Ok(r)
    }

    async fn fetch_crossings(&self, after_seq: u64, limit: usize) -> Result<CrossingsResponse> {
        let url = format!(
            "{}/crossings?after_seq={after_seq}&limit={limit}",
            self.cfg.backend_url
        );
        let r = self
            .http
            .get(&url)
            .send()
            .await
            .context("crossings request failed")?
            .error_for_status()
            .context("crossings error status")?
            .json()
            .await
            .context("crossings decode failed")?;
        Ok(r)
    }

    /// Send the current opportunities, one message per item with a "seen" button.
    async fn cmd_opportunities(&self, chat_id: &str) {
        let tz = self.cfg.tz_offset_minutes;
        let items: Vec<ArchivedItem> = match self.fetch_opportunities().await {
            Ok(r) => r
                .opportunities
                .iter()
                .take(15)
                .map(|o| ArchivedItem::new("opportunity", format_alert(o, tz)))
                .collect(),
            Err(e) => return self.send_backend_error(chat_id, &e).await,
        };
        self.send_items(chat_id, items, "No live opportunities right now.")
            .await;
    }

    /// Send recent micro-crossings as a single digest with one ✅ button that
    /// files every listed crossing at once — they are short, numerous and only
    /// interesting in aggregate, so one message per crossing just floods the chat.
    async fn cmd_crossings(&self, chat_id: &str) {
        let tz = self.cfg.tz_offset_minutes;
        let items: Vec<ArchivedItem> = match self.fetch_crossings(0, 500).await {
            Ok(r) => r
                .crossings
                .iter()
                .rev()
                .take(CROSSINGS_SHOWN)
                .map(|c| ArchivedItem::new("crossing", format_crossing(c, tz)))
                .collect(),
            Err(e) => return self.send_backend_error(chat_id, &e).await,
        };
        self.send_digest(
            chat_id,
            "⚡ <b>Micro-crossings</b>",
            items,
            "No micro-crossings recorded yet.",
        )
        .await;
    }

    async fn send_backend_error(&self, chat_id: &str, e: &anyhow::Error) {
        let _ = self
            .send(
                chat_id,
                &format!("backend unreachable: {}", esc(&e.to_string())),
            )
            .await;
    }

    /// Send each item as its own message carrying a "seen" button that moves it
    /// to the archive when pressed.
    async fn send_items(&self, chat_id: &str, items: Vec<ArchivedItem>, empty: &str) {
        if items.is_empty() {
            let _ = self.send(chat_id, empty).await;
            return;
        }
        for item in items {
            let text = item.text.clone();
            let id = self.remember_pending(vec![item], text.clone()).await;
            if let Err(e) = self
                .send_with_button(chat_id, &text, "✅ Seen", &format!("seen:{id}"))
                .await
            {
                tracing::warn!("send item failed: {e}");
            }
        }
    }

    /// Send many items as one digest: as few messages as Telegram's size limit
    /// allows, with a single ✅ button on the last one that files the whole batch.
    async fn send_digest(
        &self,
        chat_id: &str,
        header: &str,
        items: Vec<ArchivedItem>,
        empty: &str,
    ) {
        if items.is_empty() {
            let _ = self.send(chat_id, empty).await;
            return;
        }
        let count = items.len();
        let chunks = build_digest(header, &items);

        // The button rides on the last chunk, so that is the message an archive
        // press will edit.
        let id = self
            .remember_pending(items, chunks[chunks.len() - 1].clone())
            .await;
        let last = chunks.len() - 1;
        for (i, text) in chunks.iter().enumerate() {
            let res = if i == last {
                self.send_with_button(
                    chat_id,
                    text,
                    &format!("✅ Seen all ({count})"),
                    &format!("seen:{id}"),
                )
                .await
            } else {
                self.send(chat_id, text).await
            };
            if let Err(e) = res {
                tracing::warn!("send digest failed: {e}");
            }
        }
    }

    /// Park a batch under a fresh callback id, evicting the oldest if we are
    /// holding too many unacknowledged ones.
    async fn remember_pending(&self, items: Vec<ArchivedItem>, message_html: String) -> u64 {
        let id = self.item_seq.fetch_add(1, Ordering::Relaxed) + 1;
        let mut pending = self.pending.write().await;
        pending.insert(
            id,
            PendingBatch {
                items,
                message_html,
            },
        );
        if pending.len() > ARCHIVE_CAP {
            if let Some(&oldest) = pending.keys().min() {
                pending.remove(&oldest);
            }
        }
        id
    }

    async fn cmd_archive(&self) -> String {
        let archive = self.archive.read().await;
        if archive.is_empty() {
            return "Archive is empty. Tap ✅ on a /crossings or /opportunities item to file it."
                .into();
        }
        let tz = self.cfg.tz_offset_minutes;
        let mut s = format!("<b>Archive ({} items, newest first)</b>\n\n", archive.len());
        for item in archive.iter().rev().take(10) {
            s.push_str(&item.text);
            s.push_str(&format!(
                "\n🗂 <i>filed {}</i>\n\n",
                fmt_time(item.created_at, tz)
            ));
        }
        s
    }

    /// Handle a "seen" button press: file the whole batch and update its message.
    async fn handle_callback(&self, cq: CallbackQuery) {
        let id = cq
            .data
            .as_deref()
            .and_then(|d| d.strip_prefix("seen:"))
            .and_then(|s| s.parse::<u64>().ok());

        let batch = match id {
            Some(id) => self.pending.write().await.remove(&id),
            None => None,
        };

        match batch {
            Some(batch) if !batch.items.is_empty() => {
                let count = batch.items.len();
                {
                    let mut archive = self.archive.write().await;
                    archive.extend(batch.items);
                    if archive.len() > ARCHIVE_CAP {
                        let excess = archive.len() - ARCHIVE_CAP;
                        archive.drain(..excess);
                    }
                }
                self.save_archive().await;
                let note = if count == 1 {
                    "Archived".to_string()
                } else {
                    format!("Archived {count} items")
                };
                let _ = self.answer_callback(&cq.id, &note).await;
                if let Some(msg) = cq.message {
                    let text = format!(
                        "{}\n\n🗂 <i>archived {} at {}</i>",
                        batch.message_html,
                        if count == 1 {
                            "1 item".to_string()
                        } else {
                            format!("{count} items")
                        },
                        fmt_time(now(), self.cfg.tz_offset_minutes),
                    );
                    let _ = self.edit_message(msg.chat.id, msg.message_id, &text).await;
                }
            }
            _ => {
                let _ = self
                    .answer_callback(&cq.id, "Expired (bot restarted)")
                    .await;
            }
        }
    }

    async fn save_archive(&self) {
        let snapshot = self.archive.read().await.clone();
        if let Ok(bytes) = serde_json::to_vec_pretty(&snapshot) {
            if let Err(e) = tokio::fs::write(&self.archive_path, bytes).await {
                tracing::warn!("cannot save archive: {e}");
            }
        }
    }

    async fn send_with_button(
        &self,
        chat_id: &str,
        text: &str,
        label: &str,
        callback_data: &str,
    ) -> Result<()> {
        let url = format!("{}/sendMessage", self.api_base);
        let body = serde_json::json!({
            "chat_id": chat_id,
            "text": text,
            "parse_mode": "HTML",
            "disable_web_page_preview": true,
            "reply_markup": { "inline_keyboard": [[{ "text": label, "callback_data": callback_data }]] },
        });
        self.http
            .post(&url)
            .json(&body)
            .send()
            .await
            .context("sendMessage failed")?
            .error_for_status()
            .context("sendMessage error status")?;
        Ok(())
    }

    async fn answer_callback(&self, callback_id: &str, text: &str) -> Result<()> {
        let url = format!("{}/answerCallbackQuery", self.api_base);
        let body = serde_json::json!({ "callback_query_id": callback_id, "text": text });
        self.http.post(&url).json(&body).send().await?;
        Ok(())
    }

    async fn edit_message(&self, chat_id: i64, message_id: i64, text: &str) -> Result<()> {
        let url = format!("{}/editMessageText", self.api_base);
        let body = serde_json::json!({
            "chat_id": chat_id,
            "message_id": message_id,
            "text": text,
            "parse_mode": "HTML",
            "disable_web_page_preview": true,
        });
        self.http.post(&url).json(&body).send().await?;
        Ok(())
    }

    async fn handle_command(&self, chat_id: &str, text: &str) -> Option<String> {
        let mut parts = text.splitn(2, char::is_whitespace);
        let raw = parts.next().unwrap_or("");
        let arg = parts.next().unwrap_or("").trim();
        let cmd = raw.split('@').next().unwrap_or(raw);
        match cmd {
            "/start" | "/help" => Some(format!("{HELP}{}", version_line())),
            "/status" => Some(self.status_text().await),
            "/top" => Some(self.top_text().await),
            "/opportunities" | "/opps" => {
                self.cmd_opportunities(chat_id).await;
                None
            }
            "/crossings" => {
                self.cmd_crossings(chat_id).await;
                None
            }
            "/archive" => Some(self.cmd_archive().await),
            "/add" => Some(self.cmd_add(arg).await),
            "/remove" | "/rm" => Some(self.cmd_remove(arg).await),
            "/list" => Some(self.cmd_list().await),
            "/search" | "/find" => Some(self.cmd_search(arg).await),
            "/add_category" | "/addcategory" => Some(self.cmd_add_category(arg).await),
            "/remove_category" | "/removecategory" => Some(self.cmd_remove_category(arg).await),
            "/categories" => Some(self.cmd_categories().await),
            "/fees" => Some(self.cmd_fees().await),
            "/settings" => Some(self.cmd_settings().await),
            "/set" => Some(self.cmd_set(arg).await),
            "/mute" => Some(self.cmd_toggle(false).await),
            "/unmute" => Some(self.cmd_toggle(true).await),
            _ => match extract_polymarket_url(text) {
                Some(url) => Some(self.cmd_add(&url).await),
                None => None,
            },
        }
    }

    async fn cmd_add_category(&self, input: &str) -> String {
        if input.is_empty() {
            return "Usage: /add_category &lt;category link or slug&gt; (e.g. https://polymarket.com/iran)".into();
        }
        let url = format!("{}/category", self.cfg.backend_url);
        let body = serde_json::json!({ "input": input });
        match self.http.post(&url).json(&body).send().await {
            Ok(r) => {
                let ok = r.status().is_success();
                let text = r.text().await.unwrap_or_default();
                if !ok {
                    return format!("❌ {}", esc(text.trim_matches('"')));
                }
                let v: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
                let label = v["category"]["label"].as_str().unwrap_or(input);
                let watched = v["watched_markets"].as_u64().unwrap_or(0);
                if v["added"].as_bool() == Some(false) {
                    return format!("Already watching category <b>{}</b>.", esc(label));
                }
                format!(
                    "✅ Watching category <b>{}</b> — {watched} markets now tracked and auto-updated.",
                    esc(label)
                )
            }
            Err(e) => format!("backend unreachable: {}", esc(&e.to_string())),
        }
    }

    async fn cmd_remove_category(&self, arg: &str) -> String {
        if arg.is_empty() {
            return "Usage: /remove_category &lt;slug&gt;".into();
        }
        let url = format!("{}/category/{}", self.cfg.backend_url, as_key(arg));
        match self.http.delete(&url).send().await {
            Ok(r) if r.status().is_success() => {
                let v: serde_json::Value = r.json().await.unwrap_or_default();
                format!(
                    "🗑 Removed category: {}",
                    esc(v["label"].as_str().unwrap_or(arg))
                )
            }
            Ok(_) => "No such watched category.".into(),
            Err(e) => format!("backend unreachable: {}", esc(&e.to_string())),
        }
    }

    async fn cmd_categories(&self) -> String {
        let url = format!("{}/categories", self.cfg.backend_url);
        match self.http.get(&url).send().await {
            Ok(r) => {
                let v: serde_json::Value = r.json().await.unwrap_or_default();
                let cats = v["categories"].as_array().cloned().unwrap_or_default();
                if cats.is_empty() {
                    return "No categories watched. Add one with /add_category.".into();
                }
                let mut s = format!("<b>Watched categories ({})</b>\n", cats.len());
                for c in &cats {
                    s.push_str(&format!(
                        "• {} (<code>{}</code>)\n",
                        esc(c["label"].as_str().unwrap_or("")),
                        esc(c["slug"].as_str().unwrap_or("")),
                    ));
                }
                s
            }
            Err(e) => format!("backend unreachable: {}", esc(&e.to_string())),
        }
    }

    async fn cmd_fees(&self) -> String {
        let url = format!("{}/config", self.cfg.backend_url);
        let v: serde_json::Value = match self.http.get(&url).send().await {
            Ok(r) => r.json().await.unwrap_or_default(),
            Err(e) => return format!("backend unreachable: {}", esc(&e.to_string())),
        };
        let mut s = String::from("<b>Configured taker fees</b> (bps per side)\n\n");
        if let Some(map) = v["category_fee_bps"].as_object() {
            let mut rows: Vec<(String, f64)> = map
                .iter()
                .filter_map(|(k, val)| val.as_f64().map(|f| (k.clone(), f)))
                .collect();
            rows.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
            for (cat, bps) in rows {
                s.push_str(&format!(
                    "• {}: <b>{:.0}</b>  (≈ ${:.2}/100 @ 0.50)\n",
                    esc(&cat),
                    bps,
                    bps * 0.005,
                ));
            }
        }
        let global = v["arb"]["fee_bps"].as_f64().unwrap_or(0.0);
        s.push_str(&format!(
            "\nfallback (uncategorised): <b>{global:.0}</b>\n\
             <i>Both legs are takers in either direction (buy YES+NO, or sell both \
             after a split), so a set always pays two taker fees. The fee peaks at \
             price 0.50 and fades toward the tails.</i>"
        ));
        s
    }

    async fn cmd_settings(&self) -> String {
        let s = self.settings.read().await.clone();
        format!(
            "<b>Alert settings</b>\n\
             enabled: <b>{}</b>\n\
             only_profitable: <b>{}</b>\n\
             min_edge: <b>{}</b>\n\
             min_profit: <b>${}</b>\n\
             crossings: <b>{}</b>\n\
             mint (asks &lt; $1): <b>{}</b>\n\
             split (bids &gt; $1): <b>{}</b>\n\n\
             Change with: /set &lt;key&gt; &lt;value&gt;\n\
             keys: enabled, only_profitable, min_edge, min_profit, crossings, mint, split\n\
             or /mute /unmute",
            s.enabled, s.only_profitable, s.min_edge, s.min_profit, s.crossings, s.mint, s.split
        )
    }

    async fn cmd_set(&self, arg: &str) -> String {
        let mut it = arg.split_whitespace();
        let (Some(key), Some(val)) = (it.next(), it.next()) else {
            return format!("Usage: /set &lt;key&gt; &lt;value&gt; (keys: {SET_KEYS})");
        };
        let mut s = self.settings.write().await;
        let result = match key {
            "enabled" => parse_bool(val).map(|b| s.enabled = b),
            "only_profitable" => parse_bool(val).map(|b| s.only_profitable = b),
            "min_edge" => val.parse::<f64>().ok().map(|v| s.min_edge = v),
            "min_profit" => val.parse::<f64>().ok().map(|v| s.min_profit = v),
            "crossings" => parse_bool(val).map(|b| s.crossings = b),
            "mint" => parse_bool(val).map(|b| s.mint = b),
            "split" => parse_bool(val).map(|b| s.split = b),
            _ => return format!("Unknown key '{}'. Keys: {SET_KEYS}", esc(key)),
        };
        if result.is_none() {
            return format!("Bad value '{}' for {}.", esc(val), esc(key));
        }
        let snapshot = s.clone();
        drop(s);
        self.save_settings(&snapshot).await;
        format!("✅ {} = {}", esc(key), esc(val))
    }

    async fn cmd_toggle(&self, enabled: bool) -> String {
        let snapshot = {
            let mut s = self.settings.write().await;
            s.enabled = enabled;
            s.clone()
        };
        self.save_settings(&snapshot).await;
        if enabled {
            "🔔 Alerts on.".into()
        } else {
            "🔕 Alerts muted. /unmute to resume.".into()
        }
    }

    async fn save_settings(&self, s: &Settings) {
        match serde_json::to_vec_pretty(s) {
            Ok(bytes) => {
                if let Err(e) = tokio::fs::write(&self.settings_path, bytes).await {
                    tracing::warn!("cannot save settings: {e}");
                }
            }
            Err(e) => tracing::warn!("cannot serialize settings: {e}"),
        }
    }

    async fn cmd_add(&self, input: &str) -> String {
        if input.is_empty() {
            return "Usage: /add &lt;polymarket link or slug&gt;".into();
        }
        let url = format!("{}/watch", self.cfg.backend_url);
        let body = serde_json::json!({ "input": input });
        match self.http.post(&url).json(&body).send().await {
            Ok(r) => {
                let ok = r.status().is_success();
                let text = r.text().await.unwrap_or_default();
                if !ok {
                    return format!("❌ {}", esc(text.trim_matches('"')));
                }
                let v: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
                let added = v["added"].as_u64().unwrap_or(0);
                if added == 0 {
                    return "Already watching this market.".into();
                }
                let mut s = format!("✅ Watching {added} market(s):\n");
                for m in v["markets"].as_array().into_iter().flatten() {
                    s.push_str(&format!(
                        "• {}\n",
                        esc(m["question"].as_str().unwrap_or(""))
                    ));
                }
                s
            }
            Err(e) => format!("backend unreachable: {}", esc(&e.to_string())),
        }
    }

    async fn cmd_remove(&self, arg: &str) -> String {
        if arg.is_empty() {
            return "Usage: /remove &lt;slug or condition_id&gt;".into();
        }
        let url = format!("{}/watch/{}", self.cfg.backend_url, as_key(arg));
        match self.http.delete(&url).send().await {
            Ok(r) if r.status().is_success() => {
                let v: serde_json::Value = r.json().await.unwrap_or_default();
                format!("🗑 Removed: {}", esc(v["question"].as_str().unwrap_or(arg)))
            }
            Ok(_) => "Not in the watchlist.".into(),
            Err(e) => format!("backend unreachable: {}", esc(&e.to_string())),
        }
    }

    async fn cmd_list(&self) -> String {
        let url = format!("{}/watch", self.cfg.backend_url);
        match self.http.get(&url).send().await {
            Ok(r) => {
                let v: serde_json::Value = r.json().await.unwrap_or_default();
                let markets = v["markets"].as_array().cloned().unwrap_or_default();
                if markets.is_empty() {
                    return "Watchlist is empty. Add one with /add or /search.".into();
                }
                let tz = self.cfg.tz_offset_minutes;
                let mut s = format!("<b>Watching {} market(s)</b>\n", markets.len());
                for m in &markets {
                    s.push_str(&format!(
                        "• {}\n  <code>{}</code>{}\n",
                        esc(m["question"].as_str().unwrap_or("")),
                        esc(m["slug"].as_str().unwrap_or("")),
                        fmt_window_short(m, tz),
                    ));
                }
                s
            }
            Err(e) => format!("backend unreachable: {}", esc(&e.to_string())),
        }
    }

    async fn cmd_search(&self, query: &str) -> String {
        if query.is_empty() {
            return "Usage: /search &lt;query&gt;".into();
        }
        let url = format!("{}/search?q={}", self.cfg.backend_url, urlencode(query));
        match self.http.get(&url).send().await {
            Ok(r) => {
                let v: serde_json::Value = r.json().await.unwrap_or_default();
                let results = v["results"].as_array().cloned().unwrap_or_default();
                if results.is_empty() {
                    return "Nothing found.".into();
                }
                let tz = self.cfg.tz_offset_minutes;
                let mut s = String::from("<b>Results</b> (tap a slug, then /add it)\n\n");
                for m in &results {
                    s.push_str(&format!(
                        "• {}\n  <code>{}</code>{}\n",
                        esc(m["question"].as_str().unwrap_or("")),
                        esc(m["slug"].as_str().unwrap_or("")),
                        fmt_window_short(m, tz),
                    ));
                }
                s
            }
            Err(e) => format!("backend unreachable: {}", esc(&e.to_string())),
        }
    }

    async fn status_text(&self) -> String {
        let health = self
            .http
            .get(format!("{}/health", self.cfg.backend_url))
            .send()
            .await
            .ok();
        let health = match health {
            Some(r) => r.json::<serde_json::Value>().await.unwrap_or_default(),
            None => return "backend unreachable".into(),
        };
        let cats = self
            .http
            .get(format!("{}/categories", self.cfg.backend_url))
            .send()
            .await
            .ok();
        let cat_count = match cats {
            Some(r) => r.json::<serde_json::Value>().await.unwrap_or_default()["count"]
                .as_u64()
                .unwrap_or(0),
            None => 0,
        };
        format!(
            "<b>Backend status</b>\n\
             ws_connected: <b>{}</b>\n\
             watched markets: <b>{}</b>\n\
             categories: <b>{}</b>\n\
             live mint arbs (asks &lt; $1): <b>{}</b>\n\
             live split arbs (bids &gt; $1): <b>{}</b>",
            health["ws_connected"].as_bool().unwrap_or(false),
            health["watched"].as_u64().unwrap_or(0),
            cat_count,
            health["mint_opportunities"].as_u64().unwrap_or(0),
            health["split_opportunities"].as_u64().unwrap_or(0),
        )
    }

    async fn top_text(&self) -> String {
        match self.fetch_opportunities().await {
            Ok(r) if !r.opportunities.is_empty() => {
                let mut s = String::from("<b>Top opportunities</b>\n\n");
                for o in r.opportunities.iter().take(5) {
                    s.push_str(&format_alert(o, self.cfg.tz_offset_minutes));
                    s.push_str("\n\n");
                }
                s
            }
            Ok(_) => "No profitable opportunities right now.".to_string(),
            Err(e) => format!("backend unreachable: {}", esc(&e.to_string())),
        }
    }

    async fn send(&self, chat_id: &str, text: &str) -> Result<()> {
        let url = format!("{}/sendMessage", self.api_base);
        let body = serde_json::json!({
            "chat_id": chat_id,
            "text": text,
            "parse_mode": "HTML",
            "disable_web_page_preview": true,
        });
        self.http
            .post(&url)
            .json(&body)
            .send()
            .await
            .context("sendMessage failed")?
            .error_for_status()
            .context("sendMessage error status")?;
        Ok(())
    }

    async fn get_updates(&self, offset: i64) -> Result<Vec<Update>> {
        let url = format!("{}/getUpdates", self.api_base);
        let body = serde_json::json!({
            "offset": offset,
            "timeout": 25,
            "allowed_updates": ["message", "callback_query"],
        });
        let resp: TgResponse<Vec<Update>> = self
            .updates_http
            .post(&url)
            .json(&body)
            .send()
            .await
            .context("getUpdates request failed")?
            .json()
            .await
            .context("getUpdates decode failed")?;
        Ok(resp.result.unwrap_or_default())
    }
}

fn format_alert(o: &Opportunity, tz: i64) -> String {
    let a = &o.arb;
    let split = a.strategy == "split";
    // Headline is the volume-weighted edge that actually clears at `size`; the
    // top-of-book edge is shown separately and only as context.
    let (header, plan, book_label) = if split {
        (
            "🔵 <b>Split &amp; sell</b> (bids &gt; $1)",
            format!(
                "plan: split <code>${:.2}</code> into {:.0} sets, sell both legs as taker for <code>${:.2}</code>",
                a.cost, a.size, a.payout
            ),
            "best bids",
        )
    } else {
        (
            "🟢 <b>YES+NO arbitrage</b> (asks &lt; $1)",
            format!(
                "plan: buy {:.0} sets for <code>${:.2}</code>, merge back into <code>${:.2}</code>",
                a.size, a.cost, a.payout
            ),
            "best asks",
        )
    };
    let window = fmt_window(o.start_date.as_deref(), o.end_date.as_deref(), tz, !split);
    format!(
        "{header}\n<b>{q}</b>\n\n\
         effective edge: <b>{avg:.3}</b> per set on <b>{size:.0}</b> sets\n\
         net profit: <b>${net:.2}</b>  roi: <b>{roi:.2}%</b>\n\
         {plan}\n\
         fees: <code>${fees:.2}</code> (2 taker legs)  gas: <code>${gas:.2}</code>\n\
         {book_label}: YES <code>{yes:.4}</code> + NO <code>{no:.4}</code> = <code>{sum:.4}</code> (edge {edge:.4})\n\
         volume: <code>${vol:.0}</code>{spotted}{window}\n\
         <a href=\"{url}\">open market</a>",
        header = header,
        q = esc(&o.question),
        avg = a.avg_edge,
        size = a.size,
        net = a.net_profit,
        roi = a.roi * 100.0,
        plan = plan,
        fees = a.fees,
        gas = a.gas,
        book_label = book_label,
        yes = a.best_yes,
        no = a.best_no,
        sum = a.sum,
        edge = a.gross_edge,
        vol = o.volume,
        spotted = fmt_spotted(o.first_seen, tz),
        window = window,
        url = esc(&o.market_url),
    )
}

fn format_crossing(c: &Crossing, tz: i64) -> String {
    let direction = if c.strategy == "split" {
        "split &amp; sell"
    } else {
        "buy set"
    };
    format!(
        "⚡ <b>{q}</b> ({direction})\n\
         spotted <b>{at}</b>, lived <b>{ms} ms</b>, peak edge <b>{edge:.3}</b>\n\
         size <code>{size:.0}</code>  would-be net <code>${net:.2}</code>  \
         <a href=\"{url}\">open market</a>",
        q = esc(&c.question),
        direction = direction,
        at = fmt_time(c.opened_at, tz),
        ms = c.lived_ms,
        edge = c.peak_avg_edge,
        size = c.peak_size,
        net = c.peak_net,
        url = esc(&c.market_url),
    )
}

/// Lay a digest out as one headed message, split into as few chunks as
/// `MESSAGE_CHARS` allows. An item is never split across two messages, so a
/// single oversized item just gets a chunk of its own.
fn build_digest(header: &str, items: &[ArchivedItem]) -> Vec<String> {
    let mut chunks: Vec<String> = Vec::new();
    let mut cur = format!("{header} ({}, newest first)\n\n", items.len());
    let mut in_chunk = 0usize;
    for item in items {
        if in_chunk > 0 && cur.chars().count() + item.text.chars().count() + 2 > MESSAGE_CHARS {
            chunks.push(std::mem::take(&mut cur));
            in_chunk = 0;
        }
        cur.push_str(&item.text);
        cur.push_str("\n\n");
        in_chunk += 1;
    }
    chunks.push(cur);
    for c in chunks.iter_mut() {
        while c.ends_with('\n') {
            c.pop();
        }
    }
    chunks
}

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// Civil `(year, month, day)` from days since the epoch — Howard Hinnant's
/// `civil_from_days`, so timestamps can be rendered without a date crate.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

/// Days since the epoch for a civil date — Hinnant's `days_from_civil`, the
/// inverse of [`civil_from_days`], used to turn Gamma's ISO dates into unix time.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 } as i64;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Unix seconds from an ISO 8601 timestamp as Gamma sends it
/// (`2026-11-03T12:00:00Z`, sometimes date-only or with a `±HH:MM` zone).
/// Fractional seconds are ignored; anything unparseable yields `None`.
fn parse_iso8601(s: &str) -> Option<u64> {
    let s = s.trim();
    let b = s.as_bytes();
    if b.len() < 10 {
        return None;
    }
    let y: i64 = s.get(0..4)?.parse().ok()?;
    let m: u32 = s.get(5..7)?.parse().ok()?;
    let d: u32 = s.get(8..10)?.parse().ok()?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }

    let mut secs = days_from_civil(y, m, d) * 86_400;
    if b.len() >= 16 && (b[10] == b'T' || b[10] == b' ') {
        secs += s.get(11..13)?.parse::<i64>().ok()? * 3_600;
        secs += s.get(14..16)?.parse::<i64>().ok()? * 60;
        if b.len() >= 19 && b[16] == b':' {
            secs += s.get(17..19)?.parse::<i64>().unwrap_or(0);
        }
        // A trailing zone offset, if any, is what the wall clock was shifted by.
        if let Some(i) = s.rfind(['+', '-']).filter(|i| *i >= 11) {
            let z = &s[i + 1..];
            let zh: i64 = z.get(0..2).and_then(|v| v.parse().ok()).unwrap_or(0);
            let zm: i64 = z.get(3..5).and_then(|v| v.parse().ok()).unwrap_or(0);
            let sign = if b[i] == b'-' { 1 } else { -1 };
            secs += sign * (zh * 3_600 + zm * 60);
        }
    }
    u64::try_from(secs).ok()
}

/// Wall-clock `(year, month, day, hour, minute, second)` at the configured UTC offset.
fn wall_clock(ts: u64, offset_minutes: i64) -> (i64, u32, u32, u32, u32, u32) {
    let local = ts as i64 + offset_minutes * 60;
    let (y, m, d) = civil_from_days(local.div_euclid(86_400));
    let secs = local.rem_euclid(86_400);
    (
        y,
        m,
        d,
        (secs / 3_600) as u32,
        (secs % 3_600 / 60) as u32,
        (secs % 60) as u32,
    )
}

/// `UTC`, `UTC+3`, `UTC-5:30` — appended so a bare clock time is never ambiguous.
fn tz_label(offset_minutes: i64) -> String {
    if offset_minutes == 0 {
        return "UTC".into();
    }
    let sign = if offset_minutes < 0 { '-' } else { '+' };
    let (h, m) = (offset_minutes.abs() / 60, offset_minutes.abs() % 60);
    if m == 0 {
        format!("UTC{sign}{h}")
    } else {
        format!("UTC{sign}{h}:{m:02}")
    }
}

/// A timestamp for chat: `16:23:41 UTC+3` today, `25 Jul 16:23:41 UTC+3` otherwise,
/// so live alerts stay short while archived ones stay unambiguous.
fn fmt_time(ts: u64, offset_minutes: i64) -> String {
    if ts == 0 {
        return "unknown".into();
    }
    let (y, m, d, hh, mm, ss) = wall_clock(ts, offset_minutes);
    let (ny, nm, nd, ..) = wall_clock(now(), offset_minutes);
    let tz = tz_label(offset_minutes);
    let month = MONTHS[(m as usize - 1).min(11)];
    if (y, m, d) == (ny, nm, nd) {
        format!("{hh:02}:{mm:02}:{ss:02} {tz}")
    } else {
        format!("{d} {month} {hh:02}:{mm:02}:{ss:02} {tz}")
    }
}

/// Coarse "how long ago", e.g. `4s`, `3m 20s`, `2h 05m`, `1d 3h`.
fn fmt_age(secs: u64) -> String {
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3_600 => format!("{}m {:02}s", s / 60, s % 60),
        s if s < 86_400 => format!("{}h {:02}m", s / 3_600, s % 3_600 / 60),
        s => format!("{}d {}h", s / 86_400, s % 86_400 / 3_600),
    }
}

/// A market date for chat: `3 Nov 2026 12:00 UTC+3`, or `3 Nov 2026` when
/// `with_clock` is false. Unparseable input is passed through as-is.
fn fmt_market_date(iso: &str, tz: i64, with_clock: bool) -> String {
    let Some(ts) = parse_iso8601(iso) else {
        return esc(iso);
    };
    let (y, m, d, hh, mm, _) = wall_clock(ts, tz);
    let month = MONTHS[(m as usize - 1).min(11)];
    if with_clock {
        format!("{d} {month} {y} {hh:02}:{mm:02} {}", tz_label(tz))
    } else {
        format!("{d} {month} {y}")
    }
}

/// How far a market date is from now: `in 3d 4h`, or `2h 10m ago` once passed.
fn fmt_relative(iso: &str) -> String {
    let Some(ts) = parse_iso8601(iso) else {
        return String::new();
    };
    let now = now();
    if ts >= now {
        format!(" (in {})", fmt_age(ts - now))
    } else {
        format!(" ({} ago)", fmt_age(now - ts))
    }
}

/// The market's trading window. `merge_note` adds the buy-and-hold caveat, which
/// only applies to the mint direction — a split closes the same instant it opens.
fn fmt_window(start: Option<&str>, end: Option<&str>, tz: i64, merge_note: bool) -> String {
    let mut s = String::new();
    if let Some(start) = start.filter(|d| !d.is_empty()) {
        s.push_str(&format!(
            "\nmarket opened: <b>{}</b>",
            fmt_market_date(start, tz, true)
        ));
    }
    if let Some(end) = end.filter(|d| !d.is_empty()) {
        s.push_str(&format!(
            "\nmarket closes: <b>{}</b>{}{}",
            fmt_market_date(end, tz, true),
            fmt_relative(end),
            if merge_note {
                " — held until then unless you merge"
            } else {
                ""
            },
        ));
    }
    s
}

/// One-line trading window for market listings: ` · 12 Jan 2026 → 3 Nov 2026`.
/// Empty when the backend reports neither date.
fn fmt_window_short(m: &serde_json::Value, tz: i64) -> String {
    let day = |k: &str| {
        m[k].as_str()
            .filter(|d| !d.is_empty())
            .map(|d| fmt_market_date(d, tz, false))
    };
    match (day("start_date"), day("end_date")) {
        (Some(a), Some(b)) => format!(" · {a} → {b}"),
        (Some(a), None) => format!(" · opened {a}"),
        (None, Some(b)) => format!(" · closes {b}"),
        (None, None) => String::new(),
    }
}

/// "spotted: <time> (<age> ago)" — how long the opportunity has been standing.
fn fmt_spotted(ts: u64, offset_minutes: i64) -> String {
    if ts == 0 {
        return String::new();
    }
    format!(
        "\nspotted: <b>{}</b> ({} ago)",
        fmt_time(ts, offset_minutes),
        fmt_age(now().saturating_sub(ts))
    )
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// First whitespace-separated token that looks like a Polymarket link.
fn extract_polymarket_url(text: &str) -> Option<String> {
    text.split_whitespace()
        .find(|t| t.contains("polymarket.com"))
        .map(|t| t.to_string())
}

/// Reduce a link/slug/condition_id to the path key the backend expects.
fn as_key(arg: &str) -> String {
    let base = arg.split('?').next().unwrap_or(arg).trim_end_matches('/');
    base.rsplit('/').next().unwrap_or(base).to_string()
}

fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Short build identity for `/help`: first and last 4 chars of the commit hash.
fn version_line() -> String {
    let h = env!("GIT_HASH");
    let short = if h.len() >= 8 {
        format!("{}…{}", &h[..4], &h[h.len() - 4..])
    } else {
        h.to_string()
    };
    format!("\n\n<i>build {short}</i>")
}

fn parse_bool(v: &str) -> Option<bool> {
    match v.to_ascii_lowercase().as_str() {
        "true" | "on" | "yes" | "1" => Some(true),
        "false" | "off" | "no" | "0" => Some(false),
        _ => None,
    }
}

/// Load persisted settings, falling back to config defaults if absent/invalid.
fn load_settings(path: &std::path::Path, cfg: &Config) -> Settings {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|_| Settings::from_config(cfg)),
        Err(_) => Settings::from_config(cfg),
    }
}

fn load_archive(path: &std::path::Path) -> Vec<ArchivedItem> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

/// Keys accepted by `/set`, kept in one place so usage and errors agree.
const SET_KEYS: &str = "enabled, only_profitable, min_edge, min_profit, crossings, mint, split";

const HELP: &str = "<b>Polymarket YES/NO arbitrage alerter</b>\n\n\
Two directions, both alert-only — this bot never places orders:\n\
🟢 <b>mint</b> — asks sum below $1: buy YES+NO, merge the set back into $1\n\
🔵 <b>split</b> — bids sum above $1: split $1 into a set, sell both legs\n\
Both legs are takers in either direction, so both taker fees and CTF gas are \
already subtracted from the net profit shown.\n\n\
<b>Categories</b> (auto-tracked, refreshed every few minutes)\n\
/add_category &lt;link|slug&gt; — track a whole category (e.g. polymarket.com/iran)\n\
/remove_category &lt;slug&gt; — stop tracking it\n\
/categories — list tracked categories\n\
/fees — configured taker fees per category\n\n\
<b>Single markets</b>\n\
Paste a Polymarket link, or:\n\
/add &lt;link|slug&gt; · /remove &lt;slug&gt; · /list · /search &lt;query&gt;\n\n\
<b>Notifications</b>\n\
/settings — show alert filters\n\
/set &lt;key&gt; &lt;value&gt; — keys: enabled, only_profitable, min_edge, min_profit, crossings, mint, split\n\
/mute · /unmute\n\n\
<b>Info</b>\n\
/top — quick summary of current opportunities\n\
/opportunities — each opportunity as its own message with a ✅ Seen button\n\
/crossings — recent micro-crossings as one digest with a single ✅ Seen all button\n\
/archive — items you marked as seen\n\
/status — backend health";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_dates_match_known_timestamps() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        // 2026-07-25T16:23:41Z = 1784996621
        assert_eq!(wall_clock(1_784_996_621, 0), (2026, 7, 25, 16, 23, 41));
        // Leap day, and the offset rolling the date backwards over midnight.
        assert_eq!(wall_clock(1_709_164_800, 0), (2024, 2, 29, 0, 0, 0));
        assert_eq!(wall_clock(1_709_164_800, -180), (2024, 2, 28, 21, 0, 0));
        // Fractional offset (UTC+5:30).
        assert_eq!(wall_clock(1_784_996_621, 330), (2026, 7, 25, 21, 53, 41));
    }

    #[test]
    fn tz_labels_read_the_way_people_write_them() {
        assert_eq!(tz_label(0), "UTC");
        assert_eq!(tz_label(180), "UTC+3");
        assert_eq!(tz_label(-300), "UTC-5");
        assert_eq!(tz_label(330), "UTC+5:30");
    }

    #[test]
    fn ages_stay_short() {
        assert_eq!(fmt_age(4), "4s");
        assert_eq!(fmt_age(200), "3m 20s");
        assert_eq!(fmt_age(7_500), "2h 05m");
        assert_eq!(fmt_age(97_200), "1d 3h");
    }

    #[test]
    fn iso_timestamps_round_trip_through_civil_days() {
        assert_eq!(parse_iso8601("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_iso8601("2026-07-25T16:23:41Z"), Some(1_784_996_621));
        // Date-only, fractional seconds, and a non-UTC zone that shifts the instant.
        assert_eq!(parse_iso8601("2024-02-29"), Some(1_709_164_800));
        assert_eq!(
            parse_iso8601("2026-07-25T16:23:41.123456Z"),
            Some(1_784_996_621)
        );
        assert_eq!(
            parse_iso8601("2026-07-25T19:23:41+03:00"),
            Some(1_784_996_621)
        );
        assert_eq!(
            parse_iso8601("2026-07-25T13:23:41-03:00"),
            Some(1_784_996_621)
        );
        // Every day of a leap year survives the round trip.
        for day in 0..366 {
            let ts = 1_704_067_200 + day * 86_400; // from 2024-01-01
            let (y, m, d, ..) = wall_clock(ts, 0);
            assert_eq!(days_from_civil(y, m, d) * 86_400, ts as i64);
        }
    }

    #[test]
    fn market_dates_render_in_the_configured_zone() {
        assert_eq!(
            fmt_market_date("2026-07-25T16:23:41Z", 180, true),
            "25 Jul 2026 19:23 UTC+3"
        );
        assert_eq!(
            fmt_market_date("2026-07-25T16:23:41Z", 0, false),
            "25 Jul 2026"
        );
        // An offset can roll the date over, and garbage is passed through escaped.
        assert_eq!(
            fmt_market_date("2026-07-25T23:30:00Z", 60, false),
            "26 Jul 2026"
        );
        assert_eq!(fmt_market_date("not a date", 0, true), "not a date");
        assert_eq!(fmt_market_date("<b>x</b>", 0, true), "&lt;b&gt;x&lt;/b&gt;");
    }

    #[test]
    fn window_lines_adapt_to_missing_dates() {
        let full = fmt_window(
            Some("2026-01-12T09:00:00Z"),
            Some("2026-11-03T12:00:00Z"),
            0,
            true,
        );
        assert!(full.contains("market opened: <b>12 Jan 2026 09:00 UTC</b>"));
        assert!(full.contains("market closes: <b>3 Nov 2026 12:00 UTC</b>"));
        assert!(full.contains("held until then unless you merge"));

        // The split direction gets no merge caveat, and empty strings are skipped.
        let split = fmt_window(Some(""), Some("2026-11-03T12:00:00Z"), 0, false);
        assert!(!split.contains("market opened"));
        assert!(!split.contains("unless you merge"));
        assert!(fmt_window(None, None, 0, true).is_empty());
    }

    #[test]
    fn digest_fits_telegram_and_keeps_every_item() {
        let items: Vec<ArchivedItem> = (0..40)
            .map(|i| {
                ArchivedItem::new("crossing", format!("⚡ crossing #{i}\n{}", "x".repeat(240)))
            })
            .collect();
        let chunks = build_digest("⚡ <b>Micro-crossings</b>", &items);

        assert!(chunks.len() > 1, "40 items must not fit in one message");
        for c in &chunks {
            assert!(c.chars().count() <= 4_096, "chunk exceeds Telegram's limit");
            assert!(!c.ends_with('\n'), "trailing blank lines were trimmed");
        }
        let joined = chunks.join("\n");
        for item in &items {
            assert!(joined.contains(&item.text), "item dropped from the digest");
        }
        assert!(chunks[0].starts_with("⚡ <b>Micro-crossings</b> (40, newest first)"));
    }

    #[test]
    fn single_item_digest_is_one_message() {
        let items = vec![ArchivedItem::new("crossing", "⚡ one".into())];
        let chunks = build_digest("⚡ <b>Micro-crossings</b>", &items);
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].ends_with("⚡ one"));
    }

    #[test]
    fn unknown_timestamps_do_not_render_as_1970() {
        // An older backend omits `first_seen`, which serde fills with 0.
        assert_eq!(fmt_time(0, 180), "unknown");
        assert!(fmt_spotted(0, 180).is_empty());
    }
}

#[derive(Debug, Deserialize)]
struct TgResponse<T> {
    #[serde(default)]
    result: Option<T>,
}

#[derive(Debug, Deserialize)]
struct Update {
    update_id: i64,
    message: Option<Message>,
    callback_query: Option<CallbackQuery>,
}

#[derive(Debug, Deserialize)]
struct Message {
    chat: Chat,
    #[serde(default)]
    message_id: i64,
    text: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Chat {
    id: i64,
}

#[derive(Debug, Deserialize)]
struct CallbackQuery {
    id: String,
    #[serde(default)]
    data: Option<String>,
    #[serde(default)]
    message: Option<Message>,
}
