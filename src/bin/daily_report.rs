use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;

use chrono::{NaiveDate, Utc};
use clap::Parser;
use rusqlite::{Connection, params};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use peanut_internship_rust::inventory::pnl::ArbRecord;

#[derive(Debug, Parser)]
#[command(name = "daily_report")]
#[command(about = "Generate a daily operations report from arb_bot JSONL events")]
struct Cli {
    #[arg(long, default_value = "events.jsonl")]
    events: PathBuf,

    #[arg(long)]
    date: Option<NaiveDate>,

    #[arg(long, default_value = "markdown", value_parser = ["markdown", "json", "csv", "html"])]
    format: String,

    #[arg(long)]
    output: Option<PathBuf>,

    #[arg(long)]
    reconcile_db: Option<PathBuf>,

    #[arg(long)]
    trade_log: Option<PathBuf>,

    #[arg(long)]
    metrics_snapshot: Option<PathBuf>,

    #[arg(long)]
    compare_report: Option<PathBuf>,

    #[arg(long)]
    alert_summary_output: Option<PathBuf>,

    #[arg(long, default_value = "structured", value_parser = ["structured", "telegram"])]
    alert_summary_format: String,

    #[arg(long, default_value = "", env = "TELEGRAM_CHAT_ID")]
    alert_telegram_chat_id: String,

    #[arg(long, value_parser = ["yellow", "red"])]
    fail_on_health: Option<String>,
}

#[derive(Debug, Deserialize)]
struct EventLine {
    ts: chrono::DateTime<Utc>,
    event_type: String,
    fields: Value,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default)]
struct DailyReport {
    date: NaiveDate,
    health: String,
    risk_score: u16,
    top_risk_causes: Vec<RiskCause>,
    total_events: usize,
    parse_errors: usize,
    bot_started: usize,
    bot_stopped: usize,
    halted_stops: usize,
    executions_total: usize,
    execution_states: BTreeMap<String, usize>,
    executions_by_pair: BTreeMap<String, usize>,
    realized_pnl_usd: Decimal,
    winning_executions: usize,
    losing_executions: usize,
    dex_pending_timeouts: usize,
    dex_cancel_outcomes: BTreeMap<String, usize>,
    reconcile_enqueued: usize,
    reconcile_enqueue_failed: usize,
    reconcile_resolved: BTreeMap<String, usize>,
    reconcile_inspect_errors: usize,
    reconcile_db: Option<ReconcileDbSnapshot>,
    trade_log: Option<TradeLogSummary>,
    metrics_snapshot: Option<MetricsSnapshotSummary>,
    comparison: Option<ReportComparison>,
    risk_notes: Vec<String>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default)]
struct RiskCause {
    severity: String,
    score: u16,
    message: String,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default)]
struct ReportComparison {
    previous_date: NaiveDate,
    previous_health: String,
    health_changed: bool,
    risk_score_delta: i64,
    total_events_delta: i64,
    executions_delta: i64,
    realized_pnl_usd_delta: Decimal,
    trade_net_pnl_usd_delta: Option<Decimal>,
    dex_pending_timeouts_delta: i64,
    reconcile_open_entries_delta: Option<i64>,
    flashbots_inclusion_rate_delta: Option<f64>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default)]
struct MetricsSnapshotSummary {
    path: String,
    parse_errors: usize,
    totals_by_metric: BTreeMap<String, f64>,
    series: BTreeMap<String, f64>,
    flashbots_inclusion_rate: Option<f64>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default)]
struct TradeLogSummary {
    path: String,
    parse_errors: usize,
    trades_total: usize,
    trades_by_pair: BTreeMap<String, usize>,
    net_pnl_usd: Decimal,
    fees_usd: Decimal,
    notional_usd: Decimal,
    avg_net_pnl_bps: Decimal,
    best_trade_pnl_usd: Decimal,
    worst_trade_pnl_usd: Decimal,
    winning_trades: usize,
    losing_trades: usize,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default)]
struct ReconcileDbSnapshot {
    path: String,
    status_counts_for_day: BTreeMap<String, usize>,
    open_entries: Vec<ReconcileDbOpenEntry>,
    oldest_pending_age_secs: Option<i64>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default)]
struct ReconcileDbOpenEntry {
    signal_id: String,
    tx_hash: String,
    pair: Option<String>,
    status: String,
    started_at_unix: i64,
    age_secs: i64,
    resolution: Option<String>,
}

#[derive(Debug, Serialize)]
struct AlertSummary {
    date: NaiveDate,
    health: String,
    risk_score: u16,
    top_risk_causes: Vec<RiskCause>,
    risk_notes: Vec<String>,
    telegram_payload: TelegramPayload,
    output_hint: String,
}

#[derive(Debug, Clone, Serialize)]
struct TelegramPayload {
    chat_id: String,
    text: String,
    parse_mode: String,
    disable_web_page_preview: bool,
}

impl DailyReport {
    fn for_date(date: NaiveDate) -> Self {
        Self {
            date,
            ..Self::default()
        }
    }

    fn record(&mut self, event: &EventLine) {
        self.total_events += 1;
        match event.event_type.as_str() {
            "bot_started" => self.bot_started += 1,
            "bot_stopped" => {
                self.bot_stopped += 1;
                if bool_field(&event.fields, "halted") == Some(true) {
                    self.halted_stops += 1;
                }
            }
            "execution_terminal" => self.record_execution(&event.fields),
            "dex_pending_timeout" => self.dex_pending_timeouts += 1,
            "dex_cancel_outcome" => {
                let outcome =
                    string_field(&event.fields, "outcome").unwrap_or_else(|| "unknown".into());
                *self.dex_cancel_outcomes.entry(outcome).or_default() += 1;
            }
            "reconcile_enqueued" => self.reconcile_enqueued += 1,
            "reconcile_enqueue_failed" => self.reconcile_enqueue_failed += 1,
            "reconcile_resolved" => {
                let outcome =
                    string_field(&event.fields, "outcome").unwrap_or_else(|| "unknown".into());
                *self.reconcile_resolved.entry(outcome).or_default() += 1;
            }
            "reconcile_inspect_error" => self.reconcile_inspect_errors += 1,
            _ => {}
        }
    }

    fn record_execution(&mut self, fields: &Value) {
        self.executions_total += 1;
        if let Some(state) = string_field(fields, "state") {
            *self.execution_states.entry(state).or_default() += 1;
        }
        if let Some(pair) = string_field(fields, "pair") {
            *self.executions_by_pair.entry(pair).or_default() += 1;
        }
        if let Some(pnl) = decimal_field(fields, "actual_net_pnl") {
            self.realized_pnl_usd += pnl;
            if pnl > Decimal::ZERO {
                self.winning_executions += 1;
            } else if pnl < Decimal::ZERO {
                self.losing_executions += 1;
            }
        }
    }

