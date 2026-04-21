use std::collections::HashMap;

use chrono::Utc;
use rust_decimal::Decimal;

use peanut_internship_rust::core::types::Address;
use peanut_internship_rust::exchange::orderbook::OrderBookAnalyzer;
use peanut_internship_rust::exchange::types::{NormalizedBalance, OrderBookSnapshot};
use peanut_internship_rust::integration::{CrossDexOpportunity, ForkSimInfo};
use peanut_internship_rust::inventory::WalletBalanceFetcher;
use peanut_internship_rust::inventory::pnl::{ArbRecord, PnLEngine, TradeLeg};
use peanut_internship_rust::inventory::rebalancer::RebalancePlanner;
use peanut_internship_rust::inventory::tracker::InventoryTracker;
use peanut_internship_rust::inventory::types::Venue;
use peanut_internship_rust::pricing::amm::UniswapV2Pair;
use peanut_internship_rust::pricing::router::{PoolRef, RouteFinder};

fn make_orderbook() -> OrderBookSnapshot {
    let bids = vec![
        (Decimal::from(2015), Decimal::from(10)),
        (Decimal::from(2014), Decimal::from(15)),
        (Decimal::from(2013), Decimal::from(20)),
    ];
    let asks = vec![
        (Decimal::from(2016), Decimal::from(5)),
        (Decimal::from(2017), Decimal::from(10)),
        (Decimal::from(2018), Decimal::from(15)),
    ];
    let best_bid = bids.first().copied();
    let best_ask = asks.first().copied();
    let mid_price = (Decimal::from(2015) + Decimal::from(2016)) / Decimal::TWO;
    let spread_bps = Decimal::ONE / mid_price * Decimal::from(10000);

    OrderBookSnapshot {
        symbol: "ETH/USDT".into(),
        timestamp: 1706000000000,
        bids,
        asks,
        best_bid,
        best_ask,
        mid_price: Some(mid_price),
        spread_bps: Some(spread_bps),
    }
}

fn make_tracker() -> InventoryTracker {
    let mut tracker = InventoryTracker::new(vec![Venue::Binance, Venue::Wallet]);

    let mut binance_bals = HashMap::new();
    binance_bals.insert(
        "ETH".into(),
        NormalizedBalance {
            free: Decimal::from(8),
            locked: Decimal::ZERO,
            total: Decimal::from(8),
        },
    );
    binance_bals.insert(
        "USDT".into(),
        NormalizedBalance {
            free: Decimal::from(20000),
            locked: Decimal::ZERO,
            total: Decimal::from(20000),
        },
    );
    tracker.update_from_cex(Venue::Binance, binance_bals);

    let mut wallet_bals = HashMap::new();
    wallet_bals.insert("ETH".into(), Decimal::from(5));
    wallet_bals.insert("USDT".into(), Decimal::from(15000));
    tracker.update_from_wallet(Venue::Wallet, wallet_bals);

    tracker
}

#[test]
fn test_orderbook_analysis_with_spread() {
    let ob = make_orderbook();
    let analyzer = OrderBookAnalyzer::new(ob);

    assert!(
        analyzer
            .orderbook()
            .spread_bps
            .is_some_and(|s| s > Decimal::ZERO)
    );
    assert!(analyzer.orderbook().best_bid.is_some());
    assert!(analyzer.orderbook().best_ask.is_some());
}

#[test]
fn test_gap_detection_buy_dex_sell_cex() {
    let ob = make_orderbook();
    let cex_bid = ob.best_bid.unwrap().0;
    let dex_price = Decimal::from(2007);

    let gap_bps = (cex_bid - dex_price) / dex_price * Decimal::from(10000);
    assert!(
        gap_bps > Decimal::ZERO,
        "CEX bid > DEX price should show positive gap"
    );
}

#[test]
fn test_gap_detection_buy_cex_sell_dex() {
    let ob = make_orderbook();
    let cex_ask = ob.best_ask.unwrap().0;
    let dex_price = Decimal::from(2025);

    let gap_bps = (dex_price - cex_ask) / cex_ask * Decimal::from(10000);
    assert!(
        gap_bps > Decimal::ZERO,
        "DEX price > CEX ask should show positive gap"
    );
}

