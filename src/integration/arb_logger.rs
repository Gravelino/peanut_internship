use std::path::Path;

use tracing::info;

use super::arb_checker::ArbCheckResult;

/// Accumulates arbitrage check results and exports them to CSV.
pub struct ArbLogger {
    opportunities: Vec<ArbCheckResult>,
}

impl ArbLogger {
    /// Creates a new, empty logger.
    pub fn new() -> Self {
        Self {
            opportunities: Vec::new(),
        }
    }

    /// Records an arb check result.
    pub fn log(&mut self, result: ArbCheckResult) {
        info!(
            pair = %result.pair,
            executable = result.executable,
            net_pnl_bps = %result.estimated_net_pnl_bps,
            "Logged arb check result"
        );
        self.opportunities.push(result);
    }

    /// Returns all logged results.
    pub fn opportunities(&self) -> &[ArbCheckResult] {
        &self.opportunities
    }

    /// Returns the number of logged results that are executable.
    pub fn profitable_count(&self) -> usize {
        self.opportunities.iter().filter(|r| r.executable).count()
    }

    /// Returns the total number of logged results.
    pub fn len(&self) -> usize {
        self.opportunities.len()
    }

    /// Returns `true` if no results have been logged.
    pub fn is_empty(&self) -> bool {
        self.opportunities.is_empty()
    }

    /// Writes all logged results to a CSV file at the given path.
    pub fn export_csv(&self, filepath: &str) -> std::io::Result<()> {
        let path = Path::new(filepath);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let mut wtr = csv::Writer::from_path(path)?;

        wtr.write_record([
            "timestamp",
            "pair",
            "dex_price",
            "dex_price_source",
            "cex_bid",
            "cex_ask",
            "gap_bps",
            "direction",
            "estimated_costs_bps",
            "estimated_net_pnl_bps",
            "inventory_ok",
            "executable",
            "dex_price_impact_bps",
            "cex_slippage_bps",
            "cex_fee_bps",
            "dex_fee_bps",
            "gas_cost_usd",
        ])?;

        for r in &self.opportunities {
            wtr.write_record([
                &r.timestamp,
                &r.pair,
                &r.dex_price.to_string(),
                &r.dex_price_source,
                &r.cex_bid.to_string(),
                &r.cex_ask.to_string(),
                &r.gap_bps.to_string(),
                r.direction.as_deref().unwrap_or("N/A"),
                &r.estimated_costs_bps.to_string(),
                &r.estimated_net_pnl_bps.to_string(),
                &r.inventory_ok.to_string(),
                &r.executable.to_string(),
                &r.details.dex_price_impact_bps.to_string(),
                &r.details.cex_slippage_bps.to_string(),
                &r.details.cex_fee_bps.to_string(),
                &r.details.dex_fee_bps.to_string(),
                &r.details.gas_cost_usd.to_string(),
            ])?;
        }

        wtr.flush()?;
        info!(
            path = filepath,
            count = self.opportunities.len(),
            "Arb log exported to CSV"
        );
        Ok(())
    }
}

impl Default for ArbLogger {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use rust_decimal::Decimal;

    use super::*;
    use crate::integration::arb_checker::ArbCheckDetails;

    fn make_result(pair: &str, net_pnl_bps: Decimal, executable: bool) -> ArbCheckResult {
        ArbCheckResult {
            pair: pair.to_string(),
            timestamp: "2026-04-20T12:00:00Z".to_string(),
            dex_price: Decimal::ONE,
            dex_price_source: "test".to_string(),
            cex_bid: Decimal::ONE,
            cex_ask: Decimal::ONE,
            gap_bps: Decimal::ZERO,
            direction: None,
            estimated_costs_bps: Decimal::ZERO,
            estimated_net_pnl_bps: net_pnl_bps,
            inventory_ok: true,
            executable,
            details: ArbCheckDetails {
                dex_price_impact_bps: Decimal::ZERO,
                cex_slippage_bps: Decimal::ZERO,
                cex_fee_bps: Decimal::ZERO,
                dex_fee_bps: Decimal::ZERO,
                gas_cost_usd: Decimal::ZERO,
            },
            price_sources: None,
            dex_pool_info: None,
            fork_simulation: None,
            cross_dex_opportunities: vec![],
        }
    }

    #[test]
    fn test_arb_logger_new() {
        let logger = ArbLogger::new();
        assert!(logger.is_empty());
        assert_eq!(logger.len(), 0);
    }

    #[test]
    fn test_arb_logger_log_accumulates() {
        let mut logger = ArbLogger::new();
        logger.log(make_result("ETH/USDT", Decimal::from(5), true));
        logger.log(make_result("BTC/USDT", Decimal::from(-2), false));
        assert_eq!(logger.len(), 2);
        assert_eq!(logger.profitable_count(), 1);
    }

    #[test]
    fn test_arb_logger_profitable_count() {
        let mut logger = ArbLogger::new();
        logger.log(make_result("ETH/USDT", Decimal::from(10), true));
        logger.log(make_result("ETH/USDT", Decimal::from(5), true));
        logger.log(make_result("ETH/USDT", Decimal::from(-1), false));
        assert_eq!(logger.profitable_count(), 2);
    }

    #[test]
    fn test_arb_logger_export_csv() {
        let mut logger = ArbLogger::new();
        logger.log(make_result("ETH/USDT", Decimal::from(5), true));
        logger.log(make_result("BTC/USDT", Decimal::from(-2), false));

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("arb_log.csv");
        let path_str = path.to_str().unwrap();
        logger.export_csv(path_str).unwrap();

        let content = std::fs::read_to_string(path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 3); // header + 2 rows
        assert!(lines[0].contains("timestamp"));
        assert!(lines[1].contains("ETH/USDT"));
        assert!(lines[2].contains("BTC/USDT"));
    }

    #[test]
    fn test_arb_logger_export_csv_empty() {
        let logger = ArbLogger::new();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.csv");
        logger.export_csv(path.to_str().unwrap()).unwrap();

        let content = std::fs::read_to_string(path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 1); // header only
    }

    #[test]
    fn test_arb_logger_default() {
        let logger = ArbLogger::default();
        assert!(logger.is_empty());
    }
}
