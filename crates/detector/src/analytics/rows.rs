//! One struct per ClickHouse table. Field names and order match the DDL in
//! [`super::schema`]; they are inserted as `JSONEachRow`, so a name that drifts
//! from the DDL is a silent write failure — keep the two in step.
//!
//! Timestamps are rendered as `DateTime64(3)` literals rather than numbers:
//! ClickHouse reads a bare number in a `DateTime64(3)` column as raw ticks, not
//! seconds, which would put every row 50 years from where it belongs.

use serde::Serialize;

/// Coarse liquidity bucket from a market's lifetime volume. The tail/mid split
/// is what separates "an edge nobody else bothers with" from "an edge that is
/// gone before you see it".
pub fn liquidity_tier(volume: f64) -> &'static str {
    match volume {
        v if v < 10_000.0 => "tail",
        v if v < 100_000.0 => "mid",
        v if v < 1_000_000.0 => "liquid",
        _ => "deep",
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct OpportunityRow {
    pub ts: String,
    pub event: &'static str,
    pub opportunity_id: String,
    pub strategy: &'static str,
    pub condition_id: String,
    pub slug: String,
    pub question: String,
    pub category: String,

    pub yes_bid: f64,
    pub yes_ask: f64,
    pub no_bid: f64,
    pub no_ask: f64,
    pub yes_spread: f64,
    pub no_spread: f64,
    pub ask_sum: f64,
    pub bid_sum: f64,
    pub yes_ask_prices: Vec<f64>,
    pub yes_ask_sizes: Vec<f64>,
    pub yes_bid_prices: Vec<f64>,
    pub yes_bid_sizes: Vec<f64>,
    pub no_ask_prices: Vec<f64>,
    pub no_ask_sizes: Vec<f64>,
    pub no_bid_prices: Vec<f64>,
    pub no_bid_sizes: Vec<f64>,
    pub yes_depth_usd: f64,
    pub no_depth_usd: f64,
    pub yes_levels: u16,
    pub no_levels: u16,
    pub book_age_ms: u32,
    pub trades_5m: u32,
    pub traded_5m_usd: f64,
    pub market_volume: f64,
    pub liquidity_tier: &'static str,

    pub gross_edge: f64,
    pub avg_edge: f64,
    pub size: f64,
    pub cost: f64,
    pub payout: f64,
    pub fees: f64,
    pub gas: f64,
    pub net_profit: f64,
    pub roi: f64,
    pub profitable: u8,
    pub depth_cost_usd: f64,
    pub fee_bps: f64,
    pub slippage_bps: f64,
    pub gas_cost: f64,
    pub max_capital: f64,

    pub capital_locked: f64,
    pub secs_to_resolution: i64,

    pub detect_latency_us: u32,
    pub age_ms: u32,
    pub observation: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct CrossingRow {
    pub closed_at: String,
    pub opened_at: String,
    pub seq: u64,
    pub opportunity_id: String,
    pub condition_id: String,
    pub slug: String,
    pub question: String,
    pub category: String,
    pub strategy: &'static str,

    pub lived_ms: u32,
    pub observations: u32,
    pub time_to_peak_ms: u32,

    pub peak_avg_edge: f64,
    pub peak_gross_edge: f64,
    pub peak_net: f64,
    pub peak_size: f64,
    pub first_avg_edge: f64,
    pub last_avg_edge: f64,
    pub peak_profitable: u8,

    pub market_volume: f64,
    pub liquidity_tier: &'static str,
    pub reopen_gap_ms: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct BookSampleRow {
    pub ts: String,
    pub condition_id: String,
    pub slug: String,
    pub category: String,
    pub market_volume: f64,
    pub liquidity_tier: &'static str,

    pub yes_bid: f64,
    pub yes_ask: f64,
    pub no_bid: f64,
    pub no_ask: f64,
    pub yes_spread: f64,
    pub no_spread: f64,
    pub ask_sum: f64,
    pub bid_sum: f64,
    pub yes_depth_usd: f64,
    pub no_depth_usd: f64,
    pub yes_levels: u16,
    pub no_levels: u16,

    pub book_updates: u32,
    pub trades_5m: u32,
    pub traded_5m_usd: f64,
    pub secs_to_resolution: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct TradeRow {
    pub ts: String,
    pub condition_id: String,
    pub token_id: String,
    pub outcome: &'static str,
    pub side: String,
    pub price: f64,
    pub size: f64,
    pub notional: f64,
}

/// A leg of a real, placed trade. Nothing in this repo produces one — it is
/// posted in by an external execution layer over `POST /analytics/execution`.
/// Every field is optional on the wire and defaulted here, so a caller can log
/// what it knows (say: signal/submit timing and a partial fill) without having
/// to invent gas numbers it does not have yet.
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
#[serde(default)]
pub struct ExecutionRow {
    pub ts: String,
    pub opportunity_id: String,
    pub condition_id: String,
    pub strategy: String,
    pub leg: String,
    pub status: String,

    pub signal_ts: String,
    pub submit_ts: String,
    pub confirm_ts: String,
    pub signal_to_submit_ms: u32,
    pub submit_to_confirm_ms: u32,

    pub quoted_price: f64,
    pub filled_price: f64,
    pub slippage_price: f64,
    pub slippage_usd: f64,
    pub requested_size: f64,
    pub filled_size: f64,
    pub fill_ratio: f64,

    pub fee_usd: f64,
    pub gas_price_gwei: f64,
    pub gas_used: u64,
    pub gas_cost_usd: f64,

    pub tx_hash: String,
    pub block_number: u64,
    pub realized_edge: f64,
    pub net_pnl: f64,
    pub toxic_fill: u8,
    pub capital_locked: f64,
    pub note: String,
}

impl Default for ExecutionRow {
    fn default() -> Self {
        let epoch = || arb_core::time::clickhouse_datetime(0);
        Self {
            ts: String::new(),
            opportunity_id: String::new(),
            condition_id: String::new(),
            strategy: String::new(),
            leg: String::new(),
            status: String::new(),
            signal_ts: epoch(),
            submit_ts: epoch(),
            confirm_ts: epoch(),
            signal_to_submit_ms: 0,
            submit_to_confirm_ms: 0,
            quoted_price: 0.0,
            filled_price: 0.0,
            slippage_price: 0.0,
            slippage_usd: 0.0,
            requested_size: 0.0,
            filled_size: 0.0,
            fill_ratio: 0.0,
            fee_usd: 0.0,
            gas_price_gwei: 0.0,
            gas_used: 0,
            gas_cost_usd: 0.0,
            tx_hash: String::new(),
            block_number: 0,
            realized_edge: 0.0,
            net_pnl: 0.0,
            toxic_fill: 0,
            capital_locked: 0.0,
            note: String::new(),
        }
    }
}

impl ExecutionRow {
    /// Fill in what the caller left out but we can derive: the timestamp, the
    /// two latency legs, the slippage in price and dollars, and the fill ratio.
    pub fn derive(&mut self, now_ms: u64) {
        if self.ts.is_empty() {
            self.ts = arb_core::time::clickhouse_datetime(now_ms);
        }
        if self.status.is_empty() {
            self.status = "filled".into();
        }
        if self.fill_ratio == 0.0 && self.requested_size > 0.0 {
            self.fill_ratio = self.filled_size / self.requested_size;
        }
        if self.slippage_price == 0.0 && self.quoted_price > 0.0 && self.filled_price > 0.0 {
            // Buying (mint) fills worse when higher; selling (split) when lower.
            self.slippage_price = if self.strategy == "split" {
                self.quoted_price - self.filled_price
            } else {
                self.filled_price - self.quoted_price
            };
        }
        if self.slippage_usd == 0.0 {
            self.slippage_usd = self.slippage_price * self.filled_size;
        }
    }
}

/// A row bound for one table. Boxed where big so the channel's item size stays
/// reasonable — a live arb can enqueue these on every book update.
#[derive(Debug, Clone)]
pub enum Row {
    Opportunity(Box<OpportunityRow>),
    Crossing(Box<CrossingRow>),
    BookSample(Box<BookSampleRow>),
    Trade(Box<TradeRow>),
    Execution(Box<ExecutionRow>),
}

impl Row {
    pub fn table(&self) -> &'static str {
        match self {
            Row::Opportunity(_) => "opportunities",
            Row::Crossing(_) => "crossings",
            Row::BookSample(_) => "book_samples",
            Row::Trade(_) => "trades",
            Row::Execution(_) => "executions",
        }
    }

    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        match self {
            Row::Opportunity(r) => serde_json::to_string(r),
            Row::Crossing(r) => serde_json::to_string(r),
            Row::BookSample(r) => serde_json::to_string(r),
            Row::Trade(r) => serde_json::to_string(r),
            Row::Execution(r) => serde_json::to_string(r),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tiers_split_where_the_behaviour_changes() {
        assert_eq!(liquidity_tier(0.0), "tail");
        assert_eq!(liquidity_tier(9_999.0), "tail");
        assert_eq!(liquidity_tier(10_000.0), "mid");
        assert_eq!(liquidity_tier(250_000.0), "liquid");
        assert_eq!(liquidity_tier(5_000_000.0), "deep");
    }

    #[test]
    fn execution_rows_derive_what_the_caller_omitted() {
        let mut r = ExecutionRow {
            strategy: "mint".into(),
            quoted_price: 0.24,
            filled_price: 0.26,
            requested_size: 100.0,
            filled_size: 40.0,
            ..Default::default()
        };
        r.derive(1_784_996_621_000);

        assert_eq!(r.ts, "2026-07-25 16:23:41.000");
        assert_eq!(r.status, "filled");
        assert!((r.fill_ratio - 0.4).abs() < 1e-9);
        // A buy that filled 2c above the quote is 2c of adverse slippage.
        assert!((r.slippage_price - 0.02).abs() < 1e-9);
        assert!((r.slippage_usd - 0.8).abs() < 1e-9);
    }

    #[test]
    fn selling_slips_the_other_way() {
        let mut r = ExecutionRow {
            strategy: "split".into(),
            quoted_price: 0.72,
            filled_price: 0.70,
            requested_size: 10.0,
            filled_size: 10.0,
            ..Default::default()
        };
        r.derive(0);
        // Hitting a bid 2c lower than quoted is also 2c of adverse slippage.
        assert!((r.slippage_price - 0.02).abs() < 1e-9);
    }

    #[test]
    fn json_field_names_match_the_ddl_columns() {
        // JSONEachRow matches on name: a field that drifts from the DDL is a
        // silent drop, so pin the ones the queries depend on.
        let row = Row::Trade(Box::new(TradeRow {
            ts: "2026-07-25 16:23:41.000".into(),
            condition_id: "0xabc".into(),
            token_id: "123".into(),
            outcome: "yes",
            side: "BUY".into(),
            price: 0.24,
            size: 10.0,
            notional: 2.4,
        }));
        let json = row.to_json().unwrap();
        assert_eq!(row.table(), "trades");
        for column in ["ts", "condition_id", "token_id", "outcome", "notional"] {
            assert!(json.contains(&format!("\"{column}\":")), "missing {column}");
        }
    }
}
