use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use tracing::warn;

use crate::core::types::BPS_SCALE;
use crate::exchange::errors::{ExchangeError, ExchangeResult};
use crate::exchange::types::{FillLevel, OrderBookSnapshot, WalkResult};

/// Analyzes an order book snapshot to compute fill simulations, depth, and spread metrics.
#[derive(Debug, Clone)]
pub struct OrderBookAnalyzer {
    orderbook: OrderBookSnapshot,
}

impl OrderBookAnalyzer {
    /// Creates a new analyzer wrapping the given order book snapshot.
    pub fn new(orderbook: OrderBookSnapshot) -> Self {
        Self { orderbook }
    }

    /// Simulates filling `qty` on `side` ("buy"/"sell") through successive book levels,
    /// returning average price, slippage in BPS, and fill details.
    /// Returns `ExchangeError::OrderRejected` if `side` is invalid.
    pub fn walk_the_book(&self, side: &str, qty: Decimal) -> ExchangeResult<WalkResult> {
        let levels = match side {
            "buy" => &self.orderbook.asks,
            "sell" => &self.orderbook.bids,
            _ => {
                return Err(ExchangeError::OrderRejected(format!(
                    "invalid side: {side}"
                )));
            }
        };

        let best_price = match side {
            "buy" => self.orderbook.best_ask.map(|(p, _)| p),
            "sell" => self.orderbook.best_bid.map(|(p, _)| p),
            _ => None,
        };

        let mut remaining = qty;
        let mut total_cost = Decimal::ZERO;
        let mut total_qty = Decimal::ZERO;
        let mut fills = Vec::new();
        let mut levels_consumed = 0;

        for (price, available) in levels {
            if remaining <= Decimal::ZERO {
                break;
            }
            levels_consumed += 1;
            let fill_qty = remaining.min(*available);
            let cost = fill_qty * price;
            fills.push(FillLevel {
                price: *price,
                qty: fill_qty,
                cost,
            });
            total_cost += cost;
            total_qty += fill_qty;
            remaining -= fill_qty;
        }

        let fully_filled = remaining <= Decimal::ZERO;
        let avg_price = if total_qty > Decimal::ZERO {
            total_cost / total_qty
        } else {
            Decimal::ZERO
        };

        let slippage_bps = match best_price {
            Some(bp) if bp > Decimal::ZERO && side == "buy" => {
                (avg_price - bp) / bp * Decimal::from(BPS_SCALE)
            }
            Some(bp) if bp > Decimal::ZERO && side == "sell" => {
                (bp - avg_price) / bp * Decimal::from(BPS_SCALE)
            }
            _ => Decimal::ZERO,
        };

        Ok(WalkResult {
            avg_price,
            total_cost,
            slippage_bps: slippage_bps.max(Decimal::ZERO),
            levels_consumed,
            fully_filled,
            fills,
        })
    }

    /// Computes the total quantity available within `bps` basis points of the best price on `side` ("bid"/"ask").
    /// Returns `ExchangeError::OrderRejected` if `side` is invalid.
    pub fn depth_at_bps(&self, side: &str, bps: Decimal) -> ExchangeResult<Decimal> {
        let levels = match side {
            "bid" => &self.orderbook.bids,
            "ask" => &self.orderbook.asks,
            _ => {
                return Err(ExchangeError::OrderRejected(format!(
                    "invalid side: {side}"
                )));
            }
        };

        let best = match side {
            "bid" => self.orderbook.best_bid.map(|(p, _)| p),
            "ask" => self.orderbook.best_ask.map(|(p, _)| p),
            _ => unreachable!(),
        };

        let Some(best_price) = best else {
            return Ok(Decimal::ZERO);
        };

        if best_price <= Decimal::ZERO {
            return Ok(Decimal::ZERO);
        }

        let threshold = best_price * bps / Decimal::from(BPS_SCALE);

        let mut total_qty = Decimal::ZERO;
        for (price, qty) in levels {
            let diff = match side {
                "bid" => best_price - price,
                "ask" => price - best_price,
                _ => unreachable!(),
            };
            if diff <= threshold {
                total_qty += qty;
            } else {
                break;
            }
        }

        Ok(total_qty)
    }

