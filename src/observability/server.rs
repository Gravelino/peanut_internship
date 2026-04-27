//! Minimal `/metrics` HTTP endpoint for Prometheus scraping.
//!
//! Uses `hyper` 1.x directly (no framework) to keep the dependency surface
//! small. Binds to the supplied `SocketAddr` and serves the registry snapshot
//! from [`Metrics::render`](crate::observability::metrics::Metrics::render) on
//! `GET /metrics`. Any other path returns 404.
//!
//! Intended to be spawned once at startup; returns when the listener errors or
//! the process shuts down. All other errors are logged and the per-connection
//! task is dropped — a single bad client never takes down the endpoint.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use http_body_util::Full;
use hyper::body::Bytes;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;
use tracing::{error, info, warn};

use crate::observability::metrics::Metrics;

// ---------------------------------------------------------------------------
// Halt coordinator
// ---------------------------------------------------------------------------

/// Process-wide kill-switch shared between the main loop, the metrics server
/// (HTTP halt endpoint), and the watchdog file checker.
///
/// Three halt paths converge here:
/// 1. **HTTP** — `POST /admin/halt` sets the flag via the metrics server.
/// 2. **Watchdog file** — the main loop calls [`HaltCoordinator::check_watchdog`]
///    each tick; if the file exists the flag is set.
/// 3. **PnL breaker** — the completion consumer calls [`HaltCoordinator::halt`]
///    when the daily loss threshold is exceeded.
///
/// The main loop checks [`HaltCoordinator::is_halted`] at the top of every
/// tick and breaks out cleanly.
#[derive(Debug)]
pub struct HaltCoordinator {
    flag: AtomicBool,
    watchdog_path: Option<PathBuf>,
    reason: std::sync::Mutex<Option<String>>,
}

impl HaltCoordinator {
    /// Creates a coordinator. When `watchdog_path` is `Some`, the
    /// [`Self::check_watchdog`] method will probe the file system.
    pub fn new(watchdog_path: Option<PathBuf>) -> Self {
        Self {
            flag: AtomicBool::new(false),
            watchdog_path,
            reason: std::sync::Mutex::new(None),
        }
    }

    /// Signals an immediate halt with a human-readable reason.
    pub fn halt(&self, reason: impl Into<String>) {
        let was = self.flag.swap(true, Ordering::SeqCst);
        if !was {
            let reason = reason.into();
            warn!(reason = %reason, "HALT SIGNAL RECEIVED");
            if let Ok(mut guard) = self.reason.lock() {
                *guard = Some(reason);
            }
        }
    }

    /// Returns `true` if any halt path has fired.
    pub fn is_halted(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    /// Human-readable reason for the halt, if one was recorded.
    pub fn reason(&self) -> Option<String> {
        self.reason.lock().ok().and_then(|g| g.clone())
    }

    /// Checks whether the watchdog file exists; if so, triggers halt.
    /// No-op when no watchdog path was configured.
    pub fn check_watchdog(&self) {
        if let Some(ref p) = self.watchdog_path
            && Path::new(p).exists()
        {
            self.halt(format!("watchdog file detected: {}", p.display()));
        }
    }
}

impl Default for HaltCoordinator {
    fn default() -> Self {
        Self::new(None)
    }
}

/// Runs the `/metrics` server until the listener errors. Spawn on a dedicated
/// task; callers typically don't `await` this future directly but drop its
/// `JoinHandle` on shutdown.
///
/// This is a convenience wrapper that does **not** expose the halt endpoint.
/// Use [`serve_metrics_with_halt`] when the halt coordinator is available.
pub async fn serve_metrics(addr: SocketAddr, metrics: Arc<Metrics>) -> std::io::Result<()> {
    serve_metrics_with_halt(addr, metrics, None).await
}

/// Like [`serve_metrics`], but additionally serves `POST /admin/halt` and
/// `GET /admin/status` when a [`HaltCoordinator`] is provided.
pub async fn serve_metrics_with_halt(
    addr: SocketAddr,
    metrics: Arc<Metrics>,
    halt: Option<Arc<HaltCoordinator>>,
) -> std::io::Result<()> {
    let listener = TcpListener::bind(addr).await?;
    info!(%addr, "metrics endpoint listening on /metrics");

    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(pair) => pair,
            Err(e) => {
                error!(error = %e, "metrics listener accept failed");
                return Err(e);
            }
        };
        let metrics = metrics.clone();
        let halt = halt.clone();
        let io = TokioIo::new(stream);
        tokio::spawn(async move {
            let svc = service_fn(move |req| handle(req, metrics.clone(), halt.clone()));
            if let Err(e) = hyper::server::conn::http1::Builder::new()
                .serve_connection(io, svc)
                .await
            {
                warn!(peer = %peer, error = %e, "metrics connection error");
            }
        });
    }
}