    fn finalize(&mut self) {
        self.risk_notes.clear();
        self.top_risk_causes.clear();
        self.risk_score = 0;
        if self.halted_stops > 0 {
            self.risk_notes
                .push("bot halted during report window".into());
        }
        if self.reconcile_enqueue_failed > 0 {
            self.risk_notes
                .push("reconcile enqueue failures observed".into());
        }
        if self.reconcile_inspect_errors > 0 {
            self.risk_notes
                .push("reconcile inspect errors observed".into());
        }
        if self.reconcile_resolved.get("expired").copied().unwrap_or(0) > 0 {
            self.risk_notes
                .push("expired reconcile entries require manual review".into());
        }
        if let Some(db) = &self.reconcile_db {
            let db_expired_or_errored = db
                .open_entries
                .iter()
                .filter(|entry| entry.status == "expired" || entry.status == "errored")
                .count();
            if db_expired_or_errored > 0 {
                self.risk_notes
                    .push("reconcile DB has expired/errored open entries".into());
            }
            if !db.open_entries.is_empty() {
                self.risk_notes
                    .push("reconcile DB has open entries requiring follow-up".into());
            }
        }
        if let Some(trades) = &self.trade_log
            && trades.net_pnl_usd < Decimal::ZERO
        {
            self.risk_notes
                .push("trade log net PnL is negative for report day".into());
        }
        if let Some(metrics) = &self.metrics_snapshot {
            if metric_total(metrics, "peanut_flashbots_relay_errors_total") > 0.0 {
                self.risk_notes
                    .push("Flashbots relay errors present in metrics snapshot".into());
            }
            if metric_total(metrics, "peanut_flashbots_bundles_not_included_total") > 0.0 {
                self.risk_notes
                    .push("Flashbots bundle not-included count is non-zero".into());
            }
            if metric_total(metrics, "peanut_reconcile_expired_total") > 0.0 {
                self.risk_notes
                    .push("Prometheus snapshot reports expired reconcile entries".into());
            }
        }
        let cancel_unknown = self
            .dex_cancel_outcomes
            .get("unknown")
            .copied()
            .unwrap_or(0);
        let cancel_error = self.dex_cancel_outcomes.get("error").copied().unwrap_or(0);
        if cancel_unknown + cancel_error > 0 {
            self.risk_notes
                .push("DEX cancel had unknown/error outcomes".into());
        }
        self.health = if self.halted_stops > 0
            || self.reconcile_enqueue_failed > 0
            || self.reconcile_resolved.get("expired").copied().unwrap_or(0) > 0
            || self
                .reconcile_db
                .as_ref()
                .map(|db| {
                    db.open_entries
                        .iter()
                        .any(|entry| entry.status == "expired" || entry.status == "errored")
                })
                .unwrap_or(false)
            || cancel_error > 0
        {
            "RED".into()
        } else if self.dex_pending_timeouts > 0
            || self.reconcile_inspect_errors > 0
            || self
                .reconcile_db
                .as_ref()
                .map(|db| !db.open_entries.is_empty())
                .unwrap_or(false)
            || self
                .metrics_snapshot
                .as_ref()
                .map(|metrics| {
                    metric_total(metrics, "peanut_flashbots_relay_errors_total") > 0.0
                        || metric_total(metrics, "peanut_flashbots_bundles_not_included_total")
                            > 0.0
                })
                .unwrap_or(false)
            || self
                .trade_log
                .as_ref()
                .map(|trades| trades.net_pnl_usd < Decimal::ZERO)
                .unwrap_or(false)
            || cancel_unknown > 0
            || self.losing_executions > 0
        {
            "YELLOW".into()
        } else {
            "GREEN".into()
        };
        self.top_risk_causes = build_risk_causes(self);
        self.risk_score = self
            .top_risk_causes
            .iter()
            .map(|cause| cause.score)
            .sum::<u16>()
            .min(100);
        self.top_risk_causes
            .sort_by(|left, right| right.score.cmp(&left.score));
        self.top_risk_causes.truncate(5);
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    let date = cli.date.unwrap_or_else(|| Utc::now().date_naive());
    let report = build_report(
        &cli.events,
        date,
        cli.reconcile_db.as_ref(),
        cli.trade_log.as_ref(),
        cli.metrics_snapshot.as_ref(),
        cli.compare_report.as_ref(),
    )?;
    let body = match cli.format.as_str() {
        "json" => serde_json::to_string_pretty(&report)?,
        "csv" => render_csv(&report)?,
        "html" => render_html(&report),
        _ => render_markdown(&report),
    };
    if let Some(path) = cli.output {
        write_report_file(&path, body)?;
    } else {
        print!("{body}");
    }
    if let Some(path) = cli.alert_summary_output {
        if cli.alert_summary_format == "telegram" && cli.alert_telegram_chat_id.is_empty() {
            return Err("--alert-summary-format telegram requires --alert-telegram-chat-id or TELEGRAM_CHAT_ID".into());
        }
        let summary = alert_summary(&report, &cli.alert_telegram_chat_id);
        let body = match cli.alert_summary_format.as_str() {
            "telegram" => serde_json::to_string_pretty(&summary.telegram_payload)?,
            _ => serde_json::to_string_pretty(&summary)?,
        };
        write_report_file(&path, body)?;
    }
    if should_fail_on_health(&report.health, cli.fail_on_health.as_deref()) {
        eprintln!(
            "daily_report health {} matched --fail-on-health threshold",
            report.health
        );
        std::process::exit(2);
    }
    Ok(())
}

fn build_report(
    path: &PathBuf,
    date: NaiveDate,
    reconcile_db: Option<&PathBuf>,
    trade_log: Option<&PathBuf>,
    metrics_snapshot: Option<&PathBuf>,
    compare_report: Option<&PathBuf>,
) -> Result<DailyReport, Box<dyn std::error::Error>> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let mut report = DailyReport::for_date(date);
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<EventLine>(&line) {
            Ok(event) if event.ts.date_naive() == date => report.record(&event),
            Ok(_) => {}
            Err(_) => report.parse_errors += 1,
        }
    }
    if let Some(db_path) = reconcile_db {
        report.reconcile_db = Some(load_reconcile_db_snapshot(db_path, date)?);
    }
    if let Some(trade_log_path) = trade_log {
        report.trade_log = Some(load_trade_log_summary(trade_log_path, date)?);
    }
    if let Some(metrics_path) = metrics_snapshot {
        report.metrics_snapshot = Some(load_metrics_snapshot(metrics_path)?);
    }
    report.finalize();
    if let Some(compare_path) = compare_report {
        report.comparison = Some(load_report_comparison(compare_path, &report)?);
    }
    Ok(report)
}

fn build_risk_causes(report: &DailyReport) -> Vec<RiskCause> {
    let mut causes = Vec::new();
    push_risk_cause(
        &mut causes,
        report.halted_stops > 0,
        "red",
        40,
        "bot halted during report window",
    );
    push_risk_cause(
        &mut causes,
        report.reconcile_enqueue_failed > 0,
        "red",
        30,
        "reconcile enqueue failures observed",
    );
    push_risk_cause(
        &mut causes,
        report
            .reconcile_resolved
            .get("expired")
            .copied()
            .unwrap_or(0)
            > 0,
        "red",
        25,
        "expired reconcile entries require manual review",
    );
    push_risk_cause(
        &mut causes,
        report
            .dex_cancel_outcomes
            .get("error")
            .copied()
            .unwrap_or(0)
            > 0,
        "red",
        25,
        "DEX cancel error outcomes observed",
    );
    if let Some(db) = &report.reconcile_db {
        let expired_or_errored = db
            .open_entries
            .iter()
            .any(|entry| entry.status == "expired" || entry.status == "errored");
        push_risk_cause(
            &mut causes,
            expired_or_errored,
            "red",
            25,
            "reconcile DB has expired/errored open entries",
        );
        push_risk_cause(
            &mut causes,
            !db.open_entries.is_empty(),
            "yellow",
            15,
            "reconcile DB has open entries requiring follow-up",
        );
    }
    push_risk_cause(
        &mut causes,
        report.dex_pending_timeouts > 0,
        "yellow",
        15,
        "DEX pending timeouts observed",
    );
    push_risk_cause(
        &mut causes,
        report.reconcile_inspect_errors > 0,
        "yellow",
        15,
        "reconcile inspect errors observed",
    );
    push_risk_cause(
        &mut causes,
        report
            .dex_cancel_outcomes
            .get("unknown")
            .copied()
            .unwrap_or(0)
            > 0,
        "yellow",
        10,
        "DEX cancel unknown outcomes observed",
    );
    push_risk_cause(
        &mut causes,
        report.losing_executions > 0,
        "yellow",
        10,
        "losing terminal executions observed",
    );
    if let Some(trades) = &report.trade_log {
        push_risk_cause(
            &mut causes,
            trades.net_pnl_usd < Decimal::ZERO,
            "yellow",
            15,
            "trade log net PnL is negative",
        );
    }
    if let Some(metrics) = &report.metrics_snapshot {
        push_risk_cause(
            &mut causes,
            metric_total(metrics, "peanut_flashbots_relay_errors_total") > 0.0,
            "yellow",
            15,
            "Flashbots relay errors present",
        );
        push_risk_cause(
            &mut causes,
            metric_total(metrics, "peanut_flashbots_bundles_not_included_total") > 0.0,
            "yellow",
            10,
            "Flashbots bundle not-included count is non-zero",
        );
        push_risk_cause(
            &mut causes,
            metric_total(metrics, "peanut_reconcile_expired_total") > 0.0,
            "red",
            20,
            "Prometheus snapshot reports expired reconcile entries",
        );
    }
    causes
}