    /// Returns the bid/ask imbalance over the top `levels` as a value in [-1.0, 1.0] (positive = buy pressure).
    pub fn imbalance(&self, levels: usize) -> f64 {
        let bid_levels: Vec<(Decimal, Decimal)> =
            self.orderbook.bids.iter().take(levels).copied().collect();
        let ask_levels: Vec<(Decimal, Decimal)> =
            self.orderbook.asks.iter().take(levels).copied().collect();

        let bid_total: Decimal = bid_levels.iter().map(|(_, q)| q).sum();
        let ask_total: Decimal = ask_levels.iter().map(|(_, q)| q).sum();
        let total = bid_total + ask_total;

        if total == Decimal::ZERO {
            return 0.0;
        }

        let imbalance = (bid_total - ask_total) / total;
        let imbalance_f64 = imbalance.to_f64();
        if imbalance_f64.is_none() {
            warn!("Imbalance value too large for f64, clamping");
        }
        imbalance_f64.unwrap_or(0.0).clamp(-1.0, 1.0)
    }

    /// Computes the effective spread in BPS for a given `qty` by walking both sides of the book.
    /// Returns `Err` if either walk fails; returns zero if mid price is unavailable.
    pub fn effective_spread(&self, qty: Decimal) -> ExchangeResult<Decimal> {
        let buy_walk = self.walk_the_book("buy", qty)?;
        let sell_walk = self.walk_the_book("sell", qty)?;

        let avg_ask = buy_walk.avg_price;
        let avg_bid = sell_walk.avg_price;
        let mid_price = self.orderbook.mid_price;

        let mid = match mid_price {
            Some(m) if m > Decimal::ZERO && avg_ask > Decimal::ZERO && avg_bid > Decimal::ZERO => m,
            _ => return Ok(Decimal::ZERO),
        };

        Ok((avg_ask - avg_bid) / mid * Decimal::from(BPS_SCALE))
    }

