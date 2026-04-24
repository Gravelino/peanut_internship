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
use std::sync::Arc;

use http_body_util::Full;
use hyper::body::Bytes;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;
use tracing::{error, info, warn};

use crate::observability::metrics::Metrics;

/// Runs the `/metrics` server until the listener errors. Spawn on a dedicated
/// task; callers typically don't `await` this future directly but drop its
/// `JoinHandle` on shutdown.
pub async fn serve_metrics(addr: SocketAddr, metrics: Arc<Metrics>) -> std::io::Result<()> {
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
        let io = TokioIo::new(stream);
        tokio::spawn(async move {
            let svc = service_fn(move |req| handle(req, metrics.clone()));
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
) -> Result<Response<Full<Bytes>>, Infallible> {
    match (req.method(), req.uri().path()) {
        (&Method::GET, "/metrics") => Ok(render_response(&metrics)),
        (&Method::GET, "/health") => Ok(text_response(StatusCode::OK, "ok")),
        _ => Ok(text_response(StatusCode::NOT_FOUND, "not found")),
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
}
