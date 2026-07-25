//! YES/NO arbitrage math for Polymarket. No networking, no order placement —
//! given the order book ladders of both outcomes, it decides whether a full set
//! (1 YES + 1 NO) can be traded against $1 of intrinsic value and how much net
//! profit survives fees, slippage and gas.
//!
//! A full set always converts to exactly 1 USDC (merge/redeem) and is always
//! mintable for exactly 1 USDC (`splitPosition`), which gives two mirror-image
//! strategies:
//!
//! * [`Strategy::Mint`] — `ask(YES) + ask(NO) < 1`: buy both legs, merge the set
//!   back into 1 USDC. Edge per set is `1 - sum`.
//! * [`Strategy::Split`] — `bid(YES) + bid(NO) > 1`: split 1 USDC into a set,
//!   sell both legs into the bids. Edge per set is `sum - 1`.
//!
//! Both are taker on both legs, so both pay two taker fees; both touch the CTF
//! contract once (merge or split), so both carry `gas_cost`.
//!
//! Note that Polymarket's YES and NO order books are mirror images
//! (`ask_NO = 1 - bid_YES`), so on a single coherent snapshot `ask(YES)+ask(NO)`
//! equals `1 + spread` and `bid(YES)+bid(NO)` equals `1 - spread`; either can
//! cross 1 only through genuine native crossing or — far more often —
//! stale/desynced snapshots. Coherence filtering for that lives in the detector,
//! not here.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Level {
    pub price: f64,
    pub size: f64,
}

impl Level {
    pub fn new(price: f64, size: f64) -> Self {
        Self { price, size }
    }
}

/// Which direction of the set arbitrage a result describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Strategy {
    /// Buy YES and NO below $1 combined, then merge/redeem the set for $1.
    Mint,
    /// Split $1 into a set, then sell both legs into bids summing above $1.
    Split,
}

