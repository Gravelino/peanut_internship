use std::collections::HashMap;
use std::io;
use std::sync::mpsc;
use std::time::Duration;

use crossterm::ExecutableCommand;
use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, Wrap};

use clap::Parser;
use peanut_internship_rust::exchange::config::BinanceConfig;
use peanut_internship_rust::exchange::ws::{DepthEvent, LocalOrderBook};
use peanut_internship_rust::exchange::{BINANCE_TESTNET_WS_URL, ExchangeClient, OrderBookAnalyzer};
use peanut_internship_rust::inventory::pnl::PnLSummary;
use peanut_internship_rust::inventory::tracker::InventoryTracker;
use peanut_internship_rust::inventory::types::Venue;
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;

/// Refresh interval in milliseconds for polling the TUI events.
const TICK_RATE_MS: u64 = 250;
/// Number of top-of-book levels to display in the order book panel.
const ORDERBOOK_DISPLAY_LEVELS: usize = 5;
/// Interval in seconds between CEX balance refresh attempts.
const BALANCE_REFRESH_INTERVAL_SECS: u64 = 30;

#[derive(Parser)]
#[command(name = "dashboard")]
#[command(about = "Real-time inventory dashboard (TUI)")]
struct Cli {
    /// Trading pair for order book (e.g. ETHUSDT)
    #[arg(long, default_value = "ETHUSDT")]
    pair: String,

    /// Binance WebSocket URL
    #[arg(long, default_value = BINANCE_TESTNET_WS_URL)]
    ws_url: String,
}

/// Snapshot of all dashboard data for rendering (no async locks needed).
struct DashboardData {
    pair: String,
    balance_rows: Vec<BalanceRow>,
    skew_rows: Vec<SkewRow>,
    ob_asks: Vec<(Decimal, Decimal)>,
    ob_bids: Vec<(Decimal, Decimal)>,
    ob_mid: Option<Decimal>,
    ob_spread_bps: Option<Decimal>,
    ob_imbalance: f64,
    pnl_summary: PnLSummary,
    last_refresh: String,
}

struct BalanceRow {
    asset: String,
    total: Decimal,
    binance: Decimal,
    wallet: Decimal,
    usd_val: Decimal,
}

struct SkewRow {
    asset: String,
    max_deviation_pct: f64,
    binance_pct: Option<f64>,
    wallet_pct: Option<f64>,
    needs_rebalance: bool,
}

