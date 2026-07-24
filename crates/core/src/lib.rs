//! YES/NO arbitrage math for Polymarket. No networking, no order placement —
//! given the ASK ladders of both outcomes, it decides whether a full set
//! (1 YES + 1 NO) can be bought for less than 1 USDC and how much net profit
//! survives fees, slippage and gas.
//!
//! A full set always redeems (or merges via the CTF) for exactly 1 USDC, so
//! `ask(YES) + ask(NO) < 1` is a risk-free edge of `1 - sum` per set. The alert
//! condition is therefore `sum < 1`, not `> 1`. The mirror case
//! `bid(YES) + bid(NO) > 1` is a separate sell-side opportunity exposed via
//! [`sell_side_edge`].

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

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ArbParams {
    /// Trading fee per side in basis points. Fee model:
    /// `rate * min(price, 1 - price) * shares`.
    pub fee_bps: f64,
    /// Slippage buffer in bps added to each buy price.
    pub slippage_bps: f64,
    /// Fixed cost to close the position (merge/redeem gas) in USDC.
    pub gas_cost: f64,
    /// Capital cap per trade in USDC.
    pub max_capital: f64,
    /// Minimum net profit in USDC to flag an opportunity as profitable.
    pub min_profit: f64,
    /// Minimum gross edge (`1 - sum`) at best prices to report at all.
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
    pub best_yes: f64,
    pub best_no: f64,
    pub sum: f64,
    pub gross_edge: f64,
    pub size: f64,
    pub cost: f64,
    pub payout: f64,
    pub fees: f64,
    pub gas: f64,
    pub net_profit: f64,
    pub roi: f64,
    pub profitable: bool,
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
        let set_fee = fee_rate * (yp.min(1.0 - yp) + np.min(1.0 - np));
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

    Some(ArbResult {
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
        profitable: net_profit >= params.min_profit && net_profit > 0.0,
    })
}

/// Gross edge of the mirror sell-side case, realizable only if a full set is
/// already held. Returns `Some(edge)` when `bid_yes + bid_no > 1`.
pub fn sell_side_edge(best_bid_yes: f64, best_bid_no: f64) -> Option<f64> {
    let edge = best_bid_yes + best_bid_no - 1.0;
    (edge > 0.0).then_some(edge)
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
        assert!(r.profitable);
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
}
