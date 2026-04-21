use std::path::Path;

use plotters::prelude::*;
use tracing::info;

use crate::inventory::errors::{InventoryError, InventoryResult};
use crate::inventory::pnl::ArbRecord;

/// Exports PnL data to visual chart formats.
pub struct PnLChartExporter;

/// Chart title shown in the output.
const CHART_TITLE: &str = "Cumulative PnL Over Time";
/// Width of SVG chart in pixels.
const SVG_WIDTH: u32 = 1200;
/// Height of SVG chart in pixels.
const SVG_HEIGHT: u32 = 600;

impl PnLChartExporter {
    /// Exports cumulative PnL over time as a self-contained HTML file with Plotly.js.
    ///
    /// The HTML file embeds trade data as inline JSON and loads Plotly from CDN.
    /// Open the file in any browser for interactive zoom, hover, and pan.
    pub fn export_html(trades: &[ArbRecord], filepath: &str) -> InventoryResult<()> {
        let path = Path::new(filepath);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let (timestamps, cum_pnl) = cumulative_pnl_series(trades);

        let x_json = serde_json::to_string(&timestamps)
            .map_err(|e| InventoryError::Io(std::io::Error::other(e.to_string())))?;
        let y_json = serde_json::to_string(&cum_pnl)
            .map_err(|e| InventoryError::Io(std::io::Error::other(e.to_string())))?;

        let html = format!(
            r##"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>PnL Chart</title>
<script src="https://cdn.plot.ly/plotly-2.35.0.min.js"></script>
<style>body{{margin:0;padding:20px;font-family:sans-serif}}</style>
</head>
<body>
<h2>Cumulative PnL Over Time</h2>
<div id="chart" style="width:100%;height:80vh;"></div>
<script>
Plotly.newPlot('chart', [{{
  x: {x_json},
  y: {y_json},
  type: 'scatter',
  mode: 'lines',
  name: 'Cumulative PnL (USD)',
  line: {{ color: '#2196F3', width: 2 }},
  fill: 'tozeroy',
  fillcolor: 'rgba(33,150,243,0.1)'
}}], {{
  title: 'Cumulative PnL',
  xaxis: {{ title: 'Time' }},
  yaxis: {{ title: 'PnL (USD)', zeroline: true, zerolinecolor: '#999' }},
  template: 'plotly_white'
}});
</script>
</body>
</html>"##
        );

        std::fs::write(path, html)?;
        info!(
            path = filepath,
            trades = trades.len(),
            "PnL chart exported as HTML"
        );
        Ok(())
    }

    /// Exports cumulative PnL over time as an SVG file using `plotters`.
    pub fn export_svg(trades: &[ArbRecord], filepath: &str) -> InventoryResult<()> {
        let path = Path::new(filepath);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let (_, cum_pnl) = cumulative_pnl_series(trades);

        if cum_pnl.is_empty() {
            write_empty_svg(path)?;
            info!(
                path = filepath,
                "PnL chart exported as empty SVG (no trades)"
            );
            return Ok(());
        }

        let min_pnl = cum_pnl.iter().cloned().fold(f64::INFINITY, f64::min);
        let max_pnl = cum_pnl.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        let y_min = min_pnl.min(0.0);
        let y_max = max_pnl.max(0.0);
        let y_range = y_max - y_min;
        let y_pad = if y_range == 0.0 { 1.0 } else { y_range * 0.1 };

        let n = cum_pnl.len();

        let root = SVGBackend::new(path, (SVG_WIDTH, SVG_HEIGHT)).into_drawing_area();

        root.fill(&WHITE)
            .map_err(|e| InventoryError::Io(std::io::Error::other(e.to_string())))?;

        let mut chart = ChartBuilder::on(&root)
            .caption(CHART_TITLE, ("sans-serif", 20).into_font())
            .margin(10)
            .x_label_area_size(40)
            .y_label_area_size(50)
            .build_cartesian_2d(0f64..n as f64, (y_min - y_pad)..(y_max + y_pad))
            .map_err(|e| InventoryError::Io(std::io::Error::other(e.to_string())))?;

        chart
            .configure_mesh()
            .x_desc("Trade #")
            .y_desc("Cumulative PnL (USD)")
            .draw()
            .map_err(|e| InventoryError::Io(std::io::Error::other(e.to_string())))?;

        let data: Vec<(f64, f64)> = cum_pnl
            .iter()
            .enumerate()
            .map(|(i, &v)| (i as f64, v))
            .collect();

        chart
            .draw_series(LineSeries::new(data, &BLUE))
            .map_err(|e| InventoryError::Io(std::io::Error::other(e.to_string())))?;

        root.present()
            .map_err(|e| InventoryError::Io(std::io::Error::other(e.to_string())))?;

        info!(
            path = filepath,
            trades = trades.len(),
            "PnL chart exported as SVG"
        );
        Ok(())
    }
}

