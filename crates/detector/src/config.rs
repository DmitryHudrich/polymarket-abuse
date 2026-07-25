//! Layered detector configuration. Precedence (later wins):
//! defaults < YAML < ENV < CLI. Every field can be set in any layer.

use clap::Parser;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;

use arb_core::ArbParams;

fn env_str(key: &str) -> Option<String> {
    std::env::var(key).ok()
}

fn env_parse<T: std::str::FromStr>(key: &str) -> Option<T> {
    std::env::var(key).ok().and_then(|v| v.parse().ok())
}

/// Default taker fee (bps per side) by category slug, derived from Polymarket's
/// Fee Structure V2 (`fee_bps = 5000 × category_rate`, matching the max fee at
/// price 0.50). Geopolitics/world events are free.
fn default_category_fees() -> HashMap<String, f64> {
    [
        ("crypto", 350.0),
        ("sports", 250.0),
        ("politics", 200.0),
        ("tech", 200.0),
        ("finance", 200.0),
        ("economy", 250.0),
        ("culture", 250.0),
        ("weather", 250.0),
        ("geopolitics", 0.0),
        ("world", 0.0),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect()
}

/// Fully resolved configuration.
#[derive(Debug, Clone)]
pub struct Config {
    pub bind_addr: String,
    pub ws_url: String,
    pub gamma_url: String,
    pub watch_file: PathBuf,
    pub search_limit: usize,
    pub http_timeout_secs: u64,
    pub log_level: String,
    pub auto_discover: bool,
    pub discover_interval_secs: u64,
    pub discover_min_volume: f64,
    pub discover_max_markets: usize,
    pub category_fee_bps: HashMap<String, f64>,
    pub arb: ArbParams,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bind_addr: "0.0.0.0:8080".into(),
            ws_url: "wss://ws-subscriptions-clob.polymarket.com/ws/market".into(),
            gamma_url: "https://gamma-api.polymarket.com".into(),
            watch_file: "watchlist.json".into(),
            search_limit: 8,
            http_timeout_secs: 15,
            log_level: "info".into(),
            auto_discover: true,
            discover_interval_secs: 300,
            discover_min_volume: 0.0,
            discover_max_markets: 100_000,
            category_fee_bps: default_category_fees(),
            arb: ArbParams::default(),
        }
    }
}

/// Partial configuration: every field is optional so an unset value never
/// overwrites a lower layer. Shared by YAML, ENV and CLI.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PartialConfig {
    pub bind_addr: Option<String>,
    pub ws_url: Option<String>,
    pub gamma_url: Option<String>,
    pub watch_file: Option<PathBuf>,
    pub search_limit: Option<usize>,
    pub http_timeout_secs: Option<u64>,
    pub log_level: Option<String>,
    pub auto_discover: Option<bool>,
    pub discover_interval_secs: Option<u64>,
    pub discover_min_volume: Option<f64>,
    pub discover_max_markets: Option<usize>,
    pub category_fee_bps: Option<HashMap<String, f64>>,

    pub fee_bps: Option<f64>,
    pub slippage_bps: Option<f64>,
    pub gas_cost: Option<f64>,
    pub max_capital: Option<f64>,
    pub min_profit: Option<f64>,
    pub min_edge: Option<f64>,
}

impl PartialConfig {
    fn overlay(mut self, other: PartialConfig) -> Self {
        macro_rules! ov {
            ($($f:ident),* $(,)?) => {$(
                if other.$f.is_some() { self.$f = other.$f; }
            )*};
        }
        ov!(
            bind_addr,
            ws_url,
            gamma_url,
            watch_file,
            search_limit,
            http_timeout_secs,
            log_level,
            auto_discover,
            discover_interval_secs,
            discover_min_volume,
            discover_max_markets,
            category_fee_bps,
            fee_bps,
            slippage_bps,
            gas_cost,
            max_capital,
            min_profit,
            min_edge
        );
        self
    }

    fn from_env() -> Self {
        Self {
            bind_addr: env_str("DETECTOR_BIND_ADDR"),
            ws_url: env_str("DETECTOR_WS_URL"),
            gamma_url: env_str("DETECTOR_GAMMA_URL"),
            watch_file: env_str("DETECTOR_WATCH_FILE").map(PathBuf::from),
            search_limit: env_parse("DETECTOR_SEARCH_LIMIT"),
            http_timeout_secs: env_parse("DETECTOR_HTTP_TIMEOUT_SECS"),
            log_level: env_str("DETECTOR_LOG_LEVEL"),
            auto_discover: env_parse("DETECTOR_AUTO_DISCOVER"),
            discover_interval_secs: env_parse("DETECTOR_DISCOVER_INTERVAL_SECS"),
            discover_min_volume: env_parse("DETECTOR_DISCOVER_MIN_VOLUME"),
            discover_max_markets: env_parse("DETECTOR_DISCOVER_MAX_MARKETS"),
            category_fee_bps: None,
            fee_bps: env_parse("DETECTOR_FEE_BPS"),
            slippage_bps: env_parse("DETECTOR_SLIPPAGE_BPS"),
            gas_cost: env_parse("DETECTOR_GAS_COST"),
            max_capital: env_parse("DETECTOR_MAX_CAPITAL"),
            min_profit: env_parse("DETECTOR_MIN_PROFIT"),
            min_edge: env_parse("DETECTOR_MIN_EDGE"),
        }
    }