impl Default for DashboardData {
    fn default() -> Self {
        Self {
            pair: String::new(),
            balance_rows: Vec::new(),
            skew_rows: Vec::new(),
            ob_asks: Vec::new(),
            ob_bids: Vec::new(),
            ob_mid: None,
            ob_spread_bps: None,
            ob_imbalance: 0.0,
            pnl_summary: PnLSummary::default(),
            last_refresh: "never".into(),
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    let config = BinanceConfig::from_env().unwrap_or_else(|e| {
        eprintln!("Binance config error: {e}. Using testnet defaults.");
        BinanceConfig::with_custom_url(
            String::new(),
            String::new(),
            peanut_internship_rust::exchange::BINANCE_TESTNET_BASE_URL.to_string(),
        )
    });

    let (tx, rx) = mpsc::channel::<DashboardData>();

    let pair = cli.pair.clone();
    let ws_url = cli.ws_url.clone();

    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");

        rt.block_on(async move {
            let tracker =
                std::sync::Arc::new(tokio::sync::Mutex::new(InventoryTracker::new(vec![
                    Venue::Binance,
                    Venue::Wallet,
                ])));
            let book = std::sync::Arc::new(tokio::sync::Mutex::new(LocalOrderBook::new(&pair)));
            let prices =
                std::sync::Arc::new(tokio::sync::Mutex::new(HashMap::<String, Decimal>::new()));
            let pnl_engine = std::sync::Arc::new(tokio::sync::Mutex::new(
                peanut_internship_rust::inventory::pnl::PnLEngine::new(),
            ));
            let last_refresh = std::sync::Arc::new(tokio::sync::Mutex::new("never".to_string()));

            let ws_book = book.clone();
            let ws_pair = pair.clone();
            let ws_handle = tokio::spawn(async move {
                let rx_result =
                    peanut_internship_rust::exchange::ws::subscribe_depth_stream(&ws_url, &ws_pair)
                        .await;
                if let Ok(mut ws_rx) = rx_result {
                    while let Some(event) = ws_rx.recv().await {
                        let mut b = ws_book.lock().await;
                        match event {
                            DepthEvent::Snapshot(snap) => {
                                b.apply_snapshot(snap);
                            }
                            DepthEvent::Update(update) => {
                                use peanut_internship_rust::exchange::ws::SequenceStatus;
                                match b.apply_update(update) {
                                    SequenceStatus::Applied => {}
                                    SequenceStatus::Stale => continue,
                                    SequenceStatus::NeedsReconnect => break,
                                }
                            }
                        }
                    }
                }
            });

            let bal_tracker = tracker.clone();
            let bal_prices = prices.clone();
            let bal_last = last_refresh.clone();
            let bal_config = config;
            let balance_handle = tokio::spawn(async move {
                let client = match ExchangeClient::new(bal_config) {
                    Ok(c) => c,
                    Err(_) => return,
                };
                loop {
                    if let Ok(balances) = client.fetch_balance().await {
                        let mut t = bal_tracker.lock().await;
                        t.update_from_cex(Venue::Binance, balances);

                        let default_eth_price =
                            Decimal::from_str_exact("2000").expect("valid Decimal literal");
                        {
                            let mut p = bal_prices.lock().await;
                            if !p.contains_key("ETH") {
                                p.insert("ETH".into(), default_eth_price);
                            }
                            if !p.contains_key("USDT") {
                                p.insert("USDT".into(), Decimal::ONE);
                            }
                        }

                        let mut l = bal_last.lock().await;
                        *l = chrono::Utc::now().format("%H:%M:%S UTC").to_string();
                    }
                    tokio::time::sleep(Duration::from_secs(BALANCE_REFRESH_INTERVAL_SECS)).await;
                }
            });

            loop {
                tokio::time::sleep(Duration::from_millis(TICK_RATE_MS)).await;

                let t = tracker.lock().await;
                let p = prices.lock().await;
                let b = book.lock().await;
                let pnl = pnl_engine.lock().await;
                let lr = last_refresh.lock().await;

                let snap = t.snapshot(&p);
                let ob = b.snapshot();
                let analyzer = OrderBookAnalyzer::new(ob);
                let ob_ref = analyzer.orderbook();
                let skews = t.get_skews();
                let pnl_summary = pnl.summary();

                let mut balance_rows = Vec::new();
                let mut assets: Vec<&String> = snap.totals.keys().collect();
                assets.sort();
                for asset in &assets {
                    let total = snap.totals.get(*asset).copied().unwrap_or(Decimal::ZERO);
                    let price = p.get(*asset).copied().unwrap_or(Decimal::ZERO);
                    let binance = snap
                        .venues
                        .get("binance")
                        .and_then(|m| m.get(*asset))
                        .map(|b| b.total)
                        .unwrap_or(Decimal::ZERO);
                    let wallet = snap
                        .venues
                        .get("wallet")
                        .and_then(|m| m.get(*asset))
                        .map(|b| b.total)
                        .unwrap_or(Decimal::ZERO);

                    balance_rows.push(BalanceRow {
                        asset: asset.to_string(),
                        total,
                        binance,
                        wallet,
                        usd_val: total * price,
                    });
                }

                let mut skew_rows = Vec::new();
                for s in &skews {
                    skew_rows.push(SkewRow {
                        asset: s.asset.clone(),
                        max_deviation_pct: s.max_deviation_pct,
                        binance_pct: s.venues.get("binance").map(|v| v.pct),
                        wallet_pct: s.venues.get("wallet").map(|v| v.pct),
                        needs_rebalance: s.needs_rebalance,
                    });
                }

                let ask_count = ob_ref.asks.len().min(ORDERBOOK_DISPLAY_LEVELS);
                let bid_count = ob_ref.bids.len().min(ORDERBOOK_DISPLAY_LEVELS);
                let ob_asks: Vec<(Decimal, Decimal)> = ob_ref.asks[..ask_count].to_vec();
                let ob_bids: Vec<(Decimal, Decimal)> = ob_ref.bids[..bid_count].to_vec();

                let data = DashboardData {
                    pair: pair.clone(),
                    balance_rows,
                    skew_rows,
                    ob_asks,
                    ob_bids,
                    ob_mid: ob_ref.mid_price,
                    ob_spread_bps: ob_ref.spread_bps,
                    ob_imbalance: analyzer.imbalance(10),
                    pnl_summary,
                    last_refresh: lr.clone(),
                };

                drop(t);
                drop(p);
                drop(b);
                drop(pnl);
                drop(lr);

                if tx.send(data).is_err() {
                    break;
                }
            }

            ws_handle.abort();
            balance_handle.abort();
        });
    });

