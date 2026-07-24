//! Standalone Telegram alerter. It polls the detector REST API and pushes
//! "arbitrage available" messages to a chat, and answers a few read-only
//! commands. It talks only to the backend and the Telegram Bot API — it has no
//! access to Polymarket or to any funds.

mod config;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, RwLock};

use crate::config::Config;

/// Runtime-tunable alert filters, changed from chat via `/set` and persisted.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Settings {
    enabled: bool,
    only_profitable: bool,
    min_edge: f64,
    min_profit: f64,
}

impl Settings {
    fn from_config(cfg: &Config) -> Self {
        Self {
            enabled: true,
            only_profitable: cfg.only_profitable,
            min_edge: cfg.alert_min_edge,
            min_profit: cfg.alert_min_profit,
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
struct Arb {
    best_yes: f64,
    best_no: f64,
    sum: f64,
    gross_edge: f64,
    size: f64,
    cost: f64,
    fees: f64,
    gas: f64,
    net_profit: f64,
    roi: f64,
    profitable: bool,
}

#[derive(Debug, Deserialize, Clone)]
struct Opportunity {
    question: String,
    condition_id: String,
    volume: f64,
    market_url: String,
    arb: Arb,
    sell_side_edge: Option<f64>,
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
    let bot = Arc::new(Bot {
        http,
        updates_http,
        api_base,
        settings: RwLock::new(settings),
        settings_path,
        cfg,
    });

    tracing::info!(backend = %bot.cfg.backend_url, "telegram alerter started");

    let alerter = tokio::spawn(alert_loop(bot.clone()));
    let commands = tokio::spawn(command_loop(bot.clone()));

    let _ = tokio::try_join!(alerter, commands)?;
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
            if s.only_profitable && !o.arb.profitable {
                continue;
            }
            if o.arb.gross_edge < s.min_edge || o.arb.net_profit < s.min_profit {
                continue;
            }
            let prev = seen.get(&o.condition_id).copied();
            let fresh = match prev {
                None => true,
                Some(p) => o.arb.gross_edge - p >= bot.cfg.realert_improvement,
            };
            if !fresh {
                continue;
            }
            if let Err(e) = bot.send(&bot.cfg.chat_id, &format_alert(o)).await {
                tracing::warn!("send alert failed: {e}");
            } else {
                seen.insert(o.condition_id.clone(), o.arb.gross_edge);
            }
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
            let Some(msg) = u.message else { continue };
            let Some(text) = msg.text else { continue };
            let chat_id = msg.chat.id.to_string();
            let reply = bot.handle_command(text.trim()).await;
            if let Some(reply) = reply {
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

    async fn handle_command(&self, text: &str) -> Option<String> {
        let mut parts = text.splitn(2, char::is_whitespace);
        let raw = parts.next().unwrap_or("");
        let arg = parts.next().unwrap_or("").trim();
        let cmd = raw.split('@').next().unwrap_or(raw);
        match cmd {
            "/start" | "/help" => Some(HELP.to_string()),
            "/status" => Some(self.status_text().await),
            "/top" => Some(self.top_text().await),
            "/add" => Some(self.cmd_add(arg).await),
            "/remove" | "/rm" => Some(self.cmd_remove(arg).await),
            "/list" => Some(self.cmd_list().await),
            "/search" | "/find" => Some(self.cmd_search(arg).await),
            "/add_category" | "/addcategory" => Some(self.cmd_add_category(arg).await),
            "/remove_category" | "/removecategory" => Some(self.cmd_remove_category(arg).await),
            "/categories" => Some(self.cmd_categories().await),
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

    async fn cmd_settings(&self) -> String {
        let s = self.settings.read().await.clone();
        format!(
            "<b>Alert settings</b>\n\
             enabled: <b>{}</b>\n\
             only_profitable: <b>{}</b>\n\
             min_edge: <b>{}</b>\n\
             min_profit: <b>${}</b>\n\n\
             Change with: /set &lt;key&gt; &lt;value&gt;\n\
             keys: enabled, only_profitable, min_edge, min_profit\n\
             or /mute /unmute",
            s.enabled, s.only_profitable, s.min_edge, s.min_profit
        )
    }

    async fn cmd_set(&self, arg: &str) -> String {
        let mut it = arg.split_whitespace();
        let (Some(key), Some(val)) = (it.next(), it.next()) else {
            return "Usage: /set &lt;key&gt; &lt;value&gt; (keys: enabled, only_profitable, min_edge, min_profit)".into();
        };
        let mut s = self.settings.write().await;
        let result = match key {
            "enabled" => parse_bool(val).map(|b| s.enabled = b),
            "only_profitable" => parse_bool(val).map(|b| s.only_profitable = b),
            "min_edge" => val.parse::<f64>().ok().map(|v| s.min_edge = v),
            "min_profit" => val.parse::<f64>().ok().map(|v| s.min_profit = v),
            _ => {
                return format!(
                    "Unknown key '{}'. Keys: enabled, only_profitable, min_edge, min_profit",
                    esc(key)
                )
            }
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
                let mut s = format!("<b>Watching {} market(s)</b>\n", markets.len());
                for m in &markets {
                    s.push_str(&format!(
                        "• {}\n  <code>{}</code>\n",
                        esc(m["question"].as_str().unwrap_or("")),
                        esc(m["slug"].as_str().unwrap_or("")),
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
                let mut s = String::from("<b>Results</b> (tap a slug, then /add it)\n\n");
                for m in &results {
                    s.push_str(&format!(
                        "• {}\n  <code>{}</code>\n",
                        esc(m["question"].as_str().unwrap_or("")),
                        esc(m["slug"].as_str().unwrap_or("")),
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
        let opp_count = self
            .fetch_opportunities()
            .await
            .map(|r| r.opportunities.len())
            .unwrap_or(0);
        format!(
            "<b>Backend status</b>\n\
             ws_connected: <b>{}</b>\n\
             watched markets: <b>{}</b>\n\
             categories: <b>{}</b>\n\
             live opportunities: <b>{}</b>",
            health["ws_connected"].as_bool().unwrap_or(false),
            health["watched"].as_u64().unwrap_or(0),
            cat_count,
            opp_count,
        )
    }

    async fn top_text(&self) -> String {
        match self.fetch_opportunities().await {
            Ok(r) if !r.opportunities.is_empty() => {
                let mut s = String::from("<b>Top opportunities</b>\n\n");
                for o in r.opportunities.iter().take(5) {
                    s.push_str(&format_alert(o));
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
        let body =
            serde_json::json!({ "offset": offset, "timeout": 25, "allowed_updates": ["message"] });
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

fn format_alert(o: &Opportunity) -> String {
    let a = &o.arb;
    let mut s = format!(
        "🟢 <b>YES+NO arbitrage</b>\n<b>{q}</b>\n\n\
         YES ask: <code>{yes:.4}</code>  NO ask: <code>{no:.4}</code>\n\
         sum: <code>{sum:.4}</code>  edge: <code>{edge:.4}</code>\n\
         size: <code>{size:.1}</code> sets  cost: <code>${cost:.2}</code>\n\
         fees: <code>${fees:.2}</code>  gas: <code>${gas:.2}</code>\n\
         net profit: <b>${net:.2}</b>  roi: <b>{roi:.2}%</b>\n\
         volume: <code>${vol:.0}</code>\n{url}",
        q = esc(&o.question),
        yes = a.best_yes,
        no = a.best_no,
        sum = a.sum,
        edge = a.gross_edge,
        size = a.size,
        cost = a.cost,
        fees = a.fees,
        gas = a.gas,
        net = a.net_profit,
        roi = a.roi * 100.0,
        vol = o.volume,
        url = esc(&o.market_url),
    );
    if let Some(sse) = o.sell_side_edge {
        s.push_str(&format!(
            "\n↔️ sell-side edge (bid YES+NO−1): <code>{sse:.4}</code>"
        ));
    }
    s
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

const HELP: &str = "<b>Polymarket YES/NO arbitrage alerter</b>\n\n\
Alerts fire only when a full set (YES+NO) can be bought below $1. \
This bot never places orders.\n\n\
<b>Categories</b> (auto-tracked, refreshed every few minutes)\n\
/add_category &lt;link|slug&gt; — track a whole category (e.g. polymarket.com/iran)\n\
/remove_category &lt;slug&gt; — stop tracking it\n\
/categories — list tracked categories\n\n\
<b>Single markets</b>\n\
Paste a Polymarket link, or:\n\
/add &lt;link|slug&gt; · /remove &lt;slug&gt; · /list · /search &lt;query&gt;\n\n\
<b>Notifications</b>\n\
/settings — show alert filters\n\
/set &lt;key&gt; &lt;value&gt; — keys: enabled, only_profitable, min_edge, min_profit\n\
/mute · /unmute\n\n\
<b>Info</b>\n\
/top — current best opportunities\n\
/status — backend health";

#[derive(Debug, Deserialize)]
struct TgResponse<T> {
    #[serde(default)]
    result: Option<T>,
}

#[derive(Debug, Deserialize)]
struct Update {
    update_id: i64,
    message: Option<Message>,
}

#[derive(Debug, Deserialize)]
struct Message {
    chat: Chat,
    text: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Chat {
    id: i64,
}
