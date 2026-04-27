use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::Path;
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::core::types::BPS_SCALE;
use crate::inventory::errors::InventoryResult;
use crate::inventory::types::Venue;

/// One side (buy or sell) of an arbitrage trade.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TradeLeg {
    /// Unique identifier for this leg.
    pub id: String,
    /// Time the leg was executed.
    pub timestamp: DateTime<Utc>,
    /// Venue where the leg was executed.
    pub venue: Venue,
    /// Trading pair symbol (e.g. "ETH/USDT").
    pub symbol: String,
    /// Trade direction: "buy" or "sell".
    pub side: String,
    /// Quantity of the base asset.
    pub amount: Decimal,
    /// Execution price of the trade.
    pub price: Decimal,
    /// Fee amount charged.
    pub fee: Decimal,
    /// Asset in which the fee was charged.
    pub fee_asset: String,
}

/// A paired buy/sell arbitrage trade with associated costs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArbRecord {
    /// Unique identifier for this arb trade.
    pub id: String,
    /// Time the arb was executed.
    pub timestamp: DateTime<Utc>,
    /// The buy side of the arb.
    pub buy_leg: TradeLeg,
    /// The sell side of the arb.
    pub sell_leg: TradeLeg,
    /// On-chain gas cost in USD.
    pub gas_cost_usd: Decimal,
}

/// Append-only JSONL writer for completed arbitrage records.
#[derive(Debug)]
pub struct TradeJsonlLogger {
    writer: Mutex<BufWriter<File>>,
}

impl TradeJsonlLogger {
    /// Opens (or creates) a JSONL trade log at `path`.
    pub fn open(path: impl AsRef<Path>) -> InventoryResult<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self {
            writer: Mutex::new(BufWriter::new(file)),
        })
    }

    /// Appends one trade record as a single JSON line and flushes it.
    pub fn append(&self, trade: &ArbRecord) -> InventoryResult<()> {
        let mut guard = self
            .writer
            .lock()
            .map_err(|_| std::io::Error::other("trade log mutex poisoned"))?;
        serde_json::to_writer(&mut *guard, trade)?;
        guard.write_all(b"\n")?;
        guard.flush()?;
        Ok(())
    }
}

impl ArbRecord {
    /// Revenue from sell leg minus cost of buy leg, before fees.
    pub fn gross_pnl(&self) -> Decimal {
        let sell_revenue = self.sell_leg.amount * self.sell_leg.price;
        let buy_cost = self.buy_leg.amount * self.buy_leg.price;
        sell_revenue - buy_cost
    }

    /// Sum of buy-leg fee, sell-leg fee, and gas cost, all in USD.
    pub fn total_fees(&self) -> Decimal {
        let buy_fee_usd = if self.buy_leg.fee_asset == "USDT" || self.buy_leg.fee_asset == "USDC" {
            self.buy_leg.fee
        } else {
            self.buy_leg.fee * self.buy_leg.price
        };

        let sell_fee_usd = if self.sell_leg.fee_asset == "USDT" || self.sell_leg.fee_asset == "USDC"
        {
            self.sell_leg.fee
        } else {
            self.sell_leg.fee * self.sell_leg.price
        };

        buy_fee_usd + sell_fee_usd + self.gas_cost_usd
    }

    /// Gross PnL minus all fees.
    pub fn net_pnl(&self) -> Decimal {
        self.gross_pnl() - self.total_fees()
    }

    /// Buy-leg amount times buy price (USD notional of the trade).
    pub fn notional(&self) -> Decimal {
        self.buy_leg.amount * self.buy_leg.price
    }

    /// Net PnL expressed in basis points of notional.
    pub fn net_pnl_bps(&self) -> Decimal {
        let notional = self.notional();
        if notional.is_zero() {
            Decimal::ZERO
        } else {
            self.net_pnl() / notional * Decimal::from(BPS_SCALE)
        }
    }
}