fn push_risk_cause(
    causes: &mut Vec<RiskCause>,
    enabled: bool,
    severity: &str,
    score: u16,
    message: &str,
) {
    if enabled {
        causes.push(RiskCause {
            severity: severity.into(),
            score,
            message: message.into(),
        });
    }
}

fn load_report_comparison(
    path: &PathBuf,
    current: &DailyReport,
) -> Result<ReportComparison, Box<dyn std::error::Error>> {
    let previous: DailyReport = serde_json::from_reader(File::open(path)?)?;
    Ok(compare_reports(&previous, current))
}

fn compare_reports(previous: &DailyReport, current: &DailyReport) -> ReportComparison {
    ReportComparison {
        previous_date: previous.date,
        previous_health: previous.health.clone(),
        health_changed: previous.health != current.health,
        risk_score_delta: i64::from(current.risk_score) - i64::from(previous.risk_score),
        total_events_delta: usize_delta(current.total_events, previous.total_events),
        executions_delta: usize_delta(current.executions_total, previous.executions_total),
        realized_pnl_usd_delta: current.realized_pnl_usd - previous.realized_pnl_usd,
        trade_net_pnl_usd_delta: match (&previous.trade_log, &current.trade_log) {
            (Some(prev), Some(curr)) => Some(curr.net_pnl_usd - prev.net_pnl_usd),
            _ => None,
        },
        dex_pending_timeouts_delta: usize_delta(
            current.dex_pending_timeouts,
            previous.dex_pending_timeouts,
        ),
        reconcile_open_entries_delta: match (&previous.reconcile_db, &current.reconcile_db) {
            (Some(prev), Some(curr)) => Some(usize_delta(
                curr.open_entries.len(),
                prev.open_entries.len(),
            )),
            _ => None,
        },
        flashbots_inclusion_rate_delta: match (
            &previous.metrics_snapshot,
            &current.metrics_snapshot,
        ) {
            (Some(prev), Some(curr)) => {
                match (prev.flashbots_inclusion_rate, curr.flashbots_inclusion_rate) {
                    (Some(prev_rate), Some(curr_rate)) => Some(curr_rate - prev_rate),
                    _ => None,
                }
            }
            _ => None,
        },
    }
}

fn usize_delta(current: usize, previous: usize) -> i64 {
    current as i64 - previous as i64
}

fn alert_summary(report: &DailyReport, telegram_chat_id: &str) -> AlertSummary {
    AlertSummary {
        date: report.date,
        health: report.health.clone(),
        risk_score: report.risk_score,
        top_risk_causes: report.top_risk_causes.clone(),
        risk_notes: report.risk_notes.clone(),
        telegram_payload: telegram_payload(report, telegram_chat_id),
        output_hint: "Use the generated Markdown/HTML report for full operational context".into(),
    }
}

fn telegram_payload(report: &DailyReport, chat_id: &str) -> TelegramPayload {
    TelegramPayload {
        chat_id: chat_id.into(),
        text: telegram_alert_text(report),
        parse_mode: "HTML".into(),
        disable_web_page_preview: true,
    }
}

fn telegram_alert_text(report: &DailyReport) -> String {
    let icon = match report.health.as_str() {
        "GREEN" => "✅",
        "YELLOW" => "⚠️",
        "RED" => "🚨",
        _ => "ℹ️",
    };
    let mut text = String::new();
    text.push_str(&format!(
        "{icon} <b>Daily Ops Report</b>\nDate: <code>{}</code>\nHealth: <b>{}</b>\nRisk: <b>{}/100</b>\n",
        html_escape(&report.date.to_string()),
        html_escape(&report.health),
        report.risk_score
    ));
    text.push_str(&format!(
        "\nEvents: <code>{}</code>\nExecutions: <code>{}</code>\nPnL: <code>{}</code>\nDEX timeouts: <code>{}</code>\n",
        report.total_events,
        report.executions_total,
        html_escape(&report.realized_pnl_usd.to_string()),
        report.dex_pending_timeouts
    ));
    if let Some(trades) = &report.trade_log {
        text.push_str(&format!(
            "Trade PnL: <code>{}</code> over <code>{}</code> trades\n",
            html_escape(&trades.net_pnl_usd.to_string()),
            trades.trades_total
        ));
    }
    if let Some(db) = &report.reconcile_db {
        text.push_str(&format!(
            "Open reconcile: <code>{}</code>\n",
            db.open_entries.len()
        ));
    }
    if let Some(comparison) = &report.comparison {
        text.push_str(&format!(
            "\n<b>Day-over-day</b>\nRisk Δ: <code>{}</code>\nPnL Δ: <code>{}</code>\nTimeouts Δ: <code>{}</code>\n",
            comparison.risk_score_delta,
            html_escape(&comparison.realized_pnl_usd_delta.to_string()),
            comparison.dex_pending_timeouts_delta
        ));
    }
    if !report.top_risk_causes.is_empty() {
        text.push_str("\n<b>Top risks</b>\n");
        for cause in &report.top_risk_causes {
            text.push_str(&format!(
                "• <b>{}</b> +{}: {}\n",
                html_escape(&cause.severity.to_ascii_uppercase()),
                cause.score,
                html_escape(&cause.message)
            ));
        }
    }
    let chars: Vec<char> = text.chars().collect();
    if chars.len() > 3900 {
        chars.into_iter().take(3900).collect::<String>() + "\n…"
    } else {
        text
    }
}

fn write_report_file(path: &PathBuf, body: impl AsRef<[u8]>) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, body)
}

fn load_metrics_snapshot(
    path: &PathBuf,
) -> Result<MetricsSnapshotSummary, Box<dyn std::error::Error>> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let mut summary = MetricsSnapshotSummary {
        path: path.display().to_string(),
        ..MetricsSnapshotSummary::default()
    };
    for line in reader.lines() {
        let line = line?;
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        match parse_prometheus_sample(line) {
            Some((series_key, metric_name, value)) if metric_name.starts_with("peanut_") => {
                *summary.totals_by_metric.entry(metric_name).or_default() += value;
                summary.series.insert(series_key, value);
            }
            Some(_) => {}
            None => summary.parse_errors += 1,
        }
    }
    let submitted = metric_total(&summary, "peanut_flashbots_bundles_submitted_total");
    if submitted > 0.0 {
        summary.flashbots_inclusion_rate =
            Some(metric_total(&summary, "peanut_flashbots_bundles_included_total") / submitted);
    }
    Ok(summary)
}