    /// Returns a reference to the underlying order book snapshot.
    pub fn orderbook(&self) -> &OrderBookSnapshot {
        &self.orderbook
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exchange::types::OrderBookSnapshot;

    fn make_book() -> OrderBookSnapshot {
        let bids = vec![
            (Decimal::from(2009), Decimal::from(12)),
            (Decimal::from(2008), Decimal::from(8)),
            (Decimal::from(2007), Decimal::from(15)),
            (Decimal::from(2006), Decimal::from(20)),
            (Decimal::from(2005), Decimal::from(25)),
        ];
        let asks = vec![
            (Decimal::from(2010), Decimal::from(5)),
            (Decimal::from(2011), Decimal::from(10)),
            (Decimal::from(2012), Decimal::from(15)),
            (Decimal::from(2013), Decimal::from(20)),
            (Decimal::from(2014), Decimal::from(25)),
        ];
        let best_bid = bids.first().copied();
        let best_ask = asks.first().copied();
        let mid_price = Some((Decimal::from(2009) + Decimal::from(2010)) / Decimal::TWO);
        let spread_bps = mid_price.map(|m| Decimal::ONE / m * Decimal::from(BPS_SCALE));

        OrderBookSnapshot {
            symbol: "ETH/USDT".into(),
            timestamp: 0,
            bids,
            asks,
            best_bid,
            best_ask,
            mid_price,
            spread_bps,
        }
    }

    #[test]
    fn test_walk_the_book_exact_fill() {
        let analyzer = OrderBookAnalyzer::new(make_book());
        let result = analyzer.walk_the_book("buy", Decimal::from(5)).unwrap();
        assert!(result.fully_filled);
        assert_eq!(result.levels_consumed, 1);
        assert_eq!(result.avg_price, Decimal::from(2010));
    }

    #[test]
    fn test_walk_the_book_multiple_levels() {
        let analyzer = OrderBookAnalyzer::new(make_book());
        let result = analyzer.walk_the_book("buy", Decimal::from(12)).unwrap();
        assert!(result.fully_filled);
        assert_eq!(result.levels_consumed, 2);
        let expected_avg = (Decimal::from(5) * Decimal::from(2010)
            + Decimal::from(7) * Decimal::from(2011))
            / Decimal::from(12);
        assert_eq!(result.avg_price, expected_avg);
    }

    #[test]
    fn test_walk_the_book_insufficient_liquidity() {
        let analyzer = OrderBookAnalyzer::new(make_book());
        let result = analyzer.walk_the_book("buy", Decimal::from(1000)).unwrap();
        assert!(!result.fully_filled);
        assert!(result.avg_price > Decimal::ZERO);
    }

    #[test]
    fn test_walk_the_book_invalid_side() {
        let analyzer = OrderBookAnalyzer::new(make_book());
        assert!(analyzer.walk_the_book("byu", Decimal::from(1)).is_err());
    }

    #[test]
    fn test_depth_at_bps_correct() {
        let analyzer = OrderBookAnalyzer::new(make_book());
        let depth = analyzer.depth_at_bps("ask", Decimal::from(10)).unwrap();
        let best_ask = Decimal::from(2010);
        let threshold = best_ask * Decimal::from(10) / Decimal::from(BPS_SCALE);
        let mut expected = Decimal::ZERO;
        for (price, qty) in &analyzer.orderbook.asks {
            if *price - best_ask <= threshold {
                expected += qty;
            } else {
                break;
            }
        }
        assert_eq!(depth, expected);
        assert!(depth > Decimal::ZERO);
    }

    #[test]
    fn test_imbalance_range() {
        let analyzer = OrderBookAnalyzer::new(make_book());
        let imb = analyzer.imbalance(10);
        assert!((-1.0..=1.0).contains(&imb));
    }

    #[test]
    fn test_effective_spread_greater_than_quoted() {
        let analyzer = OrderBookAnalyzer::new(make_book());
        let eff_spread = analyzer.effective_spread(Decimal::from(2)).unwrap();
        let spread_bps = analyzer.orderbook.spread_bps.unwrap();
        assert!(eff_spread >= spread_bps);
    }

    #[test]
    fn test_sell_walk() {
        let analyzer = OrderBookAnalyzer::new(make_book());
        let result = analyzer.walk_the_book("sell", Decimal::from(12)).unwrap();
        assert!(result.fully_filled);
        assert_eq!(result.levels_consumed, 1);
        assert_eq!(result.avg_price, Decimal::from(2009));
    }

    #[test]
    fn test_walk_the_book_sell_multiple_levels() {
        let analyzer = OrderBookAnalyzer::new(make_book());
        let result = analyzer.walk_the_book("sell", Decimal::from(20)).unwrap();
        assert!(result.fully_filled);
        assert!(result.levels_consumed > 1);
        assert!(result.avg_price < Decimal::from(2009));
    }

    #[test]
    fn test_depth_at_bps_bid_side() {
        let analyzer = OrderBookAnalyzer::new(make_book());
        let depth = analyzer.depth_at_bps("bid", Decimal::from(10)).unwrap();
        assert!(depth > Decimal::ZERO);
    }

    #[test]
    fn test_depth_at_bps_invalid_side() {
        let analyzer = OrderBookAnalyzer::new(make_book());
        assert!(analyzer.depth_at_bps("byd", Decimal::from(10)).is_err());
    }

    #[test]
    fn test_imbalance_buy_pressure() {
        let mut book = make_book();
        book.bids = vec![
            (Decimal::from(2009), Decimal::from(100)),
            (Decimal::from(2008), Decimal::from(100)),
        ];
        book.asks = vec![
            (Decimal::from(2010), Decimal::from(1)),
            (Decimal::from(2011), Decimal::from(1)),
        ];
        let analyzer = OrderBookAnalyzer::new(book);
        let imb = analyzer.imbalance(10);
        assert!(imb > 0.5, "Should show strong buy pressure");
    }

    #[test]
    fn test_effective_spread_increases_with_size() {
        let analyzer = OrderBookAnalyzer::new(make_book());
        let small = analyzer.effective_spread(Decimal::from(1)).unwrap();
        let large = analyzer.effective_spread(Decimal::from(10)).unwrap();
        assert!(large >= small);
    }

    #[test]
    fn test_empty_orderbook() {
        let book = OrderBookSnapshot {
            symbol: "ETH/USDT".into(),
            timestamp: 0,
            bids: vec![],
            asks: vec![],
            best_bid: None,
            best_ask: None,
            mid_price: None,
            spread_bps: None,
        };
        let analyzer = OrderBookAnalyzer::new(book);
        let walk = analyzer.walk_the_book("buy", Decimal::from(1)).unwrap();
        assert!(!walk.fully_filled);
        assert_eq!(walk.levels_consumed, 0);
        assert_eq!(
            analyzer.depth_at_bps("bid", Decimal::from(10)).unwrap(),
            Decimal::ZERO
        );
        assert_eq!(analyzer.imbalance(10), 0.0);
    }
}
