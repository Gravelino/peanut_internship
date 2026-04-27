//! Observability: metrics, alerters, structured logging helpers.
//!
//! Currently exposes [`metrics`] (Prometheus). Future sub-modules: `alerter`
//! (S7b), structured event logs (S8 adjacent).

pub mod alerts;
pub mod metrics;
pub mod server;

pub use alerts::{
    AlertEvent, AlertProvider, AlertRules, AlertSink, LoggingSink, NoopSink, WebhookSink,
    emit_best_effort, evaluate_execution, mask_webhook_url,
};
pub use metrics::{Metrics, init_metrics, metrics_handle};
pub use server::{HaltCoordinator, serve_metrics, serve_metrics_with_halt};