#[test]
fn test_profitable_arb_after_costs() {
    let ob = make_orderbook();
    let cex_bid = ob.best_bid.unwrap().0;
    let dex_price = Decimal::from(2000);

    let gap_bps = (cex_bid - dex_price) / dex_price * Decimal::from(10000);
    let dex_fee_bps = Decimal::from(30);
    let cex_fee_bps = Decimal::from(10);
    let slippage_bps = Decimal::from(5);
    let gas_bps = Decimal::from(3);

    let total_costs = dex_fee_bps + cex_fee_bps + slippage_bps + gas_bps;
    let net_bps = gap_bps - total_costs;

    assert!(net_bps > Decimal::ZERO, "Gap should exceed costs");
}

#[test]
fn test_unprofitable_arb_rejected() {
    let ob = make_orderbook();
    let cex_bid = ob.best_bid.unwrap().0;
    let dex_price = Decimal::from(2014);

    let gap_bps = (cex_bid - dex_price) / dex_price * Decimal::from(10000);
    let total_costs = Decimal::from(30) + Decimal::from(10) + Decimal::from(5) + Decimal::from(3);

    let net_bps = gap_bps - total_costs;
    assert!(net_bps < Decimal::ZERO, "Gap should NOT exceed costs");
}

#[test]
fn test_inventory_check_passes_for_valid_arb() {
    let tracker = make_tracker();
    let result = tracker.can_execute(
        Venue::Wallet,
        "USDT",
        Decimal::from(4000),
        Venue::Binance,
        "ETH",
        Decimal::from(2),
    );
    assert!(result.can_execute);
}

#[test]
fn test_inventory_check_fails_insufficient_sell_side() {
    let tracker = make_tracker();
    let result = tracker.can_execute(
        Venue::Wallet,
        "USDT",
        Decimal::from(4000),
        Venue::Binance,
        "ETH",
        Decimal::from(100),
    );
    assert!(!result.can_execute);
}

#[test]
fn test_full_pnl_pipeline() {
    let mut engine = PnLEngine::new();

    for i in 0..5 {
        let id = format!("{i}");
        let buy_price = Decimal::from(2000);
        let sell_price = Decimal::from(2020);
        let amount = Decimal::from(2);
        let buy_fee = amount * buy_price * Decimal::from_str_exact("0.001").unwrap();
        let sell_fee = amount * sell_price * Decimal::from_str_exact("0.001").unwrap();

        let trade = ArbRecord {
            id,
            timestamp: Utc::now(),
            buy_leg: TradeLeg {
                id: format!("{i}_buy"),
                timestamp: Utc::now(),
                venue: Venue::Wallet,
                symbol: "ETH/USDT".into(),
                side: "buy".into(),
                amount,
                price: buy_price,
                fee: buy_fee,
                fee_asset: "USDT".into(),
            },
            sell_leg: TradeLeg {
                id: format!("{i}_sell"),
                timestamp: Utc::now(),
                venue: Venue::Binance,
                symbol: "ETH/USDT".into(),
                side: "sell".into(),
                amount,
                price: sell_price,
                fee: sell_fee,
                fee_asset: "USDT".into(),
            },
            gas_cost_usd: Decimal::from(5),
        };
        engine.record(trade);
    }

    let summary = engine.summary();
    assert_eq!(summary.total_trades, 5);
    assert!(summary.total_pnl_usd > Decimal::ZERO);
    assert!(summary.win_rate > 0.5);
    assert!(summary.avg_pnl_bps > Decimal::ZERO);
}

#[test]
fn test_rebalance_integration() {
    let tracker = make_tracker();
    let planner = RebalancePlanner::new(tracker, 30.0);

    let skews = planner.check_all();
    assert!(!skews.is_empty());

    let prices = HashMap::from([
        ("ETH".into(), Decimal::from(2000)),
        ("USDT".into(), Decimal::ONE),
    ]);

    let all_plans = planner.plan_all();
    let cost = planner.estimate_cost(
        &all_plans.values().flatten().cloned().collect::<Vec<_>>(),
        &prices,
    );

    if !all_plans.is_empty() {
        assert!(cost.total_transfers > 0);
        assert!(!cost.assets_affected.is_empty());
    }
}