    enable_raw_mode()?;
    io::stdout().execute(EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;

    let mut data = DashboardData {
        pair: cli.pair.clone(),
        ..DashboardData::default()
    };

    loop {
        while let Ok(d) = rx.try_recv() {
            data = d;
        }

        terminal.draw(|f| {
            let size = f.area();
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(3),
                    Constraint::Min(10),
                    Constraint::Length(3),
                ])
                .split(size);

            let header = render_header(&data);
            f.render_widget(header, chunks[0]);

            let middle = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
                .split(chunks[1]);

            let left = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
                .split(middle[0]);

            let balances = render_balances(&data);
            f.render_widget(balances, left[0]);

            let skews = render_skews(&data);
            f.render_widget(skews, left[1]);

            let right = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
                .split(middle[1]);

            let orderbook = render_orderbook(&data);
            f.render_widget(orderbook, right[0]);

            let pnl = render_pnl(&data);
            f.render_widget(pnl, right[1]);

            let footer = render_footer(&data);
            f.render_widget(footer, chunks[2]);
        })?;

        if let Ok(Event::Key(key)) =
            event::poll(Duration::from_millis(TICK_RATE_MS)).and_then(|_| event::read())
        {
            if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
                break;
            }
            if key.code == KeyCode::Char('q') {
                break;
            }
        }
    }

    disable_raw_mode()?;
    io::stdout().execute(LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    Ok(())
}

fn render_header(data: &DashboardData) -> Paragraph<'static> {
    let now = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S UTC");
    let pair = data.pair.clone();
    Paragraph::new(Line::from(vec![
        Span::styled(
            " INVENTORY DASHBOARD ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            pair,
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(now.to_string(), Style::default().fg(Color::DarkGray)),
    ]))
    .block(Block::default().borders(Borders::BOTTOM))
}

fn render_balances(data: &DashboardData) -> Table<'static> {
    let rows: Vec<Row> = data
        .balance_rows
        .iter()
        .map(|r| {
            let usd_cell = if r.usd_val > Decimal::ZERO {
                Cell::from(format!("${}", fmt_dec(r.usd_val, 2)))
            } else {
                Cell::from("-")
            };
            Row::new(vec![
                Cell::from(r.asset.clone()).style(Style::default().fg(Color::Cyan)),
                Cell::from(fmt_total(r.total)),
                Cell::from(fmt_total(r.binance)),
                Cell::from(fmt_total(r.wallet)),
                usd_cell,
            ])
        })
        .collect();

    let header = Row::new(vec![
        Cell::from("Asset").style(Style::default().add_modifier(Modifier::BOLD)),
        Cell::from("Total").style(Style::default().add_modifier(Modifier::BOLD)),
        Cell::from("Binance").style(Style::default().add_modifier(Modifier::BOLD)),
        Cell::from("Wallet").style(Style::default().add_modifier(Modifier::BOLD)),
        Cell::from("USD Val").style(Style::default().add_modifier(Modifier::BOLD)),
    ])
    .style(Style::default().bg(Color::DarkGray));

    Table::new(
        rows,
        [
            Constraint::Length(6),
            Constraint::Length(12),
            Constraint::Length(12),
            Constraint::Length(12),
            Constraint::Length(12),
        ],
    )
    .block(Block::default().title(" Balances ").borders(Borders::ALL))
    .header(header)
}

fn render_skews(data: &DashboardData) -> Table<'static> {
    let rows: Vec<Row> = data
        .skew_rows
        .iter()
        .map(|s| {
            let status_style = if s.needs_rebalance {
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Green)
            };

            Row::new(vec![
                Cell::from(s.asset.clone()).style(Style::default().fg(Color::Cyan)),
                Cell::from(format!("{:.1}%", s.max_deviation_pct)),
                Cell::from(fmt_pct(s.binance_pct)),
                Cell::from(fmt_pct(s.wallet_pct)),
                if s.needs_rebalance {
                    Cell::from("REBAL").style(status_style)
                } else {
                    Cell::from("OK").style(status_style)
                },
            ])
        })
        .collect();

    let header = Row::new(vec![
        Cell::from("Asset").style(Style::default().add_modifier(Modifier::BOLD)),
        Cell::from("MaxDev%").style(Style::default().add_modifier(Modifier::BOLD)),
        Cell::from("Binance%").style(Style::default().add_modifier(Modifier::BOLD)),
        Cell::from("Wallet%").style(Style::default().add_modifier(Modifier::BOLD)),
        Cell::from("Status").style(Style::default().add_modifier(Modifier::BOLD)),
    ])
    .style(Style::default().bg(Color::DarkGray));

    Table::new(
        rows,
        [
            Constraint::Length(6),
            Constraint::Length(9),
            Constraint::Length(9),
            Constraint::Length(9),
            Constraint::Length(7),
        ],
    )
    .block(
        Block::default()
            .title(" Skew Analysis ")
            .borders(Borders::ALL),
    )
    .header(header)
}

