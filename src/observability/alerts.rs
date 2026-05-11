//! Webhook-based alerting (S7b).
//!
//! An alerter pushes selected [`AlertEvent`]s to an external HTTP endpoint
//! (Telegram Bot API / generic JSON). The sink is decoupled from callers via
//! [`AlertSink`] so the bot can swap between a `NoopSink` (default) and a
//! [`WebhookSink`] based on config.
//!
//! ## Provider payloads
//! - `Telegram`: `{ "chat_id": "...", "text": "<message>" }` via the
//!   Bot API `sendMessage` endpoint.
//! - `Generic`: full `AlertEvent` JSON.
//!
//! Failures during `emit` are logged and swallowed — alerting **must never**
//! break the execution hot path.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::executor::engine::ExecutorState;

/// Categories of events an operator might want surfaced.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AlertEvent {
    /// Bot has started and is entering the main loop.
    BotStarted {
        /// Trading mode (dry-run / testnet / production).
        mode: String,
        /// Monitored pairs.
        pairs: String,
    },
    /// Bot has stopped — either cleanly or via halt signal.
    BotStopped {
        /// Reason for shutdown (clean / kill switch / daily loss / etc.).
        reason: String,
    },
    /// Kill switch file was detected.
    KillSwitchTriggered {
        /// Path to the kill-switch file.
        path: String,
    },
    /// Circuit breaker flipped from closed to open.
    BreakerOpened {
        /// Number of consecutive failures that tripped the breaker.
        failures: u32,
    },
    /// Circuit breaker flipped from open to closed (cooldown elapsed).
    BreakerClosed,
    RiskPaused {
        reason: String,
    },
    RiskResumed {
        reason: String,
    },
    TradeExecuted {
        signal_id: String,
        pair: String,
        direction: String,
        expected_net_pnl: String,
        actual_net_pnl: String,
        actual_gross_pnl: String,
        actual_fees: String,
        actual_cex_fee: String,
        actual_gas_fee: String,
        gas_used: Option<String>,
    },
    RebalanceTriggered {
        assets: usize,
        steps: usize,
        dry_run: bool,
        quote_asset: String,
    },
    RebalanceStepCompleted {
        asset: String,
        step: String,
        reference: String,
    },
    RebalanceStepFailed {
        asset: String,
        step: String,
        reason: String,
    },
    /// A DEX leg-2 timeout occurred and was enqueued (or skipped) for
    /// reconciliation.
    Leg2Timeout {
        /// Originating signal id.
        signal_id: String,
        /// On-chain tx hash if the swap path captured one; otherwise `None`.
        tx_hash: Option<String>,
        /// Trading pair symbol (for easier operator triage).
        pair: String,
    },
    /// An execution reached `Failed` (leg-2 failure after unwind attempt).
    ExecutionFailed {
        /// Originating signal id.
        signal_id: String,
        /// Trading pair symbol.
        pair: String,
        /// Error string carried by [`ExecutionContext::error`].
        reason: String,
    },
    /// An execution landed a loss larger than the configured threshold.
    LargeLoss {
        /// Originating signal id.
        signal_id: String,
        /// Trading pair symbol.
        pair: String,
        /// Net realised PnL (serialised as string to avoid lossy f64).
        pnl: String,
    },
    /// PnL breaker tripped — daily cumulative loss exceeded the configured
    /// maximum. The bot is halting all execution until UTC midnight rollover.
    DailyLossHalt {
        /// Cumulative PnL at the time of halt (negative).
        cumulative_pnl: String,
        /// Configured maximum daily loss threshold.
        max_daily_loss: String,
    },
    /// Post-trade balance verification detected a mismatch between tracked
    /// and actual balances. The bot should halt for manual investigation.
    BalanceMismatch {
        /// Venue where the mismatch was detected.
        venue: String,
        /// Asset symbol.
        asset: String,
        /// Tracked (expected) balance.
        tracked: String,
        /// Actual (reported) balance.
        actual: String,
        /// Absolute difference.
        diff: String,
    },
}

