//! Multi-factor scorer that assigns a 0-100 quality score to each [`Signal`].

use std::collections::VecDeque;

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::exchange::orderbook::OrderBookAnalyzer;
use crate::exchange::types::{OrderBookSnapshot, SkewResult};
use crate::strategy::signal::{Direction, Signal};

/// Upper bound for stored history; oldest entries are evicted.
const HISTORY_CAP: usize = 100;
const HISTORY_WINDOW: usize = 20;

/// Weights and thresholds controlling how a [`Signal`] is scored.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScorerConfig {
    /// Weight given to the spread sub-score.
    pub spread_weight: Decimal,
    /// Weight given to the liquidity sub-score.
    pub liquidity_weight: Decimal,
    /// Weight given to the inventory-health sub-score.
    pub inventory_weight: Decimal,
    /// Weight given to the historical-success sub-score.
    pub history_weight: Decimal,

    /// Spread (bps) at which the spread sub-score saturates at 100.
    pub excellent_spread_bps: Decimal,
    /// Spread (bps) below which the spread sub-score is zero.
    pub min_spread_bps: Decimal,

    /// CEX slippage (bps) at or below which the liquidity sub-score hits 100.
    pub min_slippage_bps: Decimal,
    /// CEX slippage (bps) at or above which the liquidity sub-score hits 0.
    pub max_slippage_bps: Decimal,
    /// Fallback liquidity sub-score when no order book is available.
    /// A mid-range default (80) keeps the sub-score from silently dominating
    /// the total when callers forget to plumb the book through.
    pub fallback_liquidity: Decimal,
}

impl Default for ScorerConfig {
    fn default() -> Self {
        Self {
            spread_weight: Decimal::new(4, 1),    // 0.4
            liquidity_weight: Decimal::new(2, 1), // 0.2
            inventory_weight: Decimal::new(2, 1), // 0.2
            history_weight: Decimal::new(2, 1),   // 0.2
            excellent_spread_bps: Decimal::from(100),
            min_spread_bps: Decimal::from(30),
            min_slippage_bps: Decimal::from(5),
            max_slippage_bps: Decimal::from(50),
            fallback_liquidity: Decimal::from(80),
        }
    }
}

/// Stateful scorer: keeps a rolling window of past signal outcomes.
#[derive(Debug, Clone)]
pub struct SignalScorer {
    config: ScorerConfig,
    recent_results: VecDeque<(String, bool)>,
}

impl Default for SignalScorer {
    fn default() -> Self {
        Self::new(ScorerConfig::default())
    }
}

impl SignalScorer {
    /// Creates a scorer with the given config and empty history.
    pub fn new(config: ScorerConfig) -> Self {
        Self {
            config,
            recent_results: VecDeque::with_capacity(HISTORY_CAP),
        }
    }

    /// Computes a score in [0, 100] for the given signal.
    ///
    /// `book` is the current CEX order book snapshot for the pair. Pass
    /// `None` when unavailable (e.g. smoke tests or startup) and the
    /// liquidity sub-score falls back to
    /// [`ScorerConfig::fallback_liquidity`].
    pub fn score(
        &self,
        signal: &Signal,
        skews: &[SkewResult],
        book: Option<&OrderBookSnapshot>,
    ) -> Decimal {
        let spread = self.score_spread(signal.spread_bps);
        let liquidity = self.score_liquidity(signal, book);
        let inventory = self.score_inventory(signal, skews);
        let history = self.score_history(&signal.pair);

        let weighted = spread * self.config.spread_weight
            + liquidity * self.config.liquidity_weight
            + inventory * self.config.inventory_weight
            + history * self.config.history_weight;

        clamp_0_100(weighted).round_dp(1)
    }