/// Aggregate profit-and-loss statistics across all recorded arb trades.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PnLSummary {
    /// Total number of arb trades.
    pub total_trades: usize,
    /// Sum of net PnL across all trades, in USD.
    pub total_pnl_usd: Decimal,
    /// Sum of all fees paid, in USD.
    pub total_fees_usd: Decimal,
    /// Average net PnL per trade, in USD.
    pub avg_pnl_per_trade: Decimal,
    /// Average net PnL in basis points.
    pub avg_pnl_bps: Decimal,
    /// Fraction of trades with positive net PnL.
    pub win_rate: f64,
    /// Highest single-trade net PnL.
    pub best_trade_pnl: Decimal,
    /// Lowest single-trade net PnL.
    pub worst_trade_pnl: Decimal,
    /// Total notional volume traded, in USD.
    pub total_notional: Decimal,
    /// Simplified Sharpe ratio estimate (mean / stddev of per-trade PnL).
    pub sharpe_estimate: f64,
    /// Net PnL broken down by UTC hour.
    pub pnl_by_hour: HashMap<u32, Decimal>,
}

/// A condensed summary of a single arb trade for display purposes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TradeSummary {
    /// Unique identifier for this trade.
    pub id: String,
    /// Time the trade was executed.
    pub timestamp: DateTime<Utc>,
    /// Trading pair symbol.
    pub symbol: String,
    /// Venue where the buy leg was executed.
    pub buy_venue: Venue,
    /// Venue where the sell leg was executed.
    pub sell_venue: Venue,
    /// Net profit or loss, in USD.
    pub net_pnl: Decimal,
    /// Net profit or loss in basis points.
    pub net_pnl_bps: Decimal,
    /// Whether the trade was profitable.
    pub profitable: bool,
}

/// Accumulates arb trade records and computes PnL statistics.
#[derive(Debug)]
pub struct PnLEngine {
    trades: Vec<ArbRecord>,
}

impl PnLEngine {
    /// Creates an empty PnL engine.
    pub fn new() -> Self {
        Self { trades: vec![] }
    }

    /// Records a completed arb trade.
    pub fn record(&mut self, trade: ArbRecord) {
        info!(
            id = %trade.id,
            net_pnl = %trade.net_pnl(),
            bps = %trade.net_pnl_bps(),
            "Recorded arb trade"
        );
        self.trades.push(trade);
    }

    /// Computes aggregate PnL statistics over all recorded trades.
    pub fn summary(&self) -> PnLSummary {
        let n = self.trades.len();

        if n == 0 {
            return PnLSummary {
                total_trades: 0,
                total_pnl_usd: Decimal::ZERO,
                total_fees_usd: Decimal::ZERO,
                avg_pnl_per_trade: Decimal::ZERO,
                avg_pnl_bps: Decimal::ZERO,
                win_rate: 0.0,
                best_trade_pnl: Decimal::ZERO,
                worst_trade_pnl: Decimal::ZERO,
                total_notional: Decimal::ZERO,
                sharpe_estimate: 0.0,
                pnl_by_hour: HashMap::new(),
            };
        }

        let total_pnl: Decimal = self.trades.iter().map(|t| t.net_pnl()).sum();
        let total_fees: Decimal = self.trades.iter().map(|t| t.total_fees()).sum();
        let total_notional: Decimal = self.trades.iter().map(|t| t.notional()).sum();

        let wins = self
            .trades
            .iter()
            .filter(|t| t.net_pnl() > Decimal::ZERO)
            .count();
        let win_rate = wins as f64 / n as f64;

        let pnl_values: Vec<Decimal> = self.trades.iter().map(|t| t.net_pnl()).collect();
        let best = pnl_values.iter().max().copied().unwrap_or(Decimal::ZERO);
        let worst = pnl_values.iter().min().copied().unwrap_or(Decimal::ZERO);

        let avg_pnl = total_pnl / Decimal::from(n as i32);
        let avg_bps: Decimal = if total_notional > Decimal::ZERO {
            self.trades.iter().map(|t| t.net_pnl_bps()).sum::<Decimal>() / Decimal::from(n as i32)
        } else {
            Decimal::ZERO
        };

        let mean = total_pnl.to_f64().unwrap_or_else(|| {
            warn!("PnL total too large for f64 Sharpe calculation, using 0.0");
            0.0
        }) / n as f64;
        let variance: f64 = pnl_values
            .iter()
            .map(|p| {
                let p_f64 = p.to_f64().unwrap_or_else(|| {
                    warn!("PnL value too large for f64, treating as 0.0 in Sharpe");
                    0.0
                });
                let diff = p_f64 - mean;
                diff * diff
            })
            .sum::<f64>()
            / n as f64;
        let stddev = variance.sqrt();
        let sharpe = if stddev > 0.0 { mean / stddev } else { 0.0 };

        let mut pnl_by_hour: HashMap<u32, Decimal> = HashMap::new();
        for trade in &self.trades {
            let hour = match trade.timestamp.format("%H").to_string().parse::<u32>() {
                Ok(h) => h,
                Err(_) => {
                    warn!("Failed to parse hour from timestamp, skipping");
                    continue;
                }
            };
            *pnl_by_hour.entry(hour).or_insert(Decimal::ZERO) += trade.net_pnl();
        }

        PnLSummary {
            total_trades: n,
            total_pnl_usd: total_pnl,
            total_fees_usd: total_fees,
            avg_pnl_per_trade: avg_pnl,
            avg_pnl_bps: avg_bps,
            win_rate,
            best_trade_pnl: best,
            worst_trade_pnl: worst,
            total_notional,
            sharpe_estimate: sharpe,
            pnl_by_hour,
        }
    }