impl AlertEvent {
    /// Human-readable single-line summary — used as the message text for
    /// Telegram alerts and as the `tracing` payload for [`LoggingSink`].
    pub fn summary(&self) -> String {
        match self {
            Self::BotStarted { mode, pairs } => {
                format!("🚀 Bot STARTED mode={mode} pairs={pairs}")
            }
            Self::BotStopped { reason } => {
                format!("🛑 Bot STOPPED reason={reason}")
            }
            Self::KillSwitchTriggered { path } => {
                format!("💀 KILL SWITCH triggered path={path}")
            }
            Self::BreakerOpened { failures } => {
                format!(
                    "🚨 Circuit breaker OPEN after {failures} failures — trades blocked, cooldown active"
                )
            }
            Self::BreakerClosed => {
                "✅ Circuit breaker CLOSED — trades resumed, breaker recovered".into()
            }
            Self::RiskPaused { reason } => {
                format!("⏸ Risk pause ACTIVE — trades blocked: {reason}")
            }
            Self::RiskResumed { reason } => {
                format!("▶️ Risk pause CLEARED — trades resumed: {reason}")
            }
            Self::TradeExecuted {
                signal_id,
                pair,
                direction,
                expected_net_pnl,
                actual_net_pnl,
                actual_gross_pnl,
                actual_fees,
                actual_cex_fee,
                actual_gas_fee,
                gas_used,
            } => {
                let gas_used = gas_used.as_deref().unwrap_or("n/a");
                format!(
                    "✅ TRADE EXECUTED {pair} {signal_id} direction={direction} expected_net={expected_net_pnl} actual_net={actual_net_pnl} gross={actual_gross_pnl} fees={actual_fees} cex_fee={actual_cex_fee} gas_fee={actual_gas_fee} gas_used={gas_used}"
                )
            }
            Self::RebalanceTriggered {
                assets,
                steps,
                dry_run,
                quote_asset,
            } => format!(
                "⚖️ REBALANCE TRIGGERED assets={assets} steps={steps} quote={quote_asset} dry_run={dry_run}"
            ),
            Self::RebalanceStepCompleted {
                asset,
                step,
                reference,
            } => format!("✅ REBALANCE STEP DONE asset={asset} step={step} ref={reference}"),
            Self::RebalanceStepFailed {
                asset,
                step,
                reason,
            } => format!("❌ REBALANCE STEP FAILED asset={asset} step={step}: {reason}"),
            Self::Leg2Timeout {
                signal_id,
                tx_hash,
                pair,
            } => match tx_hash {
                Some(tx) => {
                    format!("⚠️ LEG2_TIMEOUT {pair} {signal_id} (tx {tx}); reconcile enqueued")
                }
                None => format!(
                    "⚠️ LEG2_TIMEOUT {pair} {signal_id}; NO tx_hash captured — manual review required"
                ),
            },
            Self::ExecutionFailed {
                signal_id,
                pair,
                reason,
            } => format!("❌ FAILED {pair} {signal_id}: {reason}"),
            Self::LargeLoss {
                signal_id,
                pair,
                pnl,
            } => format!("💸 LARGE LOSS {pair} {signal_id}: pnl={pnl}"),
            Self::DailyLossHalt {
                cumulative_pnl,
                max_daily_loss,
            } => format!(
                "🚫 DAILY LOSS HALT — cumulative={cumulative_pnl}, threshold=-{max_daily_loss}"
            ),
            Self::BalanceMismatch {
                venue,
                asset,
                tracked,
                actual,
                diff,
            } => format!(
                "⚠️ BALANCE MISMATCH {venue} {asset}: tracked={tracked} actual={actual} diff={diff}"
            ),
        }
    }
}

// ---------------------------------------------------------------------------
// Sink trait + simple sinks
// ---------------------------------------------------------------------------

/// Receiver for [`AlertEvent`]s. Implementations are expected to be cheap
/// and non-blocking; IO should be done asynchronously.
#[async_trait]
pub trait AlertSink: Send + Sync + std::fmt::Debug {
    /// Emits `event`. Returns `Err(msg)` on recoverable failure — callers
    /// log the message and continue.
    async fn emit(&self, event: &AlertEvent) -> Result<(), String>;
}

/// Drops every event silently. Default when alerting is not configured.
#[derive(Debug, Default, Clone)]
pub struct NoopSink;