async fn handle(
    req: Request<hyper::body::Incoming>,
    metrics: Arc<Metrics>,
    halt: Option<Arc<HaltCoordinator>>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    match (req.method(), req.uri().path()) {
        (&Method::GET, "/metrics") => Ok(render_response(&metrics)),
        (&Method::GET, "/health") => Ok(text_response(StatusCode::OK, "ok")),
        (&Method::POST, "/admin/halt") => Ok(handle_halt(halt.as_deref())),
        (&Method::GET, "/admin/status") => Ok(handle_status(halt.as_deref())),
        _ => Ok(text_response(StatusCode::NOT_FOUND, "not found")),
    }
}

fn handle_halt(halt: Option<&HaltCoordinator>) -> Response<Full<Bytes>> {
    match halt {
        Some(h) => {
            h.halt("HTTP /admin/halt");
            text_response(StatusCode::OK, "halted")
        }
        None => text_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "halt coordinator not configured",
        ),
    }
}

fn handle_status(halt: Option<&HaltCoordinator>) -> Response<Full<Bytes>> {
    match halt {
        Some(h) => {
            let halted = h.is_halted();
            let reason = h.reason().unwrap_or_default();
            let body = format!("{{\"halted\":{halted},\"reason\":{reason:?}}}");
            Response::builder()
                .status(StatusCode::OK)
                .header(hyper::header::CONTENT_TYPE, "application/json")
                .body(Full::new(Bytes::from(body)))
                .expect("static response builds")
        }
        None => text_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "halt coordinator not configured",
        ),
    }
}

fn render_response(metrics: &Metrics) -> Response<Full<Bytes>> {
    match metrics.render() {
        Ok(buf) => Response::builder()
            .status(StatusCode::OK)
            .header(
                hyper::header::CONTENT_TYPE,
                "text/plain; version=0.0.4; charset=utf-8",
            )
            .body(Full::new(Bytes::from(buf)))
            .expect("static response builds"),
        Err(e) => {
            error!(error = %e, "metrics encoding failed");
            text_response(StatusCode::INTERNAL_SERVER_ERROR, "encoding error")
        }
    }
}