    /// Returns the `n` most recent trades as concise summaries, newest first.
    pub fn recent(&self, n: usize) -> Vec<TradeSummary> {
        self.trades
            .iter()
            .rev()
            .take(n)
            .map(|t| TradeSummary {
                id: t.id.clone(),
                timestamp: t.timestamp,
                symbol: t.buy_leg.symbol.clone(),
                buy_venue: t.buy_leg.venue,
                sell_venue: t.sell_leg.venue,
                net_pnl: t.net_pnl(),
                net_pnl_bps: t.net_pnl_bps(),
                profitable: t.net_pnl() > Decimal::ZERO,
            })
            .collect()
    }

    /// Writes all recorded trades to a CSV file at the given path.
    pub fn export_csv(&self, filepath: &str) -> crate::inventory::errors::InventoryResult<()> {
        let path = Path::new(filepath);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let mut wtr = csv::Writer::from_path(path)?;

        wtr.write_record([
            "id",
            "timestamp",
            "symbol",
            "buy_venue",
            "buy_side",
            "buy_amount",
            "buy_price",
            "buy_fee",
            "buy_fee_asset",
            "sell_venue",
            "sell_side",
            "sell_amount",
            "sell_price",
            "sell_fee",
            "sell_fee_asset",
            "gas_cost_usd",
            "gross_pnl",
            "total_fees",
            "net_pnl",
            "net_pnl_bps",
            "notional",
        ])?;

        for t in &self.trades {
            wtr.write_record([
                &t.id,
                &t.timestamp.to_rfc3339(),
                &t.buy_leg.symbol,
                &t.buy_leg.venue.to_string(),
                &t.buy_leg.side,
                &t.buy_leg.amount.to_string(),
                &t.buy_leg.price.to_string(),
                &t.buy_leg.fee.to_string(),
                &t.buy_leg.fee_asset,
                &t.sell_leg.venue.to_string(),
                &t.sell_leg.side,
                &t.sell_leg.amount.to_string(),
                &t.sell_leg.price.to_string(),
                &t.sell_leg.fee.to_string(),
                &t.sell_leg.fee_asset,
                &t.gas_cost_usd.to_string(),
                &t.gross_pnl().to_string(),
                &t.total_fees().to_string(),
                &t.net_pnl().to_string(),
                &t.net_pnl_bps().to_string(),
                &t.notional().to_string(),
            ])?;
        }

        wtr.flush()?;
        Ok(())
    }

    /// Returns a slice of all recorded arb trades in chronological order.
    pub fn trades(&self) -> &[ArbRecord] {
        &self.trades
    }
}