#[async_trait]
impl AlertSink for NoopSink {
    async fn emit(&self, _event: &AlertEvent) -> Result<(), String> {
        Ok(())
    }
}

/// Logs each event at `info!` via `tracing`. Useful in dev and as a
/// fallback when a webhook is unavailable.
#[derive(Debug, Default, Clone)]
pub struct LoggingSink;

#[async_trait]
impl AlertSink for LoggingSink {
    async fn emit(&self, event: &AlertEvent) -> Result<(), String> {
        tracing::info!(alert = %event.summary(), "alert");
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Webhook sink
// ---------------------------------------------------------------------------

/// Third-party formats supported by [`WebhookSink`]. Both post JSON
/// over HTTP POST; the payload shape differs per provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AlertProvider {
    /// Telegram Bot API `sendMessage`: `{"chat_id": "...", "text": "<summary>"}`.
    /// The URL should be `https://api.telegram.org/bot<TOKEN>/sendMessage`.
    Telegram,
    /// Generic: the full [`AlertEvent`] serialised as JSON.
    Generic,
}

impl AlertProvider {
    /// Parses the string form used on the CLI. Unknown values fall back to
    /// `Generic` — callers should log a warning rather than refuse to
    /// start.
    pub fn parse(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "telegram" => Self::Telegram,
            _ => Self::Generic,
        }
    }
}

/// Pushes each event to an HTTP(S) webhook.
#[derive(Debug, Clone)]
pub struct WebhookSink {
    url: String,
    provider: AlertProvider,
    /// Telegram `chat_id`; ignored for `Generic` provider.
    chat_id: String,
    client: reqwest::Client,
}

/// Redacts the secret portion of a webhook URL for safe logging.
///
/// Telegram Bot API URLs embed the bot token in the path
/// (e.g. `https://api.telegram.org/bot123456:ABC-DEF/sendMessage`).
/// Logging the full URL leaks that secret into log aggregators and
/// backups. This helper keeps scheme+host and the first path segment so
/// the provider is still identifiable, but masks the rest.
///
/// ```text
/// https://api.telegram.org/bot123456:ABC-DEF/sendMessage
///   → https://api.telegram.org/bot***
/// ```
pub fn mask_webhook_url(raw: &str) -> String {
    // Lightweight, no-new-dep parser sufficient for webhook URL shapes:
    // split `scheme://host[/...]` and keep only host + first path segment.
    let (scheme, rest) = match raw.split_once("://") {
        Some(t) => t,
        None => return "<unparsable-url>".into(),
    };
    let (host, path) = match rest.split_once('/') {
        Some((h, p)) => (h, p),
        None => (rest, ""),
    };
    if host.is_empty() {
        return "<unparsable-url>".into();
    }
    // Telegram Bot API URLs embed the bot token in the first path segment
    // (e.g. `bot123456:ABC-DEF123`). Detect and mask it.
    let first_segment = path.split('/').next().unwrap_or("");
    if first_segment.is_empty() {
        format!("{scheme}://{host}/***")
    } else if first_segment.starts_with("bot") && first_segment.contains(':') {
        format!("{scheme}://{host}/bot***/***")
    } else {
        format!("{scheme}://{host}/{first_segment}/***")
    }
}

impl WebhookSink {
    /// Creates a new sink. `timeout` bounds each individual HTTP request —
    /// a default of 5s is reasonable for Telegram / generic endpoints.
    pub fn new(
        url: impl Into<String>,
        provider: AlertProvider,
        chat_id: impl Into<String>,
        timeout: Duration,
    ) -> Self {
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            // Fall back to a default client if the builder fails for
            // environmental reasons; logging is downstream.
            .unwrap_or_default();
        Self {
            url: url.into(),
            provider,
            chat_id: chat_id.into(),
            client,
        }
    }

    /// Builds the JSON body for a given provider + event.
    fn encode_payload(
        provider: AlertProvider,
        chat_id: &str,
        event: &AlertEvent,
    ) -> Result<serde_json::Value, String> {
        match provider {
            AlertProvider::Telegram => Ok(serde_json::json!({
                "chat_id": chat_id,
                "text": event.summary(),
                "parse_mode": "HTML",
            })),
            AlertProvider::Generic => {
                serde_json::to_value(event).map_err(|e| format!("serialize event: {e}"))
            }
        }
    }
}