fn parse_prometheus_sample(line: &str) -> Option<(String, String, f64)> {
    let (sample, value_raw) = line.rsplit_once(char::is_whitespace)?;
    let value = value_raw.parse::<f64>().ok()?;
    let metric_name = sample
        .split_once('{')
        .map(|(name, _)| name)
        .unwrap_or(sample)
        .to_string();
    Some((sample.to_string(), metric_name, value))
}

fn load_trade_log_summary(
    path: &PathBuf,
    date: NaiveDate,
) -> Result<TradeLogSummary, Box<dyn std::error::Error>> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let mut summary = TradeLogSummary {
        path: path.display().to_string(),
        ..TradeLogSummary::default()
    };
    let mut bps_sum = Decimal::ZERO;
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<ArbRecord>(&line) {
            Ok(trade) if trade.timestamp.date_naive() == date => {
                let pnl = trade.net_pnl();
                summary.trades_total += 1;
                summary.net_pnl_usd += pnl;
                summary.fees_usd += trade.total_fees();
                summary.notional_usd += trade.notional();
                bps_sum += trade.net_pnl_bps();
                *summary
                    .trades_by_pair
                    .entry(trade.buy_leg.symbol.clone())
                    .or_default() += 1;
                if pnl > Decimal::ZERO {
                    summary.winning_trades += 1;
                } else if pnl < Decimal::ZERO {
                    summary.losing_trades += 1;
                }
                if summary.trades_total == 1 {
                    summary.best_trade_pnl_usd = pnl;
                    summary.worst_trade_pnl_usd = pnl;
                } else {
                    summary.best_trade_pnl_usd = summary.best_trade_pnl_usd.max(pnl);
                    summary.worst_trade_pnl_usd = summary.worst_trade_pnl_usd.min(pnl);
                }
            }
            Ok(_) => {}
            Err(_) => summary.parse_errors += 1,
        }
    }
    if summary.trades_total > 0 {
        summary.avg_net_pnl_bps = bps_sum / Decimal::from(summary.trades_total as u64);
    }
    Ok(summary)
}