    fn resolve(self) -> Config {
        let d = Config::default();
        Config {
            bind_addr: self.bind_addr.unwrap_or(d.bind_addr),
            ws_url: self.ws_url.unwrap_or(d.ws_url),
            gamma_url: self.gamma_url.unwrap_or(d.gamma_url),
            watch_file: self.watch_file.unwrap_or(d.watch_file),
            search_limit: self.search_limit.unwrap_or(d.search_limit),
            http_timeout_secs: self.http_timeout_secs.unwrap_or(d.http_timeout_secs),
            log_level: self.log_level.unwrap_or(d.log_level),
            auto_discover: self.auto_discover.unwrap_or(d.auto_discover),
            discover_interval_secs: self
                .discover_interval_secs
                .unwrap_or(d.discover_interval_secs),
            discover_min_volume: self.discover_min_volume.unwrap_or(d.discover_min_volume),
            discover_max_markets: self.discover_max_markets.unwrap_or(d.discover_max_markets),
            category_fee_bps: self.category_fee_bps.unwrap_or(d.category_fee_bps),
            arb: ArbParams {
                fee_bps: self.fee_bps.unwrap_or(d.arb.fee_bps),
                slippage_bps: self.slippage_bps.unwrap_or(d.arb.slippage_bps),
                gas_cost: self.gas_cost.unwrap_or(d.arb.gas_cost),
                max_capital: self.max_capital.unwrap_or(d.arb.max_capital),
                min_profit: self.min_profit.unwrap_or(d.arb.min_profit),
                min_edge: self.min_edge.unwrap_or(d.arb.min_edge),
            },
        }
    }
}

/// CLI arguments. Every parameter is an override, hence Option.
#[derive(Debug, Parser)]
#[command(
    name = "detector",
    about = "Polymarket YES/NO arbitrage detector (websocket, alerts-only)"
)]
pub struct Cli {
    /// Path to the YAML config file.
    #[arg(
        short,
        long,
        env = "DETECTOR_CONFIG",
        default_value = "config/detector.yaml"
    )]
    pub config: PathBuf,

    #[arg(long)]
    pub bind_addr: Option<String>,
    #[arg(long)]
    pub ws_url: Option<String>,
    #[arg(long)]
    pub gamma_url: Option<String>,
    #[arg(long)]
    pub watch_file: Option<PathBuf>,
    #[arg(long)]
    pub search_limit: Option<usize>,
    #[arg(long)]
    pub http_timeout_secs: Option<u64>,
    #[arg(long)]
    pub log_level: Option<String>,
    #[arg(long)]
    pub auto_discover: Option<bool>,
    #[arg(long)]
    pub discover_interval_secs: Option<u64>,
    #[arg(long)]
    pub discover_min_volume: Option<f64>,
    #[arg(long)]
    pub discover_max_markets: Option<usize>,
    #[arg(long)]
    pub fee_bps: Option<f64>,
    #[arg(long)]
    pub slippage_bps: Option<f64>,
    #[arg(long)]
    pub gas_cost: Option<f64>,
    #[arg(long)]
    pub max_capital: Option<f64>,
    #[arg(long)]
    pub min_profit: Option<f64>,
    #[arg(long)]
    pub min_edge: Option<f64>,
}

impl Cli {
    fn into_partial(self) -> (PathBuf, PartialConfig) {
        let p = PartialConfig {
            bind_addr: self.bind_addr,
            ws_url: self.ws_url,
            gamma_url: self.gamma_url,
            watch_file: self.watch_file,
            search_limit: self.search_limit,
            http_timeout_secs: self.http_timeout_secs,
            log_level: self.log_level,
            auto_discover: self.auto_discover,
            discover_interval_secs: self.discover_interval_secs,
            discover_min_volume: self.discover_min_volume,
            discover_max_markets: self.discover_max_markets,
            category_fee_bps: None,
            fee_bps: self.fee_bps,
            slippage_bps: self.slippage_bps,
            gas_cost: self.gas_cost,
            max_capital: self.max_capital,
            min_profit: self.min_profit,
            min_edge: self.min_edge,
        };
        (self.config, p)
    }
}

/// Resolve the final config: defaults < YAML < ENV < CLI.
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

    Ok(yaml_part
        .overlay(PartialConfig::from_env())
        .overlay(cli_part)
        .resolve())
}