impl Default for PnLEngine {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_leg(id: &str, side: &str, amount: Decimal, price: Decimal, fee: Decimal) -> TradeLeg {
        TradeLeg {
            id: id.into(),
            timestamp: Utc::now(),
            venue: Venue::Binance,
            symbol: "ETH/USDT".into(),
            side: side.into(),
            amount,
            price,
            fee,
            fee_asset: "USDT".into(),
        }
    }

    fn make_arb(
        id: &str,
        buy_price: Decimal,
        sell_price: Decimal,
        amount: Decimal,
        gas: Decimal,
    ) -> ArbRecord {
        let buy_fee = amount * buy_price * Decimal::from_str_exact("0.001").unwrap();
        let sell_fee = amount * sell_price * Decimal::from_str_exact("0.001").unwrap();
        ArbRecord {
            id: id.into(),
            timestamp: Utc::now(),
            buy_leg: make_leg(&format!("{id}_buy"), "buy", amount, buy_price, buy_fee),
            sell_leg: make_leg(&format!("{id}_sell"), "sell", amount, sell_price, sell_fee),
            gas_cost_usd: gas,
        }
    }

    #[test]
    fn test_gross_pnl_calculation() {
        let arb = make_arb(
            "1",
            Decimal::from(2000),
            Decimal::from(2010),
            Decimal::from(2),
            Decimal::ZERO,
        );
        let expected =
            (Decimal::from(2) * Decimal::from(2010)) - (Decimal::from(2) * Decimal::from(2000));
        assert_eq!(arb.gross_pnl(), expected);
    }

    #[test]
    fn test_net_pnl_includes_all_fees() {
        let arb = make_arb(
            "1",
            Decimal::from(2000),
            Decimal::from(2010),
            Decimal::from(2),
            Decimal::from(5),
        );
        let gross = arb.gross_pnl();
        let fees = arb.total_fees();
        assert_eq!(arb.net_pnl(), gross - fees);
        assert!(fees > Decimal::ZERO);
    }

    #[test]
    fn test_pnl_bps_calculation() {
        let arb = make_arb(
            "1",
            Decimal::from(2000),
            Decimal::from(2020),
            Decimal::from(1),
            Decimal::ZERO,
        );
        let notional = arb.notional();
        assert_eq!(notional, Decimal::from(2000));
        let bps = arb.net_pnl_bps();
        assert!(bps > Decimal::ZERO);
    }

    #[test]
    fn test_summary_win_rate() {
        let mut engine = PnLEngine::new();
        engine.record(make_arb(
            "1",
            Decimal::from(2000),
            Decimal::from(2010),
            Decimal::from(1),
            Decimal::ZERO,
        ));
        engine.record(make_arb(
            "2",
            Decimal::from(2010),
            Decimal::from(2000),
            Decimal::from(1),
            Decimal::ZERO,
        ));
        engine.record(make_arb(
            "3",
            Decimal::from(2000),
            Decimal::from(2015),
            Decimal::from(1),
            Decimal::ZERO,
        ));

        let summary = engine.summary();
        assert_eq!(summary.total_trades, 3);
        assert!(summary.win_rate > 0.0);
        assert!(summary.win_rate <= 1.0);
    }