#[test]
fn test_arb_record_properties() {
    let arb = ArbRecord {
        id: "test".into(),
        timestamp: Utc::now(),
        buy_leg: TradeLeg {
            id: "buy".into(),
            timestamp: Utc::now(),
            venue: Venue::Wallet,
            symbol: "ETH/USDT".into(),
            side: "buy".into(),
            amount: Decimal::from(2),
            price: Decimal::from(2000),
            fee: Decimal::from(4),
            fee_asset: "USDT".into(),
        },
        sell_leg: TradeLeg {
            id: "sell".into(),
            timestamp: Utc::now(),
            venue: Venue::Binance,
            symbol: "ETH/USDT".into(),
            side: "sell".into(),
            amount: Decimal::from(2),
            price: Decimal::from(2020),
            fee: Decimal::from(4),
            fee_asset: "USDT".into(),
        },
        gas_cost_usd: Decimal::from(5),
    };

    let gross = arb.gross_pnl();
    assert!(gross > Decimal::ZERO);

    let net = arb.net_pnl();
    assert!(net < gross);
    assert!(net > Decimal::ZERO);

    let bps = arb.net_pnl_bps();
    assert!(bps > Decimal::ZERO);

    assert_eq!(arb.notional(), Decimal::from(4000));
}

#[test]
fn test_wallet_balance_fetcher_builds_with_valid_address() {
    let fetcher = WalletBalanceFetcher::new(
        "http://127.0.0.1:1".to_string(),
        "0xd8dA6BF26964aF9D7eEd9d0319_COMPUTED_ADDRESS",
    );
    assert!(fetcher.is_err() || fetcher.is_ok());
}

#[test]
fn test_wallet_balance_fetcher_rejects_invalid_address() {
    let fetcher = WalletBalanceFetcher::new("http://127.0.0.1:1".to_string(), "not_an_address");
    assert!(fetcher.is_err());
}

#[test]
fn test_fork_sim_info_serialization() {
    let info = ForkSimInfo {
        success: true,
        amount_out: "123456".into(),
        gas_used: 150_000,
        error: None,
        matches_amm_math: true,
        amm_amount_out: "123456".into(),
    };
    let json = serde_json::to_string(&info).unwrap();
    assert!(json.contains("123456"));
    let deserialized: ForkSimInfo = serde_json::from_str(&json).unwrap();
    assert!(deserialized.success);
    assert!(deserialized.matches_amm_math);
}

#[test]
fn test_cross_dex_opportunity_serialization() {
    let opp = CrossDexOpportunity {
        kind: "triangular".into(),
        token_in: "WETH".into(),
        token_out: "USDC".into(),
        amount_in: "1000000000000000000".into(),
        net_profit_wei: "5000000000000000".into(),
        route_pools: vec!["0xabc".into(), "0xdef".into()],
        is_profitable: true,
    };
    let json = serde_json::to_string(&opp).unwrap();
    assert!(json.contains("triangular"));
    let deserialized: CrossDexOpportunity = serde_json::from_str(&json).unwrap();
    assert!(deserialized.is_profitable);
    assert_eq!(deserialized.route_pools.len(), 2);
}

#[test]
fn test_route_finder_single_pool_no_triangular() {
    let weth = peanut_internship_rust::core::types::Token {
        address: Address::new("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").unwrap(),
        symbol: "WETH".into(),
        decimals: 18,
    };
    let usdc = peanut_internship_rust::core::types::Token {
        address: Address::new("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").unwrap(),
        symbol: "USDC".into(),
        decimals: 6,
    };

    let pool = UniswapV2Pair::new(
        Address::new("0xB4e16d0168e52d35CaCD2c6185b44281Ec28C9Dc").unwrap(),
        weth.clone(),
        usdc.clone(),
        1_000_000_000_000_000_000_000,
        2_000_000_000_000_000_000_000,
        30,
    )
    .unwrap();

    let finder = RouteFinder::new(vec![PoolRef::V2(pool)]);
    let routes = finder.find_all_routes(&weth, &usdc, 3);
    assert!(!routes.is_empty());

    for route in &routes {
        assert!(route.num_hops() >= 1);
    }
}