impl Strategy {
    pub fn as_str(self) -> &'static str {
        match self {
            Strategy::Mint => "mint",
            Strategy::Split => "split",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ArbParams {
    /// Trading fee per side in basis points. Fee model:
    /// `rate * min(price, 1 - price) * shares`.
    pub fee_bps: f64,
    /// Slippage buffer in bps. Added to each buy price, subtracted from each
    /// sell price.
    pub slippage_bps: f64,
    /// Fixed CTF gas cost in USDC for the on-chain leg: merging the set
    /// ([`Strategy::Mint`]) or splitting it ([`Strategy::Split`]).
    pub gas_cost: f64,
    /// Capital cap per trade in USDC.
    pub max_capital: f64,
    /// Minimum net profit in USDC to flag an opportunity as profitable.
    pub min_profit: f64,
    /// Minimum gross edge at best prices to report at all — `1 - sum` for
    /// [`Strategy::Mint`], `sum - 1` for [`Strategy::Split`].
    pub min_edge: f64,
}

impl Default for ArbParams {
    fn default() -> Self {
        Self {
            fee_bps: 0.0,
            slippage_bps: 0.0,
            gas_cost: 0.0,
            max_capital: 1_000.0,
            min_profit: 0.0,
            min_edge: 0.0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArbResult {
    pub strategy: Strategy,
    /// Best YES price traded against — ask for `Mint`, bid for `Split`.
    pub best_yes: f64,
    /// Best NO price traded against — ask for `Mint`, bid for `Split`.
    pub best_no: f64,
    pub sum: f64,
    /// `1 - sum` for `Mint`, `sum - 1` for `Split`; positive means an edge.
    pub gross_edge: f64,
    pub size: f64,
    /// USDC out: ladder cost for `Mint`, `size` (minting at $1/set) for `Split`.
    pub cost: f64,
    /// USDC in: `size` (redeeming at $1/set) for `Mint`, ladder proceeds for `Split`.
    pub payout: f64,
    pub fees: f64,
    pub gas: f64,
    pub net_profit: f64,
    pub roi: f64,
    /// Volume-weighted edge per set after sweeping both ladders and fees,
    /// `(payout - cost - fees) / size`. Unlike `gross_edge` (top of book only)
    /// this is what actually clears at `size`.
    pub avg_edge: f64,
    pub profitable: bool,
}

/// Polymarket's taker fee for one leg, per share:
/// `rate * min(price, 1 - price)`. It peaks at 0.50 and vanishes at the tails,
/// so an off-center market pays much less on its cheap leg than on its dear one.
fn leg_fee(fee_rate: f64, price: f64) -> f64 {
    fee_rate * price.min(1.0 - price)
}

/// Evaluate the mint arbitrage across both buy ladders. Ladders may be in any
/// order; sorted copies are used internally. Returns `None` when either side is
/// empty or the best-price edge is below `params.min_edge`.
pub fn evaluate(yes_asks: &[Level], no_asks: &[Level], params: &ArbParams) -> Option<ArbResult> {
    let mut yes: Vec<Level> = yes_asks.iter().copied().filter(|l| l.size > 0.0).collect();
    let mut no: Vec<Level> = no_asks.iter().copied().filter(|l| l.size > 0.0).collect();
    if yes.is_empty() || no.is_empty() {
        return None;
    }
    yes.sort_by(|a, b| a.price.partial_cmp(&b.price).unwrap());
    no.sort_by(|a, b| a.price.partial_cmp(&b.price).unwrap());

    let best_yes = yes[0].price;
    let best_no = no[0].price;
    let sum = best_yes + best_no;
    let gross_edge = 1.0 - sum;
    if gross_edge < params.min_edge {
        return None;
    }

    let fee_rate = params.fee_bps / 10_000.0;
    let slip = 1.0 + params.slippage_bps / 10_000.0;

    let (mut yi, mut ni) = (0usize, 0usize);
    let mut yes_rem = yes[0].size;
    let mut no_rem = no[0].size;
    let mut total_size = 0.0f64;
    let mut total_cost = 0.0f64;
    let mut total_fees = 0.0f64;

    // Greedily stack sets while the marginal set (plus fee) stays below 1 USDC
    // and capital is left. Prices only rise, so the first miss ends the sweep.
    while yi < yes.len() && ni < no.len() {
        let yp = yes[yi].price * slip;
        let np = no[ni].price * slip;
        let set_price = yp + np;
        let set_fee = leg_fee(fee_rate, yp) + leg_fee(fee_rate, np);
        if set_price + set_fee >= 1.0 {
            break;
        }

        let remaining_capital = params.max_capital - total_cost;
        if remaining_capital <= 0.0 {
            break;
        }
        let mut step = yes_rem.min(no_rem);
        if set_price > 0.0 {
            step = step.min(remaining_capital / set_price);
        }
        if step <= 1e-9 {
            break;
        }

        total_size += step;
        total_cost += step * set_price;
        total_fees += step * set_fee;
        yes_rem -= step;
        no_rem -= step;

        if yes_rem <= 1e-9 {
            yi += 1;
            if yi < yes.len() {
                yes_rem = yes[yi].size;
            }
        }
        if no_rem <= 1e-9 {
            ni += 1;
            if ni < no.len() {
                no_rem = no[ni].size;
            }
        }
    }

    if total_size <= 0.0 {
        return None;
    }

    let payout = total_size;
    let gas = params.gas_cost;
    let net_profit = payout - total_cost - total_fees - gas;
    let roi = if total_cost > 0.0 {
        net_profit / total_cost
    } else {
        0.0
    };
    let avg_edge = (payout - total_cost - total_fees) / total_size;

    Some(ArbResult {
        strategy: Strategy::Mint,
        best_yes,
        best_no,
        sum,
        gross_edge,
        size: total_size,
        cost: total_cost,
        payout,
        fees: total_fees,
        gas,
        net_profit,
        roi,
        avg_edge,
        profitable: net_profit >= params.min_profit && net_profit > 0.0,
    })
}

/// Evaluate the split arbitrage across both sell ladders: mint sets with
/// `splitPosition` at exactly 1 USDC each and hit the resting bids on both legs.
/// Ladders may be in any order; sorted copies are used internally. Returns
/// `None` when either side is empty or the best-price edge (`sum - 1`) is below
/// `params.min_edge`.
///
/// `max_capital` caps the USDC put into `splitPosition`, i.e. the number of sets
/// directly, since a set always costs exactly $1 to mint.
pub fn evaluate_split(
    yes_bids: &[Level],
    no_bids: &[Level],
    params: &ArbParams,
) -> Option<ArbResult> {
    let mut yes: Vec<Level> = yes_bids.iter().copied().filter(|l| l.size > 0.0).collect();
    let mut no: Vec<Level> = no_bids.iter().copied().filter(|l| l.size > 0.0).collect();
    if yes.is_empty() || no.is_empty() {
        return None;
    }
    // Best bid first: selling sweeps the ladder downwards.
    yes.sort_by(|a, b| b.price.partial_cmp(&a.price).unwrap());
    no.sort_by(|a, b| b.price.partial_cmp(&a.price).unwrap());

    let best_yes = yes[0].price;
    let best_no = no[0].price;
    let sum = best_yes + best_no;
    let gross_edge = sum - 1.0;
    if gross_edge < params.min_edge {
        return None;
    }

    let fee_rate = params.fee_bps / 10_000.0;
    // Selling: slippage moves the realized price down, not up.
    let slip = (1.0 - params.slippage_bps / 10_000.0).max(0.0);

    let (mut yi, mut ni) = (0usize, 0usize);
    let mut yes_rem = yes[0].size;
    let mut no_rem = no[0].size;
    let mut total_size = 0.0f64;
    let mut total_proceeds = 0.0f64;
    let mut total_fees = 0.0f64;

    // Greedily stack sets while the marginal set still sells for more than the
    // 1 USDC it costs to mint. Bids only fall, so the first miss ends the sweep.
    while yi < yes.len() && ni < no.len() {
        let yp = yes[yi].price * slip;
        let np = no[ni].price * slip;
        let set_proceeds = yp + np;
        let set_fee = leg_fee(fee_rate, yp) + leg_fee(fee_rate, np);
        if set_proceeds - set_fee <= 1.0 {
            break;
        }

        // Every set locks up exactly 1 USDC in the split, so remaining capital
        // is also the remaining set count.
        let remaining_capital = params.max_capital - total_size;
        if remaining_capital <= 0.0 {
            break;
        }
        let step = yes_rem.min(no_rem).min(remaining_capital);
        if step <= 1e-9 {
            break;
        }

        total_size += step;
        total_proceeds += step * set_proceeds;
        total_fees += step * set_fee;
        yes_rem -= step;
        no_rem -= step;

        if yes_rem <= 1e-9 {
            yi += 1;
            if yi < yes.len() {
                yes_rem = yes[yi].size;
            }
        }
        if no_rem <= 1e-9 {
            ni += 1;
            if ni < no.len() {
                no_rem = no[ni].size;
            }
        }
    }

    if total_size <= 0.0 {
        return None;
    }

    let cost = total_size; // $1 per set into splitPosition
    let gas = params.gas_cost;
    let net_profit = total_proceeds - cost - total_fees - gas;
    let roi = if cost > 0.0 { net_profit / cost } else { 0.0 };
    let avg_edge = (total_proceeds - cost - total_fees) / total_size;

    Some(ArbResult {
        strategy: Strategy::Split,
        best_yes,
        best_no,
        sum,
        gross_edge,
        size: total_size,
        cost,
        payout: total_proceeds,
        fees: total_fees,
        gas,
        net_profit,
        roi,
        avg_edge,
        profitable: net_profit >= params.min_profit && net_profit > 0.0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_profit_from_example() {
        let r = evaluate(
            &[Level::new(0.24, 100.0)],
            &[Level::new(0.70, 100.0)],
            &ArbParams::default(),
        )
        .unwrap();
        assert!((r.sum - 0.94).abs() < 1e-9);
        assert!((r.gross_edge - 0.06).abs() < 1e-9);
        assert!((r.net_profit - 6.0).abs() < 1e-6);
        assert!((r.avg_edge - 0.06).abs() < 1e-9);
        assert!(r.profitable);
    }

    #[test]
    fn avg_edge_below_gross_when_sweeping_worse_levels() {
        // Top of book is 0.24+0.70=0.94 (gross 0.06), but half the size fills at
        // 0.28+0.70=0.98, dragging the realized per-set edge to 0.04.
        let r = evaluate(
            &[Level::new(0.24, 50.0), Level::new(0.28, 50.0)],
            &[Level::new(0.70, 100.0)],
            &ArbParams::default(),
        )
        .unwrap();
        assert!((r.size - 100.0).abs() < 1e-6);
        assert!((r.avg_edge - 0.04).abs() < 1e-9);
        assert!(r.avg_edge < r.gross_edge);
    }

    #[test]
    fn no_arb_when_sum_ge_one() {
        assert!(evaluate(
            &[Level::new(0.55, 100.0)],
            &[Level::new(0.50, 100.0)],
            &ArbParams::default()
        )
        .is_none());
    }

    #[test]
    fn capital_caps_size() {
        let p = ArbParams {
            max_capital: 94.0,
            ..Default::default()
        };
        let r = evaluate(
            &[Level::new(0.24, 1_000.0)],
            &[Level::new(0.70, 1_000.0)],
            &p,
        )
        .unwrap();
        assert!((r.size - 100.0).abs() < 1e-6);
        assert!((r.cost - 94.0).abs() < 1e-6);
    }

    #[test]
    fn gas_eats_thin_edge() {
        let p = ArbParams {
            gas_cost: 0.5,
            ..Default::default()
        };
        let r = evaluate(&[Level::new(0.49, 10.0)], &[Level::new(0.50, 10.0)], &p).unwrap();
        assert!(r.net_profit < 0.0);
        assert!(!r.profitable);
    }

    #[test]
    fn sweeps_multiple_levels() {
        let r = evaluate(
            &[Level::new(0.24, 50.0), Level::new(0.30, 50.0)],
            &[Level::new(0.70, 100.0)],
            &ArbParams::default(),
        )
        .unwrap();
        assert!((r.size - 50.0).abs() < 1e-6);
    }

    #[test]
    fn strategy_serializes_as_the_string_the_bot_matches_on() {
        let r = evaluate_split(
            &[Level::new(0.29, 10.0)],
            &[Level::new(0.72, 10.0)],
            &ArbParams::default(),
        )
        .unwrap();
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["strategy"], "split");
        assert_eq!(Strategy::Split.as_str(), "split");
        assert_eq!(Strategy::Mint.as_str(), "mint");
    }

    #[test]
    fn split_profit_from_bids_above_one() {
        // Split 100 sets for $100, sell into 0.29 / 0.72 bids for $101.
        let r = evaluate_split(
            &[Level::new(0.29, 100.0)],
            &[Level::new(0.72, 100.0)],
            &ArbParams::default(),
        )
        .unwrap();
        assert_eq!(r.strategy, Strategy::Split);
        assert!((r.sum - 1.01).abs() < 1e-9);
        assert!((r.gross_edge - 0.01).abs() < 1e-9);
        assert!((r.cost - 100.0).abs() < 1e-6);
        assert!((r.net_profit - 1.0).abs() < 1e-6);
        assert!(r.profitable);
    }

    #[test]
    fn no_split_arb_when_bids_sum_below_one() {
        assert!(evaluate_split(
            &[Level::new(0.29, 100.0)],
            &[Level::new(0.70, 100.0)],
            &ArbParams::default()
        )
        .is_none());
    }

    #[test]
    fn split_fees_eat_a_thin_edge() {
        // 250 bps per side on an off-center book: fee = 0.025 * (0.29 + 0.28)
        // = 0.01425 per set, more than the 1¢ gross edge.
        let p = ArbParams {
            fee_bps: 250.0,
            ..Default::default()
        };
        let r = evaluate_split(&[Level::new(0.29, 100.0)], &[Level::new(0.72, 100.0)], &p);
        assert!(r.is_none(), "no set clears once both taker fees are paid");
    }

    #[test]
    fn split_free_category_keeps_the_edge() {
        // Same book at 0 bps (geopolitics/world) is worth taking.
        let r = evaluate_split(
            &[Level::new(0.29, 100.0)],
            &[Level::new(0.72, 100.0)],
            &ArbParams {
                fee_bps: 0.0,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(r.net_profit > 0.0 && r.profitable);
    }

    #[test]
    fn split_sweeps_and_stops_at_the_unprofitable_level() {
        // First 40 sets clear at 0.30+0.72; the next level (0.25) drops the sum
        // to 0.97 and is skipped entirely.
        let r = evaluate_split(
            &[Level::new(0.30, 40.0), Level::new(0.25, 500.0)],
            &[Level::new(0.72, 500.0)],
            &ArbParams::default(),
        )
        .unwrap();
        assert!((r.size - 40.0).abs() < 1e-6);
        assert!((r.avg_edge - 0.02).abs() < 1e-9);
    }

    #[test]
    fn split_capital_caps_sets() {
        let p = ArbParams {
            max_capital: 50.0,
            ..Default::default()
        };
        let r = evaluate_split(
            &[Level::new(0.29, 1_000.0)],
            &[Level::new(0.72, 1_000.0)],
            &p,
        )
        .unwrap();
        assert!((r.size - 50.0).abs() < 1e-6);
        assert!((r.cost - 50.0).abs() < 1e-6);
    }

    #[test]
    fn split_gas_eats_thin_edge() {
        let p = ArbParams {
            gas_cost: 0.5,
            ..Default::default()
        };
        let r = evaluate_split(&[Level::new(0.51, 10.0)], &[Level::new(0.50, 10.0)], &p).unwrap();
        assert!(r.net_profit < 0.0);
        assert!(!r.profitable);
    }

    #[test]
    fn split_slippage_lowers_sell_prices() {
        let p = ArbParams {
            slippage_bps: 100.0,
            ..Default::default()
        };
        // 1% off both bids drops the set from 1.01 to 0.9999 — no longer worth minting.
        assert!(
            evaluate_split(&[Level::new(0.29, 100.0)], &[Level::new(0.72, 100.0)], &p).is_none()
        );
    }
}