fn load_reconcile_db_snapshot(
    path: &PathBuf,
    date: NaiveDate,
) -> Result<ReconcileDbSnapshot, Box<dyn std::error::Error>> {
    let conn = Connection::open(path)?;
    let start = date
        .and_hms_opt(0, 0, 0)
        .expect("valid midnight")
        .and_utc()
        .timestamp();
    let end = start + 24 * 60 * 60;
    let mut snapshot = ReconcileDbSnapshot {
        path: path.display().to_string(),
        ..ReconcileDbSnapshot::default()
    };
    {
        let mut stmt = conn.prepare(
            "SELECT status, COUNT(*) FROM pending_reconcile
             WHERE started_at >= ?1 AND started_at < ?2
             GROUP BY status",
        )?;
        let rows = stmt.query_map(params![start, end], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;
        for row in rows {
            let (status, count) = row?;
            snapshot
                .status_counts_for_day
                .insert(status, count.max(0) as usize);
        }
    }
    {
        let mut stmt = conn.prepare(
            "SELECT signal_id, tx_hash, payload, status, started_at, resolution
             FROM pending_reconcile
             WHERE status IN ('pending', 'expired', 'errored')
             ORDER BY started_at ASC
             LIMIT 50",
        )?;
        let now = Utc::now().timestamp();
        let rows = stmt.query_map([], |row| {
            let payload: String = row.get(2)?;
            let payload_value: Value = serde_json::from_str(&payload).unwrap_or(Value::Null);
            let started_at_unix: i64 = row.get(4)?;
            Ok(ReconcileDbOpenEntry {
                signal_id: row.get(0)?,
                tx_hash: row.get(1)?,
                pair: string_field(&payload_value, "pair"),
                status: row.get(3)?,
                started_at_unix,
                age_secs: now.saturating_sub(started_at_unix),
                resolution: row.get(5)?,
            })
        })?;
        for row in rows {
            let entry = row?;
            if entry.status == "pending" {
                snapshot.oldest_pending_age_secs = Some(
                    snapshot
                        .oldest_pending_age_secs
                        .map(|current| current.max(entry.age_secs))
                        .unwrap_or(entry.age_secs),
                );
            }
            snapshot.open_entries.push(entry);
        }
    }
    Ok(snapshot)
}

fn render_markdown(report: &DailyReport) -> String {
    let mut out = String::new();
    out.push_str(&format!("# Daily Ops Report - {}\n\n", report.date));
    out.push_str(&format!("**Health:** {}\n\n", report.health));
    out.push_str(&format!("**Risk score:** {}/100\n\n", report.risk_score));
    out.push_str("## Summary\n\n");
    out.push_str(&format!("- Total events: {}\n", report.total_events));
    out.push_str(&format!("- Parse errors: {}\n", report.parse_errors));
    out.push_str(&format!(
        "- Bot started/stopped: {}/{}\n",
        report.bot_started, report.bot_stopped
    ));
    out.push_str(&format!("- Halted stops: {}\n", report.halted_stops));
    out.push_str("\n## Executions\n\n");
    out.push_str(&format!("- Total: {}\n", report.executions_total));
    out.push_str(&format!(
        "- Realized PnL USD: {}\n",
        report.realized_pnl_usd
    ));
    out.push_str(&format!(
        "- Wins/Losses: {}/{}\n",
        report.winning_executions, report.losing_executions
    ));
    push_map(&mut out, "States", &report.execution_states);
    push_map(&mut out, "Pairs", &report.executions_by_pair);
    if let Some(trades) = &report.trade_log {
        out.push_str("\n## Trade Log PnL\n\n");
        out.push_str(&format!("- Path: {}\n", trades.path));
        out.push_str(&format!("- Trades: {}\n", trades.trades_total));
        out.push_str(&format!("- Parse errors: {}\n", trades.parse_errors));
        out.push_str(&format!("- Net PnL USD: {}\n", trades.net_pnl_usd));
        out.push_str(&format!("- Fees USD: {}\n", trades.fees_usd));
        out.push_str(&format!("- Notional USD: {}\n", trades.notional_usd));
        out.push_str(&format!("- Avg net PnL bps: {}\n", trades.avg_net_pnl_bps));
        out.push_str(&format!(
            "- Best/Worst trade PnL USD: {}/{}\n",
            trades.best_trade_pnl_usd, trades.worst_trade_pnl_usd
        ));
        out.push_str(&format!(
            "- Winning/Losing trades: {}/{}\n",
            trades.winning_trades, trades.losing_trades
        ));
        push_map(&mut out, "Trade pairs", &trades.trades_by_pair);
    }
    if let Some(metrics) = &report.metrics_snapshot {
        out.push_str("\n## Metrics Snapshot\n\n");
        out.push_str(&format!("- Path: {}\n", metrics.path));
        out.push_str(&format!("- Parse errors: {}\n", metrics.parse_errors));
        if let Some(rate) = metrics.flashbots_inclusion_rate {
            out.push_str(&format!(
                "- Flashbots inclusion rate: {:.2}%\n",
                rate * 100.0
            ));
        }
        push_selected_metrics(
            &mut out,
            "Flashbots",
            metrics,
            &[
                "peanut_flashbots_simulations_total",
                "peanut_flashbots_bundles_submitted_total",
                "peanut_flashbots_bundles_included_total",
                "peanut_flashbots_bundles_not_included_total",
                "peanut_flashbots_relay_errors_total",
            ],
        );
        push_selected_metrics(
            &mut out,
            "DEX/Reconcile",
            metrics,
            &[
                "peanut_dex_pending_timeouts_total",
                "peanut_dex_cancel_attempts_total",
                "peanut_dex_cancel_outcomes_total",
                "peanut_reconcile_entries_total",
                "peanut_reconcile_resolved_total",
                "peanut_reconcile_expired_total",
                "peanut_leg_outcomes_total",
            ],
        );
    }
    if let Some(comparison) = &report.comparison {
        out.push_str("\n## Day-over-day Comparison\n\n");
        out.push_str(&format!("- Previous date: {}\n", comparison.previous_date));
        out.push_str(&format!(
            "- Previous health: {}\n",
            comparison.previous_health
        ));
        out.push_str(&format!(
            "- Health changed: {}\n",
            comparison.health_changed
        ));
        out.push_str(&format!(
            "- Risk score delta: {}\n",
            comparison.risk_score_delta
        ));
        out.push_str(&format!(
            "- Events delta: {}\n",
            comparison.total_events_delta
        ));
        out.push_str(&format!(
            "- Executions delta: {}\n",
            comparison.executions_delta
        ));
        out.push_str(&format!(
            "- Realized PnL delta USD: {}\n",
            comparison.realized_pnl_usd_delta
        ));
        if let Some(delta) = comparison.trade_net_pnl_usd_delta {
            out.push_str(&format!("- Trade net PnL delta USD: {delta}\n"));
        }
        out.push_str(&format!(
            "- DEX pending timeouts delta: {}\n",
            comparison.dex_pending_timeouts_delta
        ));
        if let Some(delta) = comparison.reconcile_open_entries_delta {
            out.push_str(&format!("- Reconcile open entries delta: {delta}\n"));
        }
        if let Some(delta) = comparison.flashbots_inclusion_rate_delta {
            out.push_str(&format!(
                "- Flashbots inclusion rate delta: {:.2}%\n",
                delta * 100.0
            ));
        }
    }
    out.push_str("\n## DEX Safety\n\n");
    out.push_str(&format!(
        "- Pending timeouts: {}\n",
        report.dex_pending_timeouts
    ));
    push_map(&mut out, "Cancel outcomes", &report.dex_cancel_outcomes);
    out.push_str("\n## Reconcile\n\n");
    out.push_str(&format!("- Enqueued: {}\n", report.reconcile_enqueued));
    out.push_str(&format!(
        "- Enqueue failed: {}\n",
        report.reconcile_enqueue_failed
    ));
    out.push_str(&format!(
        "- Inspect errors: {}\n",
        report.reconcile_inspect_errors
    ));
    push_map(&mut out, "Resolved", &report.reconcile_resolved);
    if let Some(db) = &report.reconcile_db {
        out.push_str("\n### Reconcile DB Snapshot\n\n");
        out.push_str(&format!("- Path: {}\n", db.path));
        out.push_str(&format!("- Open entries: {}\n", db.open_entries.len()));
        if let Some(age) = db.oldest_pending_age_secs {
            out.push_str(&format!("- Oldest pending age seconds: {age}\n"));
        }
        push_map(
            &mut out,
            "DB status counts for day",
            &db.status_counts_for_day,
        );
        if !db.open_entries.is_empty() {
            out.push_str("\n### DB Open Entries\n\n");
            for entry in &db.open_entries {
                out.push_str(&format!(
                    "- {} {} status={} pair={} age_s={} resolution={}\n",
                    entry.signal_id,
                    entry.tx_hash,
                    entry.status,
                    entry.pair.as_deref().unwrap_or("unknown"),
                    entry.age_secs,
                    entry.resolution.as_deref().unwrap_or("")
                ));
            }
        }
    }
    out.push_str("\n## Top Risk Causes\n\n");
    if report.top_risk_causes.is_empty() {
        out.push_str("- None\n");
    } else {
        for cause in &report.top_risk_causes {
            out.push_str(&format!(
                "- [{} +{}] {}\n",
                cause.severity, cause.score, cause.message
            ));
        }
    }
    out.push_str("\n## Risk Notes\n\n");
    if report.risk_notes.is_empty() {
        out.push_str("- None\n");
    } else {
        for note in &report.risk_notes {
            out.push_str(&format!("- {}\n", note));
        }
    }
    out
}

fn render_html(report: &DailyReport) -> String {
    let health_class = report.health.to_ascii_lowercase();
    let mut out = String::new();
    out.push_str("<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">");
    out.push_str("<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">");
    out.push_str(&format!(
        "<title>Daily Ops Report {}</title>",
        html_escape(&report.date.to_string())
    ));
    out.push_str(
        "<style>
        :root{color-scheme:dark;background:#0f172a;color:#e2e8f0;font-family:Inter,Arial,sans-serif}
        body{margin:0;padding:32px;background:linear-gradient(135deg,#0f172a,#111827)}
        main{max-width:1180px;margin:0 auto}
        h1{margin:0 0 8px;font-size:32px}
        h2{margin:0 0 16px;font-size:18px;color:#bfdbfe}
        .grid{display:grid;grid-template-columns:repeat(auto-fit,minmax(230px,1fr));gap:16px;margin:20px 0}
        .card{background:rgba(15,23,42,.84);border:1px solid rgba(148,163,184,.24);border-radius:16px;padding:18px;box-shadow:0 10px 30px rgba(0,0,0,.24)}
        .metric{font-size:28px;font-weight:700;margin-top:6px}
        .label{color:#94a3b8;font-size:13px;text-transform:uppercase;letter-spacing:.08em}
        .pill{display:inline-flex;align-items:center;border-radius:999px;padding:6px 12px;font-weight:700}
        .green{background:#064e3b;color:#a7f3d0}.yellow{background:#713f12;color:#fde68a}.red{background:#7f1d1d;color:#fecaca}
        table{width:100%;border-collapse:collapse;font-size:14px}td,th{padding:9px 10px;border-bottom:1px solid rgba(148,163,184,.18);text-align:left}th{color:#bfdbfe}
        ul{margin:0;padding-left:20px}.muted{color:#94a3b8}.mono{font-family:ui-monospace,SFMono-Regular,Menlo,monospace}
        </style></head><body><main>",
    );
    out.push_str(&format!(
        "<h1>Daily Ops Report - {}</h1><div class=\"pill {}\">Health: {} | Risk: {}/100</div>",
        html_escape(&report.date.to_string()),
        html_escape(&health_class),
        html_escape(&report.health),
        report.risk_score
    ));
    out.push_str("<section class=\"grid\">");
    push_html_card(&mut out, "Events", report.total_events, "total");
    push_html_card(&mut out, "Executions", report.executions_total, "terminal");
    push_html_card(
        &mut out,
        "DEX Timeouts",
        report.dex_pending_timeouts,
        "pending",
    );
    push_html_card(
        &mut out,
        "Reconcile Enqueued",
        report.reconcile_enqueued,
        "entries",
    );
    out.push_str("</section>");

    out.push_str("<section class=\"grid\">");
    out.push_str("<div class=\"card\"><h2>Execution PnL</h2>");
    out.push_str(&format!(
        "<div class=\"label\">Realized PnL USD</div><div class=\"metric\">{}</div>",
        html_escape(&report.realized_pnl_usd.to_string())
    ));
    out.push_str(&format!(
        "<p class=\"muted\">Wins/Losses: {}/{}</p>",
        report.winning_executions, report.losing_executions
    ));
    out.push_str("</div>");
    if let Some(trades) = &report.trade_log {
        out.push_str("<div class=\"card\"><h2>Trade Log PnL</h2>");
        out.push_str(&format!(
            "<div class=\"label\">Net PnL USD</div><div class=\"metric\">{}</div>",
            html_escape(&trades.net_pnl_usd.to_string())
        ));
        out.push_str(&format!(
            "<p class=\"muted\">Trades: {} | Fees: {} | Avg bps: {}</p>",
            trades.trades_total, trades.fees_usd, trades.avg_net_pnl_bps
        ));
        out.push_str("</div>");
    }
    if let Some(metrics) = &report.metrics_snapshot {
        out.push_str("<div class=\"card\"><h2>Flashbots</h2>");
        let rate = metrics
            .flashbots_inclusion_rate
            .map(|value| format!("{:.2}%", value * 100.0))
            .unwrap_or_else(|| "n/a".into());
        out.push_str(&format!(
            "<div class=\"label\">Inclusion rate</div><div class=\"metric\">{}</div>",
            html_escape(&rate)
        ));
        out.push_str(&format!(
            "<p class=\"muted\">Submitted: {} | Included: {} | Not included: {}</p>",
            metric_total(metrics, "peanut_flashbots_bundles_submitted_total"),
            metric_total(metrics, "peanut_flashbots_bundles_included_total"),
            metric_total(metrics, "peanut_flashbots_bundles_not_included_total"),
        ));
        out.push_str("</div>");
    }
    if let Some(comparison) = &report.comparison {
        out.push_str("<div class=\"card\"><h2>Day-over-day</h2>");
        out.push_str(&format!(
            "<div class=\"label\">Risk score delta</div><div class=\"metric\">{}</div>",
            comparison.risk_score_delta
        ));
        out.push_str(&format!(
            "<p class=\"muted\">Prev: {} {} | PnL Δ: {} | Timeouts Δ: {}</p>",
            html_escape(&comparison.previous_date.to_string()),
            html_escape(&comparison.previous_health),
            html_escape(&comparison.realized_pnl_usd_delta.to_string()),
            comparison.dex_pending_timeouts_delta
        ));
        out.push_str("</div>");
    }
    out.push_str("</section>");

    push_html_map("Execution states", &report.execution_states, &mut out);
    push_html_map("DEX cancel outcomes", &report.dex_cancel_outcomes, &mut out);
    push_html_map("Reconcile resolved", &report.reconcile_resolved, &mut out);
    if let Some(db) = &report.reconcile_db {
        out.push_str("<section class=\"card\"><h2>Reconcile DB Open Entries</h2>");
        if db.open_entries.is_empty() {
            out.push_str("<p class=\"muted\">None</p>");
        } else {
            out.push_str("<table><thead><tr><th>Signal</th><th>Tx</th><th>Status</th><th>Pair</th><th>Age s</th></tr></thead><tbody>");
            for entry in &db.open_entries {
                out.push_str(&format!(
                    "<tr><td class=\"mono\">{}</td><td class=\"mono\">{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
                    html_escape(&entry.signal_id),
                    html_escape(&entry.tx_hash),
                    html_escape(&entry.status),
                    html_escape(entry.pair.as_deref().unwrap_or("unknown")),
                    entry.age_secs
                ));
            }
            out.push_str("</tbody></table>");
        }
        out.push_str("</section>");
    }
    out.push_str("<section class=\"card\"><h2>Top Risk Causes</h2>");
    if report.top_risk_causes.is_empty() {
        out.push_str("<p class=\"muted\">None</p>");
    } else {
        out.push_str(
            "<table><thead><tr><th>Severity</th><th>Score</th><th>Message</th></tr></thead><tbody>",
        );
        for cause in &report.top_risk_causes {
            out.push_str(&format!(
                "<tr><td>{}</td><td>{}</td><td>{}</td></tr>",
                html_escape(&cause.severity),
                cause.score,
                html_escape(&cause.message)
            ));
        }
        out.push_str("</tbody></table>");
    }
    out.push_str("</section><section class=\"card\"><h2>Risk Notes</h2>");
    if report.risk_notes.is_empty() {
        out.push_str("<p class=\"muted\">None</p>");
    } else {
        out.push_str("<ul>");
        for note in &report.risk_notes {
            out.push_str(&format!("<li>{}</li>", html_escape(note)));
        }
        out.push_str("</ul>");
    }
    out.push_str("</section></main></body></html>");
    out
}

fn render_csv(report: &DailyReport) -> Result<String, Box<dyn std::error::Error>> {
    let mut bytes = Vec::new();
    {
        let mut writer = csv::Writer::from_writer(&mut bytes);
        writer.write_record(["section", "key", "value"])?;
        writer.write_record(["summary", "date", &report.date.to_string()])?;
        writer.write_record(["summary", "health", &report.health])?;
        writer.write_record(["summary", "risk_score", &report.risk_score.to_string()])?;
        writer.write_record(["summary", "total_events", &report.total_events.to_string()])?;
        writer.write_record(["summary", "parse_errors", &report.parse_errors.to_string()])?;
        writer.write_record(["execution", "total", &report.executions_total.to_string()])?;
        writer.write_record([
            "execution",
            "realized_pnl_usd",
            &report.realized_pnl_usd.to_string(),
        ])?;
        writer.write_record([
            "dex",
            "pending_timeouts",
            &report.dex_pending_timeouts.to_string(),
        ])?;
        writer.write_record([
            "reconcile",
            "enqueued",
            &report.reconcile_enqueued.to_string(),
        ])?;
        writer.write_record([
            "reconcile",
            "enqueue_failed",
            &report.reconcile_enqueue_failed.to_string(),
        ])?;
        for (key, value) in &report.execution_states {
            writer.write_record(["execution_state", key, &value.to_string()])?;
        }
        for (key, value) in &report.dex_cancel_outcomes {
            writer.write_record(["dex_cancel_outcome", key, &value.to_string()])?;
        }
        for (key, value) in &report.reconcile_resolved {
            writer.write_record(["reconcile_resolved", key, &value.to_string()])?;
        }
        for cause in &report.top_risk_causes {
            writer.write_record([
                "risk_cause",
                &cause.severity,
                &format!("{}|{}", cause.score, cause.message),
            ])?;
        }
        if let Some(trades) = &report.trade_log {
            writer.write_record([
                "trade_log",
                "trades_total",
                &trades.trades_total.to_string(),
            ])?;
            writer.write_record([
                "trade_log",
                "parse_errors",
                &trades.parse_errors.to_string(),
            ])?;
            writer.write_record(["trade_log", "net_pnl_usd", &trades.net_pnl_usd.to_string()])?;
            writer.write_record(["trade_log", "fees_usd", &trades.fees_usd.to_string()])?;
            writer.write_record([
                "trade_log",
                "notional_usd",
                &trades.notional_usd.to_string(),
            ])?;
            writer.write_record([
                "trade_log",
                "avg_net_pnl_bps",
                &trades.avg_net_pnl_bps.to_string(),
            ])?;
            for (key, value) in &trades.trades_by_pair {
                writer.write_record(["trade_log_pair", key, &value.to_string()])?;
            }
        }
        if let Some(metrics) = &report.metrics_snapshot {
            writer.write_record(["metrics", "parse_errors", &metrics.parse_errors.to_string()])?;
            if let Some(rate) = metrics.flashbots_inclusion_rate {
                writer.write_record(["metrics", "flashbots_inclusion_rate", &rate.to_string()])?;
            }
            for (key, value) in &metrics.totals_by_metric {
                writer.write_record(["metrics_total", key, &value.to_string()])?;
            }
        }
        if let Some(db) = &report.reconcile_db {
            writer.write_record([
                "reconcile_db",
                "open_entries",
                &db.open_entries.len().to_string(),
            ])?;
            if let Some(age) = db.oldest_pending_age_secs {
                writer.write_record([
                    "reconcile_db",
                    "oldest_pending_age_secs",
                    &age.to_string(),
                ])?;
            }
            for (key, value) in &db.status_counts_for_day {
                writer.write_record(["reconcile_db_status_for_day", key, &value.to_string()])?;
            }
            for entry in &db.open_entries {
                writer.write_record([
                    "reconcile_db_open",
                    &entry.signal_id,
                    &format!(
                        "{}|{}|{}|{}",
                        entry.status,
                        entry.tx_hash,
                        entry.pair.as_deref().unwrap_or("unknown"),
                        entry.age_secs
                    ),
                ])?;
            }
        }
        if let Some(comparison) = &report.comparison {
            writer.write_record([
                "comparison",
                "previous_date",
                &comparison.previous_date.to_string(),
            ])?;
            writer.write_record(["comparison", "previous_health", &comparison.previous_health])?;
            writer.write_record([
                "comparison",
                "health_changed",
                &comparison.health_changed.to_string(),
            ])?;
            writer.write_record([
                "comparison",
                "risk_score_delta",
                &comparison.risk_score_delta.to_string(),
            ])?;
            writer.write_record([
                "comparison",
                "realized_pnl_usd_delta",
                &comparison.realized_pnl_usd_delta.to_string(),
            ])?;
            writer.write_record([
                "comparison",
                "dex_pending_timeouts_delta",
                &comparison.dex_pending_timeouts_delta.to_string(),
            ])?;
        }
        writer.flush()?;
    }
    Ok(String::from_utf8(bytes)?)
}

fn push_map(out: &mut String, title: &str, values: &BTreeMap<String, usize>) {
    out.push_str(&format!("\n### {title}\n\n"));
    if values.is_empty() {
        out.push_str("- None\n");
        return;
    }
    for (key, value) in values {
        out.push_str(&format!("- {}: {}\n", key, value));
    }
}

fn push_selected_metrics(
    out: &mut String,
    title: &str,
    metrics: &MetricsSnapshotSummary,
    names: &[&str],
) {
    out.push_str(&format!("\n### {title} Metrics\n\n"));
    let mut wrote = false;
    for name in names {
        if let Some(value) = metrics.totals_by_metric.get(*name) {
            out.push_str(&format!("- {}: {}\n", name, value));
            wrote = true;
        }
    }
    if !wrote {
        out.push_str("- None\n");
    }
}

fn push_html_card(out: &mut String, title: &str, value: usize, subtitle: &str) {
    out.push_str(&format!(
        "<div class=\"card\"><div class=\"label\">{}</div><div class=\"metric\">{}</div><p class=\"muted\">{}</p></div>",
        html_escape(title),
        value,
        html_escape(subtitle)
    ));
}

fn push_html_map(title: &str, values: &BTreeMap<String, usize>, out: &mut String) {
    out.push_str(&format!(
        "<section class=\"card\"><h2>{}</h2>",
        html_escape(title)
    ));
    if values.is_empty() {
        out.push_str("<p class=\"muted\">None</p>");
    } else {
        out.push_str("<table><thead><tr><th>Key</th><th>Value</th></tr></thead><tbody>");
        for (key, value) in values {
            out.push_str(&format!(
                "<tr><td>{}</td><td>{}</td></tr>",
                html_escape(key),
                value
            ));
        }
        out.push_str("</tbody></table>");
    }
    out.push_str("</section>");
}

fn html_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn should_fail_on_health(health: &str, threshold: Option<&str>) -> bool {
    match threshold {
        Some("yellow") => matches!(health, "YELLOW" | "RED"),
        Some("red") => health == "RED",
        _ => false,
    }
}

fn metric_total(metrics: &MetricsSnapshotSummary, name: &str) -> f64 {
    metrics.totals_by_metric.get(name).copied().unwrap_or(0.0)
}

fn string_field(fields: &Value, key: &str) -> Option<String> {
    fields.get(key)?.as_str().map(ToOwned::to_owned)
}

fn bool_field(fields: &Value, key: &str) -> Option<bool> {
    fields.get(key)?.as_bool()
}

fn decimal_field(fields: &Value, key: &str) -> Option<Decimal> {
    match fields.get(key)? {
        Value::String(s) => Decimal::from_str_exact(s).ok(),
        Value::Number(n) => Decimal::from_str_exact(&n.to_string()).ok(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    use serde_json::json;
    use tempfile::tempdir;

    #[test]
    fn summarizes_daily_events() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let mut file = File::create(&path).unwrap();
        for event in [
            json!({"ts":"2026-05-07T00:00:01Z","event_type":"bot_started","fields":{}}),
            json!({"ts":"2026-05-07T00:01:00Z","event_type":"execution_terminal","fields":{"state":"Done","pair":"ETH/USDC","actual_net_pnl":"12.5"}}),
            json!({"ts":"2026-05-07T00:02:00Z","event_type":"dex_pending_timeout","fields":{"pool_kind":"v2"}}),
            json!({"ts":"2026-05-07T00:03:00Z","event_type":"dex_cancel_outcome","fields":{"outcome":"unknown"}}),
            json!({"ts":"2026-05-08T00:00:00Z","event_type":"bot_started","fields":{}}),
        ] {
            writeln!(file, "{event}").unwrap();
        }

        let report = build_report(
            &path,
            NaiveDate::from_ymd_opt(2026, 5, 7).unwrap(),
            None,
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(report.total_events, 4);
        assert_eq!(report.executions_total, 1);
        assert_eq!(
            report.realized_pnl_usd,
            Decimal::from_str_exact("12.5").unwrap()
        );
        assert_eq!(report.dex_pending_timeouts, 1);
        assert_eq!(report.dex_cancel_outcomes.get("unknown"), Some(&1));
        assert_eq!(report.health, "YELLOW");
    }

    #[test]
    fn includes_reconcile_db_snapshot() {
        let dir = tempdir().unwrap();
        let events_path = dir.path().join("events.jsonl");
        File::create(&events_path).unwrap();
        let db_path = dir.path().join("reconcile.db");
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE pending_reconcile (
                signal_id   TEXT PRIMARY KEY,
                tx_hash     TEXT NOT NULL,
                payload     TEXT NOT NULL,
                started_at  INTEGER NOT NULL,
                status      TEXT NOT NULL DEFAULT 'pending',
                resolution  TEXT,
                resolved_at INTEGER
            );",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO pending_reconcile
                (signal_id, tx_hash, payload, started_at, status)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                "sig-db",
                "0xabc",
                json!({"pair":"ETH/USDC"}).to_string(),
                1778112000_i64,
                "pending",
            ],
        )
        .unwrap();

        let report = build_report(
            &events_path,
            NaiveDate::from_ymd_opt(2026, 5, 7).unwrap(),
            Some(&db_path),
            None,
            None,
            None,
        )
        .unwrap();
        let db = report.reconcile_db.as_ref().unwrap();
        assert_eq!(db.status_counts_for_day.get("pending"), Some(&1));
        assert_eq!(db.open_entries.len(), 1);
        assert_eq!(db.open_entries[0].pair.as_deref(), Some("ETH/USDC"));
        assert_eq!(report.health, "YELLOW");
    }

    #[test]
    fn includes_trade_log_summary() {
        let dir = tempdir().unwrap();
        let events_path = dir.path().join("events.jsonl");
        File::create(&events_path).unwrap();
        let trade_path = dir.path().join("trades.jsonl");
        let mut file = File::create(&trade_path).unwrap();
        writeln!(
            file,
            "{}",
            json!({
                "id": "trade-1",
                "timestamp": "2026-05-07T00:00:00Z",
                "buy_leg": {
                    "id": "buy-1",
                    "timestamp": "2026-05-07T00:00:00Z",
                    "venue": "Binance",
                    "symbol": "ETH/USDC",
                    "side": "buy",
                    "amount": "1",
                    "price": "100",
                    "fee": "1",
                    "fee_asset": "USDC"
                },
                "sell_leg": {
                    "id": "sell-1",
                    "timestamp": "2026-05-07T00:01:00Z",
                    "venue": "Wallet",
                    "symbol": "ETH/USDC",
                    "side": "sell",
                    "amount": "1",
                    "price": "110",
                    "fee": "1",
                    "fee_asset": "USDC"
                },
                "gas_cost_usd": "2",
                "expected_gross_pnl_usd": "10",
                "expected_fees_usd": "4",
                "expected_net_pnl_usd": "6",
                "actual_gross_pnl_usd": "10",
                "actual_fees_usd": "4",
                "actual_cex_fee_usd": "2",
                "actual_onchain_gas_fee_usd": "2",
                "actual_net_pnl_usd": "6"
            })
        )
        .unwrap();

        let report = build_report(
            &events_path,
            NaiveDate::from_ymd_opt(2026, 5, 7).unwrap(),
            None,
            Some(&trade_path),
            None,
            None,
        )
        .unwrap();
        let trades = report.trade_log.as_ref().unwrap();
        assert_eq!(trades.trades_total, 1);
        assert_eq!(trades.net_pnl_usd, Decimal::from(6));
        assert_eq!(trades.fees_usd, Decimal::from(4));
        assert_eq!(trades.trades_by_pair.get("ETH/USDC"), Some(&1));
        assert_eq!(report.health, "GREEN");
    }

    #[test]
    fn includes_metrics_snapshot_summary() {
        let dir = tempdir().unwrap();
        let events_path = dir.path().join("events.jsonl");
        File::create(&events_path).unwrap();
        let metrics_path = dir.path().join("metrics.prom");
        let mut file = File::create(&metrics_path).unwrap();
        writeln!(
            file,
            "# TYPE peanut_flashbots_bundles_submitted_total counter"
        )
        .unwrap();
        writeln!(
            file,
            "peanut_flashbots_bundles_submitted_total{{relay=\"flashbots\"}} 4"
        )
        .unwrap();
        writeln!(
            file,
            "peanut_flashbots_bundles_included_total{{relay=\"flashbots\"}} 3"
        )
        .unwrap();
        writeln!(
            file,
            "peanut_flashbots_bundles_not_included_total{{relay=\"flashbots\"}} 1"
        )
        .unwrap();
        writeln!(
            file,
            "peanut_dex_cancel_outcomes_total{{outcome=\"cancelled\"}} 2"
        )
        .unwrap();

        let report = build_report(
            &events_path,
            NaiveDate::from_ymd_opt(2026, 5, 7).unwrap(),
            None,
            None,
            Some(&metrics_path),
            None,
        )
        .unwrap();
        let metrics = report.metrics_snapshot.as_ref().unwrap();
        assert_eq!(
            metric_total(metrics, "peanut_flashbots_bundles_submitted_total"),
            4.0
        );
        assert_eq!(metrics.flashbots_inclusion_rate, Some(0.75));
        assert_eq!(report.health, "YELLOW");
        assert!(report.risk_score > 0);
        assert!(
            report
                .top_risk_causes
                .iter()
                .any(|cause| cause.message.contains("not-included"))
        );
    }

    #[test]
    fn renders_html_report() {
        let mut report = DailyReport::for_date(NaiveDate::from_ymd_opt(2026, 5, 7).unwrap());
        report.health = "GREEN".into();
        report.total_events = 3;
        report.executions_total = 1;
        report.realized_pnl_usd = Decimal::from(7);
        report.execution_states.insert("Done<&>".into(), 1);

        let html = render_html(&report);
        assert!(html.contains("<!doctype html>"));
        assert!(html.contains("Daily Ops Report - 2026-05-07"));
        assert!(html.contains("Health: GREEN"));
        assert!(html.contains("Done&lt;&amp;&gt;"));
    }

    #[test]
    fn compares_previous_json_report() {
        let dir = tempdir().unwrap();
        let events_path = dir.path().join("events.jsonl");
        let mut events = File::create(&events_path).unwrap();
        writeln!(
            events,
            "{}",
            json!({"ts":"2026-05-07T00:01:00Z","event_type":"execution_terminal","fields":{"state":"Done","pair":"ETH/USDC","actual_net_pnl":"15"}})
        )
        .unwrap();
        let previous_path = dir.path().join("previous.json");
        let mut previous = DailyReport::for_date(NaiveDate::from_ymd_opt(2026, 5, 6).unwrap());
        previous.health = "GREEN".into();
        previous.risk_score = 5;
        previous.total_events = 1;
        previous.executions_total = 1;
        previous.realized_pnl_usd = Decimal::from(10);
        write_report_file(
            &previous_path,
            serde_json::to_string_pretty(&previous).unwrap(),
        )
        .unwrap();

        let report = build_report(
            &events_path,
            NaiveDate::from_ymd_opt(2026, 5, 7).unwrap(),
            None,
            None,
            None,
            Some(&previous_path),
        )
        .unwrap();
        let comparison = report.comparison.as_ref().unwrap();
        assert_eq!(comparison.realized_pnl_usd_delta, Decimal::from(5));
        assert_eq!(comparison.previous_date, previous.date);
        assert_eq!(comparison.executions_delta, 0);
    }

    #[test]
    fn builds_alert_summary_payload() {
        let mut report = DailyReport::for_date(NaiveDate::from_ymd_opt(2026, 5, 7).unwrap());
        report.halted_stops = 1;
        report.finalize();

        let payload = alert_summary(&report, "12345");
        assert_eq!(payload.health, "RED");
        assert!(payload.risk_score >= 40);
        assert_eq!(payload.telegram_payload.chat_id, "12345");
        assert_eq!(payload.telegram_payload.parse_mode, "HTML");
        assert!(payload.telegram_payload.disable_web_page_preview);
        assert!(payload.telegram_payload.text.contains("Daily Ops Report"));
        assert!(payload.telegram_payload.text.contains("Health: <b>RED</b>"));
        assert!(
            payload
                .top_risk_causes
                .iter()
                .any(|cause| cause.message.contains("bot halted"))
        );
    }

    #[test]
    fn telegram_alert_text_is_html_safe() {
        let mut report = DailyReport::for_date(NaiveDate::from_ymd_opt(2026, 5, 7).unwrap());
        report.health = "YELLOW".into();
        report.risk_score = 10;
        report.realized_pnl_usd = Decimal::from(-3);
        report.top_risk_causes.push(RiskCause {
            severity: "yellow".into(),
            score: 10,
            message: "unsafe <router> & pair".into(),
        });

        let payload = telegram_payload(&report, "chat-1");
        let body = serde_json::to_value(&payload).unwrap();
        assert_eq!(body["chat_id"], "chat-1");
        assert_eq!(body["parse_mode"], "HTML");
        assert!(body["text"].as_str().unwrap().contains("&lt;router&gt;"));
        assert!(body["text"].as_str().unwrap().contains("&amp; pair"));
    }

    #[test]
    fn evaluates_fail_on_health_threshold() {
        assert!(!should_fail_on_health("GREEN", None));
        assert!(!should_fail_on_health("GREEN", Some("yellow")));
        assert!(should_fail_on_health("YELLOW", Some("yellow")));
        assert!(should_fail_on_health("RED", Some("yellow")));
        assert!(!should_fail_on_health("YELLOW", Some("red")));
        assert!(should_fail_on_health("RED", Some("red")));
    }
}
