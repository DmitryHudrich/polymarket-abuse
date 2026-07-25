//! Layered telegram-bot configuration. Precedence (later wins):
//! defaults < YAML < ENV < CLI.

use clap::Parser;
use serde::Deserialize;
use std::path::PathBuf;

fn env_str(key: &str) -> Option<String> {
    std::env::var(key).ok()
}

fn env_parse<T: std::str::FromStr>(key: &str) -> Option<T> {
    std::env::var(key).ok().and_then(|v| v.parse().ok())
}

#[derive(Debug, Clone)]
pub struct Config {
    pub bot_token: String,
    pub chat_id: String,
    pub backend_url: String,
    pub poll_interval_secs: u64,
    pub http_timeout_secs: u64,
    pub alert_min_edge: f64,
    pub alert_min_profit: f64,
    pub only_profitable: bool,
    pub realert_improvement: f64,
    pub settings_file: PathBuf,
    pub log_level: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bot_token: String::new(),
            chat_id: String::new(),
            backend_url: "http://localhost:8080".into(),
            poll_interval_secs: 10,
            http_timeout_secs: 15,
            alert_min_edge: 0.03,
            alert_min_profit: 25.0,
            only_profitable: true,
            realert_improvement: 0.01,
            settings_file: "bot-settings.json".into(),
            log_level: "info".into(),
        }
    }
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PartialConfig {
    pub bot_token: Option<String>,
    pub chat_id: Option<String>,
    pub backend_url: Option<String>,
    pub poll_interval_secs: Option<u64>,
    pub http_timeout_secs: Option<u64>,
    pub alert_min_edge: Option<f64>,
    pub alert_min_profit: Option<f64>,
    pub only_profitable: Option<bool>,
    pub realert_improvement: Option<f64>,
    pub settings_file: Option<PathBuf>,
    pub log_level: Option<String>,
}

impl PartialConfig {
    fn overlay(mut self, other: PartialConfig) -> Self {
        macro_rules! ov {
            ($($f:ident),* $(,)?) => {$(
                if other.$f.is_some() { self.$f = other.$f; }
            )*};
        }
        ov!(
            bot_token,
            chat_id,
            backend_url,
            poll_interval_secs,
            http_timeout_secs,
            alert_min_edge,
            alert_min_profit,
            only_profitable,
            realert_improvement,
            settings_file,
            log_level
        );
        self
    }

    fn from_env() -> Self {
        Self {
            bot_token: env_str("TELEGRAM_BOT_TOKEN"),
            chat_id: env_str("TELEGRAM_CHAT_ID"),
            backend_url: env_str("TELEGRAM_BACKEND_URL"),
            poll_interval_secs: env_parse("TELEGRAM_POLL_INTERVAL_SECS"),
            http_timeout_secs: env_parse("TELEGRAM_HTTP_TIMEOUT_SECS"),
            alert_min_edge: env_parse("TELEGRAM_ALERT_MIN_EDGE"),
            alert_min_profit: env_parse("TELEGRAM_ALERT_MIN_PROFIT"),
            only_profitable: env_parse("TELEGRAM_ONLY_PROFITABLE"),
            realert_improvement: env_parse("TELEGRAM_REALERT_IMPROVEMENT"),
            settings_file: env_str("TELEGRAM_SETTINGS_FILE").map(PathBuf::from),
            log_level: env_str("TELEGRAM_LOG_LEVEL"),
        }
    }

    fn resolve(self) -> Config {
        let d = Config::default();
        Config {
            bot_token: self.bot_token.unwrap_or(d.bot_token),
            chat_id: self.chat_id.unwrap_or(d.chat_id),
            backend_url: self.backend_url.unwrap_or(d.backend_url),
            poll_interval_secs: self.poll_interval_secs.unwrap_or(d.poll_interval_secs),
            http_timeout_secs: self.http_timeout_secs.unwrap_or(d.http_timeout_secs),
            alert_min_edge: self.alert_min_edge.unwrap_or(d.alert_min_edge),
            alert_min_profit: self.alert_min_profit.unwrap_or(d.alert_min_profit),
            only_profitable: self.only_profitable.unwrap_or(d.only_profitable),
            realert_improvement: self.realert_improvement.unwrap_or(d.realert_improvement),
            settings_file: self.settings_file.unwrap_or(d.settings_file),
            log_level: self.log_level.unwrap_or(d.log_level),
        }
    }
}

#[derive(Debug, Parser)]
#[command(name = "telegram-bot", about = "Polymarket arbitrage Telegram alerter")]
pub struct Cli {
    #[arg(
        short,
        long,
        env = "TELEGRAM_CONFIG",
        default_value = "config/telegram-bot.yaml"
    )]
    pub config: PathBuf,

    #[arg(long)]
    pub bot_token: Option<String>,
    #[arg(long)]
    pub chat_id: Option<String>,
    #[arg(long)]
    pub backend_url: Option<String>,
    #[arg(long)]
    pub poll_interval_secs: Option<u64>,
    #[arg(long)]
    pub http_timeout_secs: Option<u64>,
    #[arg(long)]
    pub alert_min_edge: Option<f64>,
    #[arg(long)]
    pub alert_min_profit: Option<f64>,
    #[arg(long)]
    pub only_profitable: Option<bool>,
    #[arg(long)]
    pub realert_improvement: Option<f64>,
    #[arg(long)]
    pub settings_file: Option<PathBuf>,
    #[arg(long)]
    pub log_level: Option<String>,
}

impl Cli {
    fn into_partial(self) -> (PathBuf, PartialConfig) {
        let p = PartialConfig {
            bot_token: self.bot_token,
            chat_id: self.chat_id,
            backend_url: self.backend_url,
            poll_interval_secs: self.poll_interval_secs,
            http_timeout_secs: self.http_timeout_secs,
            alert_min_edge: self.alert_min_edge,
            alert_min_profit: self.alert_min_profit,
            only_profitable: self.only_profitable,
            realert_improvement: self.realert_improvement,
            settings_file: self.settings_file,
            log_level: self.log_level,
        };
        (self.config, p)
    }
}

pub fn load() -> anyhow::Result<Config> {
    let (path, cli_part) = Cli::parse().into_partial();

    let yaml_part = match std::fs::read_to_string(&path) {
        Ok(text) => serde_yaml::from_str::<PartialConfig>(&text)
            .map_err(|e| anyhow::anyhow!("YAML {}: {e}", path.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            tracing::warn!(
                "config {} not found, using defaults+env+cli",
                path.display()
            );
            PartialConfig::default()
        }
        Err(e) => return Err(anyhow::anyhow!("cannot read {}: {e}", path.display())),
    };

    let cfg = yaml_part
        .overlay(PartialConfig::from_env())
        .overlay(cli_part)
        .resolve();

    if cfg.bot_token.is_empty() {
        anyhow::bail!("bot_token is required (yaml/env TELEGRAM_BOT_TOKEN/--bot-token)");
    }
    Ok(cfg)
}