    /// Liquidity sub-score derived from CEX book slippage at the signal's
    /// requested size. The side walked depends on the arb direction:
    /// buying on CEX → walk asks; selling on CEX → walk bids.
    ///
    /// Returns [`ScorerConfig::fallback_liquidity`] when no book is provided
    /// or when [`OrderBookAnalyzer::walk_the_book`] fails (which logs the
    /// error internally).
    fn score_liquidity(&self, signal: &Signal, book: Option<&OrderBookSnapshot>) -> Decimal {
        let Some(book) = book else {
            return self.config.fallback_liquidity;
        };
        if book.symbol != signal.pair {
            warn!(
                book_symbol = %book.symbol,
                signal_pair = %signal.pair,
                "scorer: book symbol mismatches signal pair; using fallback liquidity"
            );
            return self.config.fallback_liquidity;
        }
        let analyzer = OrderBookAnalyzer::new(book.clone());
        // Signal direction determines which CEX side we will cross. For
        // BuyCexSellDex we buy on CEX (walk asks). For BuyDexSellCex we sell
        // on CEX (walk bids).
        let walk_side = match signal.direction {
            Direction::BuyCexSellDex => "buy",
            Direction::BuyDexSellCex => "sell",
        };
        let slippage_bps = match analyzer.walk_the_book(walk_side, signal.size) {
            Ok(walk) if walk.fully_filled => walk.slippage_bps,
            Ok(walk) => {
                // Partial fill on the given side — the CEX can't absorb the
                // requested size. Treat as worst-case liquidity.
                warn!(
                    pair = %signal.pair,
                    side = walk_side,
                    levels = walk.levels_consumed,
                    "scorer: book cannot fully fill requested size"
                );
                return Decimal::ZERO;
            }
            Err(e) => {
                warn!(pair = %signal.pair, error = %e, "scorer: walk_the_book failed; using fallback liquidity");
                return self.config.fallback_liquidity;
            }
        };

        // Map slippage linearly: min -> 100, max -> 0.
        let min = self.config.min_slippage_bps;
        let max = self.config.max_slippage_bps;
        if slippage_bps <= min {
            return Decimal::from(100);
        }
        if slippage_bps >= max {
            return Decimal::ZERO;
        }
        let range = max - min;
        if range <= Decimal::ZERO {
            // Misconfigured thresholds — treat as saturated 100 to avoid
            // a divide-by-zero and log a warning once per call site.
            warn!("scorer: min_slippage_bps >= max_slippage_bps; using 100");
            return Decimal::from(100);
        }
        let penalty = (slippage_bps - min) / range * Decimal::from(100);
        clamp_0_100(Decimal::from(100) - penalty)
    }

    fn score_spread(&self, spread_bps: Decimal) -> Decimal {
        if spread_bps <= self.config.min_spread_bps {
            return Decimal::ZERO;
        }
        if spread_bps >= self.config.excellent_spread_bps {
            return Decimal::from(100);
        }
        let range = self.config.excellent_spread_bps - self.config.min_spread_bps;
        if range <= Decimal::ZERO {
            return Decimal::from(100);
        }
        (spread_bps - self.config.min_spread_bps) / range * Decimal::from(100)
    }

    fn score_inventory(&self, signal: &Signal, skews: &[SkewResult]) -> Decimal {
        // Penalise when the base asset needs rebalancing.
        let base = match signal.pair.split('/').next() {
            Some(b) if !b.is_empty() => b,
            _ => {
                tracing::warn!(
                    pair = %signal.pair,
                    "malformed pair in scorer; skipping inventory sub-score"
                );
                return Decimal::from(60);
            }
        };
        let relevant: Vec<_> = skews.iter().filter(|s| s.asset == base).collect();
        if relevant.iter().any(|s| s.needs_rebalance) {
            Decimal::from(20)
        } else {
            Decimal::from(60)
        }
    }

    fn score_history(&self, pair: &str) -> Decimal {
        let recent: Vec<bool> = self
            .recent_results
            .iter()
            .rev()
            .take(HISTORY_WINDOW)
            .filter(|(p, _)| p == pair)
            .map(|(_, ok)| *ok)
            .collect();
        if recent.len() < 3 {
            return Decimal::from(50);
        }
        let wins = recent.iter().filter(|v| **v).count();
        Decimal::from(wins) * Decimal::from(100) / Decimal::from(recent.len())
    }