#[async_trait]
impl AlertSink for WebhookSink {
    async fn emit(&self, event: &AlertEvent) -> Result<(), String> {
        let body = Self::encode_payload(self.provider, &self.chat_id, event)?;
        let res = self
            .client
            .post(&self.url)
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("POST failed: {e}"))?;
        let status = res.status();
        if !status.is_success() {
            // Drain the body for debuggability but cap to avoid spamming
            // logs with an HTML error page.
            let body = res.text().await.unwrap_or_default();
            let preview: String = body.chars().take(200).collect();
            return Err(format!("webhook status {status}: {preview}"));
        }
        debug!(provider = ?self.provider, "alert: webhook delivered");
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Rule: derive AlertEvents from execution outcomes
// ---------------------------------------------------------------------------

/// Configurable thresholds for [`evaluate_execution`].
#[derive(Debug, Clone)]
pub struct AlertRules {
    /// Absolute-value loss threshold that triggers [`AlertEvent::LargeLoss`].
    /// A signal with `pnl <= -large_loss_threshold` qualifies.
    pub large_loss_threshold: rust_decimal::Decimal,
}

impl Default for AlertRules {
    fn default() -> Self {
        Self {
            // $100 — intentionally conservative; operator should tune.
            large_loss_threshold: rust_decimal::Decimal::from(100),
        }
    }
}

/// Decides which [`AlertEvent`]s (if any) a terminal
/// [`ExecutionContext`](crate::executor::engine::ExecutionContext) deserves.
///
/// Extracted from a sink call so callers can unit-test the policy without
/// real HTTP traffic.
pub fn evaluate_execution(
    ctx: &crate::executor::engine::ExecutionContext,
    rules: &AlertRules,
) -> Vec<AlertEvent> {
    let mut out = Vec::new();
    let signal_id = ctx.signal.signal_id.clone();
    let pair = ctx.signal.pair.clone();
    match ctx.state {
        ExecutorState::Leg2Timeout => {
            out.push(AlertEvent::Leg2Timeout {
                signal_id,
                tx_hash: ctx.leg2_handle.clone(),
                pair,
            });
        }
        ExecutorState::Failed | ExecutorState::Leg2Reverted => {
            out.push(AlertEvent::ExecutionFailed {
                signal_id,
                pair,
                reason: ctx.error.clone().unwrap_or_else(|| "unknown".into()),
            });
        }
        ExecutorState::Done => {
            if let Some(pnl) = ctx.actual_net_pnl
                && pnl < Decimal::ZERO
                && pnl.abs() >= rules.large_loss_threshold
            {
                out.push(AlertEvent::LargeLoss {
                    signal_id,
                    pair,
                    pnl: pnl.to_string(),
                });
            }
        }
        _ => {}
    }
    out
}

// ---------------------------------------------------------------------------
// Convenience: fire-and-forget emit helper
// ---------------------------------------------------------------------------

/// Emits `event` via `sink`, logging a warning on failure but never
/// propagating the error — alerting is best-effort.
pub async fn emit_best_effort(sink: &Arc<dyn AlertSink>, event: &AlertEvent) {
    if let Err(e) = sink.emit(event).await {
        warn!(error = %e, "alert sink: emit failed");
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::engine::ExecutionContext;
    use crate::strategy::signal::{Direction, Signal, SignalParams};
    use rust_decimal::Decimal;
    use std::sync::Mutex;

    fn mk_ctx(state: ExecutorState, pnl: Option<Decimal>) -> ExecutionContext {
        let signal = Signal::new(SignalParams {
            pair: "ETH/USDC".into(),
            direction: Direction::BuyCexSellDex,
            cex_price: Decimal::from(2000),
            dex_price: Decimal::from(2010),
            spread_bps: Decimal::from(50),
            size: Decimal::ONE,
            notional_usd: Decimal::from(2000),
            expected_gross_pnl: Decimal::from(10),
            expected_fees: Decimal::from(5),
            expected_net_pnl: Decimal::from(5),
            ttl: chrono::Duration::seconds(5),
            inventory_ok: true,
            within_limits: true,
        });
        // We can't construct an ExecutionContext directly (private fields),
        // so we surgically use the public-facing fields via the struct's
        // public accessors. For the evaluator we only need `state`, `error`,
        // `actual_net_pnl`, `leg2_handle`, `signal`. Construct via a JSON
        // round-trip through the public Debug form is overkill; instead we
        // build via `Default`-ish path using ExecutionContext's module
        // accessors — there aren't any public builders, so we just rely on
        // the test passing an executor through normal means. For pure
        // evaluator tests we instead poke fields we DO have access to:
        // those that are pub. Unfortunately `state` is pub; let's go.
        // NOTE: ExecutionContext fields ARE all pub (see engine.rs).
        let mut ctx = ExecutionContext {
            signal,
            state: ExecutorState::Idle,
            leg1_venue: "",
            leg1_handle: None,
            leg1_fill_price: None,
            leg1_fill_size: None,
            leg1_fee: Decimal::ZERO,
            leg1_fee_asset: None,
            leg2_venue: "",
            leg2_handle: None,
            leg2_fill_price: None,
            leg2_fill_size: None,
            leg2_fee: Decimal::ZERO,
            leg2_fee_asset: None,
            started_at: std::time::Instant::now(),
            finished_at: None,
            actual_net_pnl: pnl,
            actual_gross_pnl_usd: pnl,
            actual_fees_usd: None,
            actual_cex_fee_usd: None,
            actual_onchain_gas_fee_usd: None,
            onchain_gas_used: None,
            onchain_gas_fee_wei: None,
            error: None,
        };
        ctx.state = state;
        ctx
    }

    // ---- AlertEvent summary --------------------------------------------

    #[test]
    fn summary_contains_key_fields() {
        let e = AlertEvent::Leg2Timeout {
            signal_id: "sig-1".into(),
            tx_hash: Some("0xdead".into()),
            pair: "ETH/USDC".into(),
        };
        let s = e.summary();
        assert!(s.contains("sig-1"));
        assert!(s.contains("0xdead"));
        assert!(s.contains("ETH/USDC"));
    }

    #[test]
    fn summary_flags_missing_tx_hash() {
        let e = AlertEvent::Leg2Timeout {
            signal_id: "sig-2".into(),
            tx_hash: None,
            pair: "BTC/USDT".into(),
        };
        assert!(e.summary().contains("manual review"));
    }

    // ---- evaluate_execution -------------------------------------------

    #[test]
    fn evaluate_leg2_timeout_emits_alert() {
        let mut ctx = mk_ctx(ExecutorState::Leg2Timeout, None);
        ctx.leg2_handle = Some("0xfeed".into());
        let out = evaluate_execution(&ctx, &AlertRules::default());
        assert_eq!(out.len(), 1);
        matches!(&out[0], AlertEvent::Leg2Timeout { tx_hash, .. } if tx_hash.as_deref() == Some("0xfeed"));
    }

    #[test]
    fn evaluate_failed_emits_execution_failed() {
        let mut ctx = mk_ctx(ExecutorState::Failed, None);
        ctx.error = Some("DEX partial".into());
        let out = evaluate_execution(&ctx, &AlertRules::default());
        assert_eq!(out.len(), 1);
        matches!(&out[0], AlertEvent::ExecutionFailed { reason, .. } if reason == "DEX partial");
    }

    #[test]
    fn evaluate_done_with_profit_is_silent() {
        let ctx = mk_ctx(ExecutorState::Done, Some(Decimal::from(50)));
        let out = evaluate_execution(&ctx, &AlertRules::default());
        assert!(out.is_empty());
    }

    #[test]
    fn evaluate_done_with_loss_below_threshold_is_silent() {
        let ctx = mk_ctx(ExecutorState::Done, Some(Decimal::from(-10)));
        let out = evaluate_execution(&ctx, &AlertRules::default());
        // |-10| = 10 < default 100 threshold.
        assert!(out.is_empty());
    }

    #[test]
    fn evaluate_done_with_loss_above_threshold_emits_large_loss() {
        let ctx = mk_ctx(ExecutorState::Done, Some(Decimal::from(-500)));
        let out = evaluate_execution(&ctx, &AlertRules::default());
        assert_eq!(out.len(), 1);
        matches!(&out[0], AlertEvent::LargeLoss { pnl, .. } if pnl == "-500");
    }

    // ---- WebhookSink payload encoding ---------------------------------

    #[test]
    fn telegram_payload_uses_text_and_chat_id() {
        let event = AlertEvent::BreakerOpened { failures: 3 };
        let body = WebhookSink::encode_payload(AlertProvider::Telegram, "12345", &event).unwrap();
        assert!(body["text"].is_string());
        assert!(body["text"].as_str().unwrap().contains("breaker"));
        assert_eq!(body["chat_id"], "12345");
        assert_eq!(body["parse_mode"], "HTML");
    }

    #[test]
    fn generic_payload_preserves_kind_tag() {
        let event = AlertEvent::BreakerOpened { failures: 5 };
        let body = WebhookSink::encode_payload(AlertProvider::Generic, "", &event).unwrap();
        assert_eq!(body["kind"], "breaker_opened");
        assert_eq!(body["failures"], 5);
    }

    #[test]
    fn trade_executed_summary_and_payload_include_pnl_breakdown() {
        let event = AlertEvent::TradeExecuted {
            signal_id: "sig-1".into(),
            pair: "LINK/ETH".into(),
            direction: "buy_dex_sell_cex".into(),
            expected_net_pnl: "$0.020000".into(),
            actual_net_pnl: "$0.010000".into(),
            actual_gross_pnl: "$0.030000".into(),
            actual_fees: "$0.020000".into(),
            actual_cex_fee: "$0.001000".into(),
            actual_gas_fee: "$0.019000".into(),
            gas_used: Some("12345".into()),
        };
        let summary = event.summary();
        assert!(summary.contains("TRADE EXECUTED"));
        assert!(summary.contains("LINK/ETH"));
        assert!(summary.contains("actual_net=$0.010000"));
        assert!(summary.contains("gas_used=12345"));

        let body = WebhookSink::encode_payload(AlertProvider::Generic, "", &event).unwrap();
        assert_eq!(body["kind"], "trade_executed");
        assert_eq!(body["actual_net_pnl"], "$0.010000");
    }

    #[test]
    fn rebalance_triggered_summary_and_payload_include_plan_size() {
        let event = AlertEvent::RebalanceTriggered {
            assets: 2,
            steps: 3,
            dry_run: false,
            quote_asset: "ETH".into(),
        };
        let summary = event.summary();
        assert!(summary.contains("REBALANCE TRIGGERED"));
        assert!(summary.contains("assets=2"));
        assert!(summary.contains("quote=ETH"));

        let body = WebhookSink::encode_payload(AlertProvider::Generic, "", &event).unwrap();
        assert_eq!(body["kind"], "rebalance_triggered");
        assert_eq!(body["steps"], 3);
    }

    #[test]
    fn provider_parse_is_case_insensitive() {
        assert_eq!(AlertProvider::parse("Telegram"), AlertProvider::Telegram);
        assert_eq!(AlertProvider::parse("TELEGRAM"), AlertProvider::Telegram);
        assert_eq!(AlertProvider::parse("custom"), AlertProvider::Generic);
    }

    // ---- Sinks behaviour ----------------------------------------------

    // ---- URL masking --------------------------------------------------

    #[test]
    fn mask_telegram_url_keeps_host_and_masks_token() {
        let url = "https://api.telegram.org/bot123456:ABC-DEF123/sendMessage";
        let masked = mask_webhook_url(url);
        assert_eq!(masked, "https://api.telegram.org/bot***/***");
        assert!(!masked.contains("ABC-DEF123"));
        assert!(!masked.contains("123456"));
    }

    #[test]
    fn mask_bare_host_masks_path_placeholder() {
        assert_eq!(
            mask_webhook_url("https://example.com/"),
            "https://example.com/***"
        );
    }

    #[test]
    fn mask_invalid_url_does_not_leak_input() {
        let bogus = "not a url supersecret=abc";
        let masked = mask_webhook_url(bogus);
        assert!(!masked.contains("supersecret"));
        assert_eq!(masked, "<unparsable-url>");
    }

    #[tokio::test]
    async fn noop_sink_never_errors() {
        let sink = NoopSink;
        sink.emit(&AlertEvent::BreakerClosed).await.unwrap();
    }

    /// Captures every emitted event for white-box tests.
    #[derive(Debug, Default)]
    struct CaptureSink {
        events: Mutex<Vec<AlertEvent>>,
    }

    #[async_trait]
    impl AlertSink for CaptureSink {
        async fn emit(&self, event: &AlertEvent) -> Result<(), String> {
            self.events.lock().unwrap().push(event.clone());
            Ok(())
        }
    }

    #[tokio::test]
    async fn emit_best_effort_forwards_events() {
        let capture = Arc::new(CaptureSink::default());
        let sink: Arc<dyn AlertSink> = capture.clone();
        emit_best_effort(&sink, &AlertEvent::BreakerClosed).await;
        assert_eq!(capture.events.lock().unwrap().len(), 1);
    }

    // ---- Bot lifecycle alerts ------------------------------------------

    #[test]
    fn bot_started_summary_contains_mode_and_pairs() {
        let e = AlertEvent::BotStarted {
            mode: "dry-run".into(),
            pairs: "ETH/USDC".into(),
        };
        let s = e.summary();
        assert!(s.contains("dry-run"));
        assert!(s.contains("ETH/USDC"));
        assert!(s.contains("STARTED"));
    }

    #[test]
    fn bot_stopped_summary_contains_reason() {
        let e = AlertEvent::BotStopped {
            reason: "clean shutdown".into(),
        };
        let s = e.summary();
        assert!(s.contains("clean shutdown"));
        assert!(s.contains("STOPPED"));
    }

    #[test]
    fn kill_switch_summary_contains_path() {
        let e = AlertEvent::KillSwitchTriggered {
            path: "/tmp/arb_bot_kill".into(),
        };
        let s = e.summary();
        assert!(s.contains("/tmp/arb_bot_kill"));
        assert!(s.contains("KILL SWITCH"));
    }

    // ---- Circuit breaker lifecycle alerts ------------------------------

    #[test]
    fn breaker_opened_summary_contains_failure_count() {
        let e = AlertEvent::BreakerOpened { failures: 3 };
        let s = e.summary();
        assert!(s.contains("3"));
        assert!(s.contains("OPEN"));
        assert!(s.contains("blocked"));
    }

    #[test]
    fn breaker_closed_summary_indicates_recovery() {
        let e = AlertEvent::BreakerClosed;
        let s = e.summary();
        assert!(s.contains("CLOSED"));
        assert!(s.contains("resumed"));
    }

    #[tokio::test]
    async fn circuit_breaker_lifecycle_emits_open_then_closed() {
        let capture = Arc::new(CaptureSink::default());
        let sink: Arc<dyn AlertSink> = capture.clone();

        // Simulate breaker opening
        emit_best_effort(&sink, &AlertEvent::BreakerOpened { failures: 3 }).await;
        // Simulate breaker closing after cooldown
        emit_best_effort(&sink, &AlertEvent::BreakerClosed).await;

        let events = capture.events.lock().unwrap();
        assert_eq!(events.len(), 2);
        assert!(matches!(&events[0], AlertEvent::BreakerOpened { failures } if *failures == 3));
        assert!(matches!(&events[1], AlertEvent::BreakerClosed));
    }

    #[test]
    fn daily_loss_halt_summary_contains_thresholds() {
        let e = AlertEvent::DailyLossHalt {
            cumulative_pnl: "-15.00".into(),
            max_daily_loss: "10".into(),
        };
        let s = e.summary();
        assert!(s.contains("-15.00"));
        assert!(s.contains("10"));
        assert!(s.contains("DAILY LOSS HALT"));
    }

    #[test]
    fn balance_mismatch_summary_contains_details() {
        let e = AlertEvent::BalanceMismatch {
            venue: "Binance".into(),
            asset: "ETH".into(),
            tracked: "10".into(),
            actual: "9".into(),
            diff: "1".into(),
        };
        let s = e.summary();
        assert!(s.contains("Binance"));
        assert!(s.contains("ETH"));
        assert!(s.contains("tracked=10"));
        assert!(s.contains("actual=9"));
        assert!(s.contains("BALANCE MISMATCH"));
    }
}