fn render_orderbook(data: &DashboardData) -> Table<'static> {
    let mid_str = data
        .ob_mid
        .map(|m| fmt_dec(m, 2))
        .unwrap_or_else(|| "N/A".into());
    let spread_str = data
        .ob_spread_bps
        .map(|s| format!("{s:.2}"))
        .unwrap_or_else(|| "N/A".into());
    let imb = data.ob_imbalance;

    let mut rows: Vec<Row> = Vec::new();

    for (price, qty) in data.ob_asks.iter().rev() {
        rows.push(Row::new(vec![
            Cell::from(fmt_dec(*price, 2)).style(Style::default().fg(Color::Red)),
            Cell::from(fmt_dec(*qty, 4)),
        ]));
    }

    rows.push(
        Row::new(vec![
            Cell::from(format!("Mid: {mid_str}")).style(
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Cell::from(format!("Spd: {spread_str} bps"))
                .style(Style::default().fg(Color::DarkGray)),
        ])
        .style(Style::default().bg(Color::DarkGray)),
    );

    for (price, qty) in &data.ob_bids {
        rows.push(Row::new(vec![
            Cell::from(fmt_dec(*price, 2)).style(Style::default().fg(Color::Green)),
            Cell::from(fmt_dec(*qty, 4)),
        ]));
    }

    let arrow = if imb > 0.1 {
        "▲"
    } else if imb < -0.1 {
        "▼"
    } else {
        "─"
    };

    let title = format!(" Order Book: {} Imb:{arrow}{imb:.2} ", data.pair);

    Table::new(rows, [Constraint::Length(14), Constraint::Length(14)])
        .block(Block::default().title(title).borders(Borders::ALL))
}

fn render_pnl(data: &DashboardData) -> Paragraph<'static> {
    let summary = &data.pnl_summary;

    let pnl_color = if summary.total_pnl_usd > Decimal::ZERO {
        Color::Green
    } else if summary.total_pnl_usd < Decimal::ZERO {
        Color::Red
    } else {
        Color::White
    };

    let win_pct = summary.win_rate * 100.0;
    let sharpe_str = format!("{:.2}", summary.sharpe_estimate);

    let lines = vec![
        Line::from(vec![
            Span::styled("Trades: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("{}", summary.total_trades),
                Style::default().fg(Color::White),
            ),
            Span::raw("  "),
            Span::styled("Win%: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("{win_pct:.0}%"),
                if win_pct > 50.0 {
                    Style::default().fg(Color::Green)
                } else {
                    Style::default().fg(Color::Red)
                },
            ),
        ]),
        Line::from(vec![
            Span::styled("PnL: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("${}", fmt_dec(summary.total_pnl_usd, 2)),
                Style::default().fg(pnl_color).add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled("Avg: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("${}/t", fmt_dec(summary.avg_pnl_per_trade, 2)),
                Style::default().fg(Color::White),
            ),
        ]),
        Line::from(vec![
            Span::styled("Fees: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("${}", fmt_dec(summary.total_fees_usd, 2)),
                Style::default().fg(Color::Magenta),
            ),
            Span::raw("  "),
            Span::styled("Sharpe: ", Style::default().fg(Color::DarkGray)),
            Span::styled(sharpe_str, Style::default().fg(Color::Cyan)),
        ]),
        Line::from(vec![
            Span::styled("Notional: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("${}", fmt_dec(summary.total_notional, 0)),
                Style::default().fg(Color::White),
            ),
        ]),
        Line::from(vec![
            Span::styled("Avg BPS: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                fmt_dec(summary.avg_pnl_bps, 2),
                Style::default().fg(Color::Cyan),
            ),
        ]),
    ];

    Paragraph::new(lines)
        .block(
            Block::default()
                .title(" PnL Summary ")
                .borders(Borders::ALL),
        )
        .wrap(Wrap { trim: true })
}

fn render_footer(data: &DashboardData) -> Paragraph<'static> {
    let last = data.last_refresh.clone();
    Paragraph::new(Line::from(vec![
        Span::styled(
            " [q] Quit ",
            Style::default().bg(Color::DarkGray).fg(Color::White),
        ),
        Span::raw("  "),
        Span::styled(
            format!("Balance refresh: {last}"),
            Style::default().fg(Color::DarkGray),
        ),
    ]))
    .block(Block::default().borders(Borders::TOP))
}

fn fmt_total(d: Decimal) -> String {
    if d == Decimal::ZERO {
        return "-".into();
    }
    fmt_dec(d, 4)
}

fn fmt_dec(d: Decimal, precision: usize) -> String {
    d.to_f64()
        .map(|f| format!("{f:.precision$}", precision = precision))
        .unwrap_or_else(|| d.to_string())
}

fn fmt_pct(opt: Option<f64>) -> String {
    opt.map(|v| format!("{v:.1}%"))
        .unwrap_or_else(|| "-".into())
}