    /// Records the outcome of an executed signal, for future history scoring.
    pub fn record_result(&mut self, pair: &str, success: bool) {
        if self.recent_results.len() >= HISTORY_CAP {
            self.recent_results.pop_front();
        }
        self.recent_results.push_back((pair.to_string(), success));
    }

    /// Time-decayed score: older signals receive progressively smaller weight.
    ///
    /// Decays linearly to 0.5 × score at expiry, and 0 beyond.
    pub fn apply_decay(&self, signal: &Signal) -> Decimal {
        let age = signal.age_seconds().max(0.0);
        let ttl = (signal.expiry - signal.timestamp).num_milliseconds() as f64 / 1000.0;
        if ttl <= 0.0 {
            return Decimal::ZERO;
        }
        let factor = (1.0 - (age / ttl) * 0.5).max(0.0);
        let factor_d = Decimal::from_f64_retain(factor).unwrap_or_else(|| {
            tracing::warn!(
                signal = %signal.signal_id,
                factor,
                "decay factor could not be converted to Decimal; using 1.0"
            );
            Decimal::ONE
        });
        (signal.score * factor_d).max(Decimal::ZERO)
    }
}

fn clamp_0_100(v: Decimal) -> Decimal {
    let max = Decimal::from(100);
    if v < Decimal::ZERO {
        Decimal::ZERO
    } else if v > max {
        max
    } else {
        v
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::strategy::signal::{Direction, Signal, SignalParams};

    fn make_signal(spread_bps: u32) -> Signal {
        let mut s = Signal::new(SignalParams {
            pair: "ETH/USDT".into(),
            direction: Direction::BuyCexSellDex,
            cex_price: Decimal::from(2000),
            dex_price: Decimal::from(2020),
            spread_bps: Decimal::from(spread_bps),
            size: Decimal::ONE,
            expected_gross_pnl: Decimal::from(20),
            expected_fees: Decimal::from(5),
            expected_net_pnl: Decimal::from(15),
            ttl: chrono::Duration::seconds(5),
            inventory_ok: true,
            within_limits: true,
        });
        s.score = Decimal::from(80);
        s
    }

    #[test]
    fn high_spread_scores_high() {
        let scorer = SignalScorer::default();
        let s = scorer.score(&make_signal(100), &[], None);
        // spread=100 -> 100, liquidity=80 (fallback), inventory=60 (no skews -> default 60), history=50
        // weighted = 40 + 16 + 12 + 10 = 78
        assert!(s >= Decimal::from(70));
    }

    #[test]
    fn low_spread_scores_low() {
        let scorer = SignalScorer::default();
        let s = scorer.score(&make_signal(30), &[], None);
        // spread=30 -> 0, rest unchanged.
        // weighted = 0 + 16 + 12 + 10 = 38
        assert!(s <= Decimal::from(45));
    }

    #[test]
    fn inventory_penalty_reduces_score() {
        let scorer = SignalScorer::default();
        let needs_rebal = SkewResult {
            asset: "ETH".into(),
            total: Decimal::from(10),
            venues: HashMap::new(),
            max_deviation_pct: 40.0,
            needs_rebalance: true,
        };
        let ok = SkewResult {
            asset: "ETH".into(),
            total: Decimal::from(10),
            venues: HashMap::new(),
            max_deviation_pct: 1.0,
            needs_rebalance: false,
        };
        let high = scorer.score(&make_signal(100), &[ok], None);
        let low = scorer.score(&make_signal(100), &[needs_rebal], None);
        assert!(low < high);
    }

    #[test]
    fn decay_reduces_old_signal_score() {
        let scorer = SignalScorer::default();
        let mut s = make_signal(100);
        s.score = Decimal::from(100);
        // Fresh signal: decay factor ~1.0.
        let fresh = scorer.apply_decay(&s);
        // Backdate timestamp so signal looks half-aged through TTL.
        let ttl = s.expiry - s.timestamp;
        s.timestamp -= ttl / 2;
        let aged = scorer.apply_decay(&s);
        assert!(aged < fresh);
    }

    #[test]
    fn history_affects_score_after_3_records() {
        let mut scorer = SignalScorer::default();
        for _ in 0..3 {
            scorer.record_result("ETH/USDT", false);
        }
        let s = scorer.score(&make_signal(100), &[], None);
        // history sub-score drops from 50 -> 0, reducing total by 10 points.
        assert!(s < Decimal::from(75));
    }

    // ---- S5 liquidity sub-score ----------------------------------------

    /// Builds a book at `mid = 2000` with configurable ask levels (for
    /// BuyCexSellDex which walks asks).
    fn book_with_asks(levels: &[(Decimal, Decimal)]) -> OrderBookSnapshot {
        let asks: Vec<(Decimal, Decimal)> = levels.to_vec();
        let best_bid = (Decimal::from(1999), Decimal::from(10));
        let best_ask = asks.first().copied();
        OrderBookSnapshot {
            symbol: "ETH/USDT".into(),
            timestamp: 0,
            bids: vec![best_bid],
            asks,
            best_bid: Some(best_bid),
            best_ask,
            mid_price: Some(Decimal::from(2000)),
            spread_bps: Some(Decimal::from(5)),
        }
    }

    #[test]
    fn liquidity_deep_book_scores_high() {
        // Deep book: first level covers the whole size at best ask + 1 bps.
        let scorer = SignalScorer::default();
        let book = book_with_asks(&[(Decimal::from(2000), Decimal::from(10))]);
        let mut sig = make_signal(100);
        sig.size = Decimal::ONE;
        // Directly exercise score_liquidity so the test is independent of
        // weighting and other sub-scores.
        let liq = scorer.score_liquidity(&sig, Some(&book));
        assert_eq!(liq, Decimal::from(100));
    }

    #[test]
    fn liquidity_shallow_book_scores_low() {
        let scorer = SignalScorer::default();
        // 0.5 ETH at 2000, 0.5 ETH at 2020 -> avg 2010 -> slippage 50 bps.
        let book = book_with_asks(&[
            (Decimal::from(2000), Decimal::from_str_exact("0.5").unwrap()),
            (Decimal::from(2020), Decimal::from_str_exact("0.5").unwrap()),
        ]);
        let mut sig = make_signal(100);
        sig.size = Decimal::ONE;
        let liq = scorer.score_liquidity(&sig, Some(&book));
        // slippage >= max_slippage_bps (50) -> 0.
        assert_eq!(liq, Decimal::ZERO);
    }

    #[test]
    fn liquidity_missing_book_uses_fallback() {
        let scorer = SignalScorer::default();
        let sig = make_signal(100);
        let liq = scorer.score_liquidity(&sig, None);
        assert_eq!(liq, scorer.config.fallback_liquidity);
    }

    #[test]
    fn liquidity_partial_fill_returns_zero() {
        let scorer = SignalScorer::default();
        // Only 0.5 available on asks but we need 1.0.
        let book =
            book_with_asks(&[(Decimal::from(2000), Decimal::from_str_exact("0.5").unwrap())]);
        let mut sig = make_signal(100);
        sig.size = Decimal::ONE;
        let liq = scorer.score_liquidity(&sig, Some(&book));
        assert_eq!(liq, Decimal::ZERO);
    }

    #[test]
    fn liquidity_symbol_mismatch_uses_fallback() {
        let scorer = SignalScorer::default();
        let mut book = book_with_asks(&[(Decimal::from(2000), Decimal::from(10))]);
        book.symbol = "BTC/USDT".into();
        let sig = make_signal(100);
        let liq = scorer.score_liquidity(&sig, Some(&book));
        assert_eq!(liq, scorer.config.fallback_liquidity);
    }
}