/// Writes a minimal empty SVG when there are no trades.
fn write_empty_svg(path: &Path) -> InventoryResult<()> {
    let svg = format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="{SVG_WIDTH}" height="{SVG_HEIGHT}">
<rect width="100%" height="100%" fill="white"/>
<text x="50%" y="50%" text-anchor="middle" font-family="sans-serif" font-size="16" fill="grey">No trade data</text>
</svg>"#
    );
    std::fs::write(path, svg)?;
    Ok(())
}

/// Computes cumulative PnL series from trade records.
///
/// Returns `(timestamps, cumulative_pnl)` where each entry is a running sum.
fn cumulative_pnl_series(trades: &[ArbRecord]) -> (Vec<String>, Vec<f64>) {
    let mut timestamps = Vec::with_capacity(trades.len());
    let mut cum_pnl = Vec::with_capacity(trades.len());
    let mut running: f64 = 0.0;

    for t in trades {
        running += rust_decimal::prelude::ToPrimitive::to_f64(&t.net_pnl()).unwrap_or(0.0);
        timestamps.push(t.timestamp.to_rfc3339());
        cum_pnl.push(running);
    }

    (timestamps, cum_pnl)
}

#[cfg(test)]
mod tests {
    use rust_decimal::Decimal;

    use super::*;
    use crate::inventory::pnl::TradeLeg;
    use crate::inventory::types::Venue;
    use chrono::Utc;

    fn make_trade(id: &str, net_pnl: Decimal, minutes_ago: i64) -> ArbRecord {
        ArbRecord {
            id: id.to_string(),
            timestamp: Utc::now() - chrono::Duration::minutes(minutes_ago),
            buy_leg: TradeLeg {
                id: format!("{id}_buy"),
                timestamp: Utc::now(),
                venue: Venue::Binance,
                symbol: "ETH/USDT".to_string(),
                side: "buy".to_string(),
                amount: Decimal::ONE,
                price: Decimal::from(2000),
                fee: Decimal::ZERO,
                fee_asset: "USDT".to_string(),
            },
            sell_leg: TradeLeg {
                id: format!("{id}_sell"),
                timestamp: Utc::now(),
                venue: Venue::Wallet,
                symbol: "ETH/USDT".to_string(),
                side: "sell".to_string(),
                amount: Decimal::ONE,
                price: Decimal::from(2000) + net_pnl,
                fee: Decimal::ZERO,
                fee_asset: "USDT".to_string(),
            },
            gas_cost_usd: Decimal::ZERO,
        }
    }

    #[test]
    fn test_cumulative_pnl_series_empty() {
        let (ts, pnl) = cumulative_pnl_series(&[]);
        assert!(ts.is_empty());
        assert!(pnl.is_empty());
    }

    #[test]
    fn test_cumulative_pnl_series_single() {
        let trades = vec![make_trade("1", Decimal::from(5), 10)];
        let (ts, pnl) = cumulative_pnl_series(&trades);
        assert_eq!(ts.len(), 1);
        assert_eq!(pnl.len(), 1);
        assert!(pnl[0] > 0.0);
    }

    #[test]
    fn test_cumulative_pnl_series_multiple() {
        let trades = vec![
            make_trade("1", Decimal::from(10), 30),
            make_trade("2", Decimal::from(-3), 20),
            make_trade("3", Decimal::from(7), 10),
        ];
        let (_, pnl) = cumulative_pnl_series(&trades);
        assert_eq!(pnl.len(), 3);
        assert!(pnl[0] > 0.0);
        assert!(pnl[1] < pnl[0]);
        assert!(pnl[2] > pnl[1]);
    }

    #[test]
    fn test_export_html_creates_file() {
        let trades = vec![make_trade("1", Decimal::from(5), 10)];
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pnl.html");
        PnLChartExporter::export_html(&trades, path.to_str().unwrap()).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("<!DOCTYPE html>"));
        assert!(content.contains("plotly"));
    }

    #[test]
    fn test_export_html_empty_trades() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.html");
        PnLChartExporter::export_html(&[], path.to_str().unwrap()).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("<!DOCTYPE html>"));
    }

    #[test]
    fn test_export_svg_creates_file() {
        let trades = vec![make_trade("1", Decimal::from(5), 10)];
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pnl.svg");
        PnLChartExporter::export_svg(&trades, path.to_str().unwrap()).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("<svg"));
    }

    #[test]
    fn test_export_svg_empty_trades() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.svg");
        PnLChartExporter::export_svg(&[], path.to_str().unwrap()).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("<svg"));
        assert!(content.contains("No trade data"));
    }
}