fn text_response(status: StatusCode, body: &'static str) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header(hyper::header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(Full::new(Bytes::from(body)))
        .expect("static response builds")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::ExecutorState;

    async fn spawn_server() -> (
        SocketAddr,
        tokio::task::JoinHandle<std::io::Result<()>>,
        Arc<Metrics>,
    ) {
        // Port 0 = let the OS pick a free one.
        let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let metrics = Arc::new(Metrics::new());
        metrics.record_execution(ExecutorState::DoneProfit, 0.25, Some(10.0));
        metrics.set_queue_depth(5);

        let m = metrics.clone();
        let handle = tokio::spawn(async move { serve_metrics(addr, m).await });

        // Give the server a tick to bind.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        (addr, handle, metrics)
    }

    async fn http_get(addr: SocketAddr, path: &str) -> (StatusCode, String) {
        let url = format!("http://{addr}{path}");
        let resp = reqwest::get(&url).await.expect("request");
        let status = resp.status();
        let body = resp.text().await.expect("body");
        (StatusCode::from_u16(status.as_u16()).unwrap(), body)
    }

    #[tokio::test]
    async fn metrics_endpoint_returns_200_with_expected_names() {
        let (addr, handle, _m) = spawn_server().await;

        let (status, body) = http_get(addr, "/metrics").await;
        assert_eq!(status, StatusCode::OK);
        for name in [
            "peanut_executions_total",
            "peanut_execution_duration_seconds",
            "peanut_signal_queue_depth",
        ] {
            assert!(body.contains(name), "missing: {name}");
        }

        handle.abort();
    }

    #[tokio::test]
    async fn unknown_path_returns_404() {
        let (addr, handle, _m) = spawn_server().await;
        let (status, _body) = http_get(addr, "/nope").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        handle.abort();
    }

    #[tokio::test]
    async fn health_endpoint_ok() {
        let (addr, handle, _m) = spawn_server().await;
        let (status, body) = http_get(addr, "/health").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "ok");
        handle.abort();
    }

    // ---- HaltCoordinator tests -------------------------------------------

    #[test]
    fn halt_coordinator_starts_unhalted() {
        let hc = HaltCoordinator::default();
        assert!(!hc.is_halted());
        assert!(hc.reason().is_none());
    }

    #[test]
    fn halt_coordinator_halt_sets_flag_and_reason() {
        let hc = HaltCoordinator::default();
        hc.halt("test halt");
        assert!(hc.is_halted());
        assert_eq!(hc.reason().as_deref(), Some("test halt"));
    }

    #[test]
    fn halt_coordinator_idempotent_keeps_first_reason() {
        let hc = HaltCoordinator::default();
        hc.halt("first");
        hc.halt("second");
        assert_eq!(hc.reason().as_deref(), Some("first"));
    }

    #[test]
    fn halt_coordinator_watchdog_no_file_no_halt() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("STOP");
        let hc = HaltCoordinator::new(Some(path));
        hc.check_watchdog();
        assert!(!hc.is_halted());
    }

    #[test]
    fn halt_coordinator_watchdog_file_triggers_halt() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("STOP");
        std::fs::write(&path, "").unwrap();
        let hc = HaltCoordinator::new(Some(path));
        hc.check_watchdog();
        assert!(hc.is_halted());
        assert!(hc.reason().unwrap().contains("watchdog"));
    }

    #[test]
    fn halt_coordinator_no_watchdog_path_check_is_noop() {
        let hc = HaltCoordinator::new(None);
        hc.check_watchdog(); // must not panic
        assert!(!hc.is_halted());
    }

    // ---- /admin/halt + /admin/status HTTP tests --------------------------

    async fn spawn_server_with_halt() -> (
        SocketAddr,
        tokio::task::JoinHandle<std::io::Result<()>>,
        Arc<Metrics>,
        Arc<HaltCoordinator>,
    ) {
        let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let metrics = Arc::new(Metrics::new());
        let halt = Arc::new(HaltCoordinator::default());
        let m = metrics.clone();
        let h = halt.clone();
        let handle = tokio::spawn(async move { serve_metrics_with_halt(addr, m, Some(h)).await });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        (addr, handle, metrics, halt)
    }

    #[tokio::test]
    async fn admin_halt_sets_flag() {
        let (addr, handle, _m, halt) = spawn_server_with_halt().await;
        assert!(!halt.is_halted());

        let url = format!("http://{addr}/admin/halt");
        let resp = reqwest::Client::new()
            .post(&url)
            .send()
            .await
            .expect("request");
        assert_eq!(resp.status().as_u16(), 200);
        assert!(halt.is_halted());

        handle.abort();
    }

    #[tokio::test]
    async fn admin_status_reflects_state() {
        let (addr, handle, _m, halt) = spawn_server_with_halt().await;

        let (status, body) = http_get(addr, "/admin/status").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("\"halted\":false"));

        halt.halt("test");

        let (status, body) = http_get(addr, "/admin/status").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("\"halted\":true"));

        handle.abort();
    }
}