    #[test]
    fn test_summary_with_no_trades() {
        let engine = PnLEngine::new();
        let summary = engine.summary();
        assert_eq!(summary.total_trades, 0);
        assert_eq!(summary.total_pnl_usd, Decimal::ZERO);
        assert!((summary.win_rate - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_export_csv_format() {
        let mut engine = PnLEngine::new();
        engine.record(make_arb(
            "1",
            Decimal::from(2000),
            Decimal::from(2010),
            Decimal::from(1),
            Decimal::ZERO,
        ));

        let dir = std::env::temp_dir().join("pnl_test_export.csv");
        let path = dir.to_str().unwrap();
        let result = engine.export_csv(path);
        assert!(result.is_ok());

        let content = std::fs::read_to_string(path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert!(lines.len() >= 2);
        let header: Vec<&str> = lines[0].split(',').collect();
        assert!(header.contains(&"id"));
        assert!(header.contains(&"net_pnl"));
    }

    #[test]
    fn test_arb_record_total_fees() {
        let arb = make_arb(
            "1",
            Decimal::from(2000),
            Decimal::from(2010),
            Decimal::from(2),
            Decimal::from(5),
        );
        let fees = arb.total_fees();
        assert!(fees > Decimal::ZERO);
        assert_eq!(fees, arb.buy_leg.fee + arb.sell_leg.fee + arb.gas_cost_usd);
    }

    #[test]
    fn test_arb_record_net_pnl_with_loss() {
        let arb = make_arb(
            "1",
            Decimal::from(2010),
            Decimal::from(2000),
            Decimal::from(1),
            Decimal::from(1),
        );
        assert!(arb.net_pnl() < Decimal::ZERO);
    }

    #[test]
    fn test_arb_record_notional() {
        let arb = make_arb(
            "1",
            Decimal::from(2000),
            Decimal::from(2010),
            Decimal::from(3),
            Decimal::ZERO,
        );
        assert_eq!(arb.notional(), Decimal::from(3) * Decimal::from(2000));
    }

    #[test]
    fn test_arb_record_net_pnl_bps() {
        let arb = make_arb(
            "1",
            Decimal::from(2000),
            Decimal::from(2020),
            Decimal::from(1),
            Decimal::ZERO,
        );
        let bps = arb.net_pnl_bps();
        assert!(bps > Decimal::ZERO);
    }

    #[test]
    fn test_pnl_engine_summary_single_winning_trade() {
        let mut engine = PnLEngine::new();
        engine.record(make_arb(
            "1",
            Decimal::from(2000),
            Decimal::from(2010),
            Decimal::from(1),
            Decimal::ZERO,
        ));
        let summary = engine.summary();
        assert_eq!(summary.total_trades, 1);
        assert!(summary.win_rate > 0.0);
        assert!(summary.total_pnl_usd > Decimal::ZERO);
    }

    #[test]
    fn test_pnl_engine_summary_single_losing_trade() {
        let mut engine = PnLEngine::new();
        engine.record(make_arb(
            "1",
            Decimal::from(2010),
            Decimal::from(2000),
            Decimal::from(1),
            Decimal::from(10),
        ));
        let summary = engine.summary();
        assert_eq!(summary.total_trades, 1);
        assert!(summary.win_rate == 0.0);
        assert!(summary.total_pnl_usd < Decimal::ZERO);
    }

    #[test]
    fn test_trade_leg_fields() {
        let leg = make_leg(
            "test",
            "buy",
            Decimal::from(2),
            Decimal::from(2000),
            Decimal::from(4),
        );
        assert_eq!(leg.id, "test");
        assert_eq!(leg.side, "buy");
        assert_eq!(leg.amount, Decimal::from(2));
        assert_eq!(leg.price, Decimal::from(2000));
        assert_eq!(leg.fee, Decimal::from(4));
        assert_eq!(leg.fee_asset, "USDT");
    }

    #[test]
    fn test_pnl_summary_default_values() {
        let summary = PnLSummary::default();
        assert_eq!(summary.total_trades, 0);
        assert_eq!(summary.total_pnl_usd, Decimal::ZERO);
        assert!((summary.win_rate - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn trade_jsonl_logger_appends_one_json_object_per_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trades.jsonl");
        let logger = TradeJsonlLogger::open(&path).unwrap();

        logger
            .append(&make_arb(
                "t1",
                Decimal::from(2000),
                Decimal::from(2010),
                Decimal::ONE,
                Decimal::ZERO,
            ))
            .unwrap();
        logger
            .append(&make_arb(
                "t2",
                Decimal::from(2000),
                Decimal::from(2020),
                Decimal::ONE,
                Decimal::ZERO,
            ))
            .unwrap();

        let body = std::fs::read_to_string(path).unwrap();
        let lines: Vec<_> = body.lines().collect();
        assert_eq!(lines.len(), 2);
        let first: ArbRecord = serde_json::from_str(lines[0]).unwrap();
        let second: ArbRecord = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(first.id, "t1");
        assert_eq!(second.id, "t2");
    }
}
