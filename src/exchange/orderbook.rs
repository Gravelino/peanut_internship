use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;

use crate::exchange::types::{FillLevel, OrderBookSnapshot, WalkResult};

#[derive(Debug, Clone)]
pub struct OrderBookAnalyzer {
    orderbook: OrderBookSnapshot,
}

impl OrderBookAnalyzer {
    pub fn new(orderbook: OrderBookSnapshot) -> Self {
        Self { orderbook }
    }

    pub fn walk_the_book(&self, side: &str, qty: Decimal) -> WalkResult {
        let levels = match side {
            "buy" => &self.orderbook.asks,
            "sell" => &self.orderbook.bids,
            _ => {
                return WalkResult {
                    avg_price: Decimal::ZERO,
                    total_cost: Decimal::ZERO,
                    slippage_bps: Decimal::ZERO,
                    levels_consumed: 0,
                    fully_filled: false,
                    fills: vec![],
                };
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
                (avg_price - bp) / bp * Decimal::from(10000)
            }
            Some(bp) if bp > Decimal::ZERO && side == "sell" => {
                (bp - avg_price) / bp * Decimal::from(10000)
            }
            _ => Decimal::ZERO,
        };

        WalkResult {
            avg_price,
            total_cost,
            slippage_bps: slippage_bps.max(Decimal::ZERO),
            levels_consumed,
            fully_filled,
            fills,
        }
    }

    pub fn depth_at_bps(&self, side: &str, bps: Decimal) -> Decimal {
        let levels = match side {
            "bid" => &self.orderbook.bids,
            "ask" => &self.orderbook.asks,
            _ => return Decimal::ZERO,
        };

        let best = match side {
            "bid" => self.orderbook.best_bid.map(|(p, _)| p),
            "ask" => self.orderbook.best_ask.map(|(p, _)| p),
            _ => None,
        };

        let Some(best_price) = best else {
            return Decimal::ZERO;
        };

        if best_price <= Decimal::ZERO {
            return Decimal::ZERO;
        }

        let threshold = best_price * bps / Decimal::from(10000);

        let mut total_qty = Decimal::ZERO;
        for (price, qty) in levels {
            let diff = match side {
                "bid" => best_price - price,
                "ask" => price - best_price,
                _ => Decimal::ZERO,
            };
            if diff <= threshold {
                total_qty += qty;
            } else {
                break;
            }
        }

        total_qty
    }

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
        imbalance.to_f64().unwrap_or(0.0).clamp(-1.0, 1.0)
    }

    pub fn effective_spread(&self, qty: Decimal) -> Decimal {
        let buy_walk = self.walk_the_book("buy", qty);
        let sell_walk = self.walk_the_book("sell", qty);

        let avg_ask = buy_walk.avg_price;
        let avg_bid = sell_walk.avg_price;

        if self.orderbook.mid_price <= Decimal::ZERO
            || avg_ask <= Decimal::ZERO
            || avg_bid <= Decimal::ZERO
        {
            return Decimal::ZERO;
        }

        (avg_ask - avg_bid) / self.orderbook.mid_price * Decimal::from(10000)
    }

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
        let mid_price = (Decimal::from(2009) + Decimal::from(2010)) / Decimal::TWO;
        let spread_bps = Decimal::ONE / mid_price * Decimal::from(10000);

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
        let result = analyzer.walk_the_book("buy", Decimal::from(5));
        assert!(result.fully_filled);
        assert_eq!(result.levels_consumed, 1);
        assert_eq!(result.avg_price, Decimal::from(2010));
    }

    #[test]
    fn test_walk_the_book_multiple_levels() {
        let analyzer = OrderBookAnalyzer::new(make_book());
        let result = analyzer.walk_the_book("buy", Decimal::from(12));
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
        let result = analyzer.walk_the_book("buy", Decimal::from(1000));
        assert!(!result.fully_filled);
        assert!(result.avg_price > Decimal::ZERO);
    }

    #[test]
    fn test_depth_at_bps_correct() {
        let analyzer = OrderBookAnalyzer::new(make_book());
        let depth = analyzer.depth_at_bps("ask", Decimal::from(10));
        let best_ask = Decimal::from(2010);
        let threshold = best_ask * Decimal::from(10) / Decimal::from(10000);
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
        assert!(imb >= -1.0 && imb <= 1.0);
    }

    #[test]
    fn test_effective_spread_greater_than_quoted() {
        let analyzer = OrderBookAnalyzer::new(make_book());
        let eff_spread = analyzer.effective_spread(Decimal::from(2));
        assert!(eff_spread >= analyzer.orderbook.spread_bps);
    }

    #[test]
    fn test_sell_walk() {
        let analyzer = OrderBookAnalyzer::new(make_book());
        let result = analyzer.walk_the_book("sell", Decimal::from(12));
        assert!(result.fully_filled);
        assert_eq!(result.levels_consumed, 1);
        assert_eq!(result.avg_price, Decimal::from(2009));
    }

    #[test]
    fn test_walk_the_book_sell_multiple_levels() {
        let analyzer = OrderBookAnalyzer::new(make_book());
        let result = analyzer.walk_the_book("sell", Decimal::from(20));
        assert!(result.fully_filled);
        assert!(result.levels_consumed > 1);
        assert!(result.avg_price < Decimal::from(2009));
    }

    #[test]
    fn test_depth_at_bps_bid_side() {
        let analyzer = OrderBookAnalyzer::new(make_book());
        let depth = analyzer.depth_at_bps("bid", Decimal::from(10));
        assert!(depth > Decimal::ZERO);
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
        let small = analyzer.effective_spread(Decimal::from(1));
        let large = analyzer.effective_spread(Decimal::from(10));
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
            mid_price: Decimal::ZERO,
            spread_bps: Decimal::ZERO,
        };
        let analyzer = OrderBookAnalyzer::new(book);
        let walk = analyzer.walk_the_book("buy", Decimal::from(1));
        assert!(!walk.fully_filled);
        assert_eq!(walk.levels_consumed, 0);
        assert_eq!(
            analyzer.depth_at_bps("bid", Decimal::from(10)),
            Decimal::ZERO
        );
        assert_eq!(analyzer.imbalance(10), 0.0);
    }
}
