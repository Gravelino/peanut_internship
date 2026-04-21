use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Mutex, Notify, Semaphore};
use tracing::{debug, warn};

use crate::core::types::{DEFAULT_RETRY_AFTER_SECS, HTTP_TIMEOUT_SECS};
use crate::exchange::errors::{ExchangeError, ExchangeResult};
use crate::exchange::rate_limiter::{
    LimitInterval, LimitKey, LimitType, RateLimiter, extract_order_count_from_headers,
    extract_rate_limit_from_headers, extract_retry_after_from_headers,
    extract_used_weight_from_headers,
};

/// Default maximum number of concurrent in-flight HTTP requests.
const DEFAULT_MAX_CONCURRENT: usize = 10;

/// Maximum time to wait for a rate-limit window reset notification before polling.
const WINDOW_RESET_POLL_SECS: u64 = 65;

/// Configuration for automatic retry on transient failures.
#[derive(Debug, Clone)]
pub struct RetryConfig {
    /// Maximum number of retry attempts (0 = no retry).
    pub max_retries: usize,
    /// Base backoff interval in seconds for exponential backoff.
    pub base_backoff_secs: u64,
    /// Maximum backoff interval in seconds (caps exponential growth).
    pub max_backoff_secs: u64,
    /// Whether to automatically retry on HTTP 429 responses.
    pub retry_on_429: bool,
    /// Whether to automatically retry on connection timeouts.
    pub retry_on_timeout: bool,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_retries: 2,
            base_backoff_secs: 1,
            max_backoff_secs: 30,
            retry_on_429: true,
            retry_on_timeout: true,
        }
    }
}

impl RetryConfig {
    /// Creates a config with retries disabled.
    pub fn no_retry() -> Self {
        Self {
            max_retries: 0,
            base_backoff_secs: 1,
            max_backoff_secs: 30,
            retry_on_429: false,
            retry_on_timeout: false,
        }
    }
}

/// Shared HTTP client with rate-limit tracking, automatic retry, and concurrency control.
///
/// Wraps a `reqwest::Client` and pairs it with a [`RateLimiter`] so that
/// every outbound request is automatically throttled and every response
/// header is inspected for quota updates.
///
/// Concurrency is bounded by an internal [`Semaphore`], preventing
/// thundering-herd spikes when the rate budget resets. Tasks that
/// cannot acquire weight budget wait on a [`Notify`] signal rather
/// than sleeping blindly, reducing wasted wake-ups.
pub struct HttpClient {
    inner: reqwest::Client,
    rate_limiter: Arc<Mutex<RateLimiter>>,
    retry: RetryConfig,
    enabled: bool,
    concurrency: Arc<Semaphore>,
    window_reset: Notify,
}

impl std::fmt::Debug for HttpClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpClient")
            .field("enabled", &self.enabled)
            .field("retry", &self.retry)
            .field("concurrency", &self.concurrency.available_permits())
            .finish_non_exhaustive()
    }
}

impl HttpClient {
    /// Creates a new `HttpClient` with the default timeout, concurrency limit, and a fresh rate limiter.
    pub fn new(retry: RetryConfig, enabled: bool) -> ExchangeResult<Self> {
        let inner = reqwest::Client::builder()
            .timeout(Duration::from_secs(HTTP_TIMEOUT_SECS))
            .build()
            .map_err(ExchangeError::Http)?;

        Ok(Self {
            inner,
            rate_limiter: Arc::new(Mutex::new(RateLimiter::default_limiter())),
            retry,
            enabled,
            concurrency: Arc::new(Semaphore::new(DEFAULT_MAX_CONCURRENT)),
            window_reset: Notify::new(),
        })
    }

    /// Creates a client with a pre-existing rate limiter (e.g. per-source).
    pub fn with_limiter(
        limiter: Arc<Mutex<RateLimiter>>,
        retry: RetryConfig,
        enabled: bool,
    ) -> ExchangeResult<Self> {
        let inner = reqwest::Client::builder()
            .timeout(Duration::from_secs(HTTP_TIMEOUT_SECS))
            .build()
            .map_err(ExchangeError::Http)?;

        Ok(Self {
            inner,
            rate_limiter: limiter,
            retry,
            enabled,
            concurrency: Arc::new(Semaphore::new(DEFAULT_MAX_CONCURRENT)),
            window_reset: Notify::new(),
        })
    }

    /// Returns a reference to the underlying `reqwest::Client` for manual requests.
    pub fn inner(&self) -> &reqwest::Client {
        &self.inner
    }

    /// Returns a reference to the retry configuration.
    pub fn retry_config(&self) -> &RetryConfig {
        &self.retry
    }

    /// Returns a reference to the shared rate limiter.
    pub fn rate_limiter(&self) -> &Arc<Mutex<RateLimiter>> {
        &self.rate_limiter
    }

    /// Returns a reference to the concurrency semaphore.
    pub fn concurrency(&self) -> &Arc<Semaphore> {
        &self.concurrency
    }

    /// Sends a GET request with rate-limit tracking and optional retry.
    ///
    /// `weight` specifies how many rate-limit units this request consumes.
    /// On success, returns the raw `reqwest::Response` so the caller can
    /// choose how to deserialize the body.
    pub async fn get(
        &self,
        url: &str,
        api_key: Option<&str>,
        weight: u32,
    ) -> ExchangeResult<reqwest::Response> {
        let mut builder = self.inner.get(url);
        if let Some(key) = api_key {
            builder = builder.header("X-MBX-APIKEY", key);
        }
        self.send_tracked(builder, weight).await
    }

    /// Sends a POST request with rate-limit tracking and optional retry.
    ///
    /// `weight` specifies how many rate-limit units this request consumes.
    pub async fn post(
        &self,
        url: &str,
        api_key: Option<&str>,
        weight: u32,
    ) -> ExchangeResult<reqwest::Response> {
        let mut builder = self.inner.post(url);
        if let Some(key) = api_key {
            builder = builder.header("X-MBX-APIKEY", key);
        }
        self.send_tracked(builder, weight).await
    }

    /// Sends a DELETE request with rate-limit tracking and optional retry.
    ///
    /// `weight` specifies how many rate-limit units this request consumes.
    pub async fn delete(
        &self,
        url: &str,
        api_key: Option<&str>,
        weight: u32,
    ) -> ExchangeResult<reqwest::Response> {
        let mut builder = self.inner.delete(url);
        if let Some(key) = api_key {
            builder = builder.header("X-MBX-APIKEY", key);
        }
        self.send_tracked(builder, weight).await
    }

    /// Core send pipeline: concurrency gate → rate-limit check → send → header discovery → 429/retry handling.
    ///
    /// The `weight` parameter specifies how many rate-limit units this request consumes.
    /// A concurrency permit is acquired before sending and released after the response
    /// is consumed by the caller (the permit is held implicitly while the future is live).
    pub async fn send_tracked(
        &self,
        request: reqwest::RequestBuilder,
        weight: u32,
    ) -> ExchangeResult<reqwest::Response> {
        let _permit = self
            .concurrency
            .acquire()
            .await
            .map_err(|_| ExchangeError::Network("concurrency semaphore closed".into()))?;

        let mut attempt = 0usize;

        loop {
            attempt += 1;

            if self.enabled {
                self.check_rate_limit(weight).await;
            }

            let cloned = request
                .try_clone()
                .ok_or_else(|| ExchangeError::Network("non-retryable request body".into()))?;

            let result = cloned.send().await;

            match result {
                Ok(response) => {
                    if self.enabled {
                        let headers = response.headers();

                        if let Some(limit) = extract_rate_limit_from_headers(headers) {
                            let mut guard = self.rate_limiter.lock().await;
                            guard.update_quota_from_limit(limit);
                        }

                        if let Some(used) = extract_used_weight_from_headers(headers) {
                            let mut guard = self.rate_limiter.lock().await;
                            guard.update_used_weight(used);
                        }

                        {
                            let mut guard = self.rate_limiter.lock().await;
                            for interval in [
                                LimitInterval::Second,
                                LimitInterval::Minute,
                                LimitInterval::Day,
                            ] {
                                if let Some(count) =
                                    extract_order_count_from_headers(headers, interval)
                                {
                                    let key = LimitKey::new(LimitType::Orders, interval);
                                    guard.update_used_weight_for(key, count);
                                }
                            }
                        }

                        if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
                            let retry_after = extract_retry_after_from_headers(headers);
                            warn!(retry_after, attempt, "429 received");

                            if self.retry.retry_on_429 && attempt <= self.retry.max_retries {
                                {
                                    let mut guard = self.rate_limiter.lock().await;
                                    guard.reset_after_429();
                                }
                                self.window_reset.notify_waiters();
                                exponential_backoff(attempt, retry_after, &self.retry).await;
                                continue;
                            }

                            {
                                let mut guard = self.rate_limiter.lock().await;
                                guard.reset_after_429();
                            }
                            self.window_reset.notify_waiters();

                            return Err(ExchangeError::RateLimit(format!(
                                "429 Too Many Requests, retry after {retry_after}s (attempt {attempt})"
                            )));
                        }
                    }

                    return Ok(response);
                }
                Err(err) => {
                    let is_retryable =
                        (err.is_timeout() && self.retry.retry_on_timeout) || err.is_connect();

                    if is_retryable && attempt <= self.retry.max_retries {
                        debug!(
                            attempt,
                            error = %err,
                            "Retryable error, backing off"
                        );
                        exponential_backoff(attempt, DEFAULT_RETRY_AFTER_SECS, &self.retry).await;
                        continue;
                    }

                    return Err(ExchangeError::Http(err));
                }
            }
        }
    }

    /// Checks rate-limit budget and waits for a window-reset notification if the budget is low.
    ///
    /// Uses [`Notify`] to wait efficiently instead of sleeping for a fixed duration.
    /// A safety timeout ensures we re-poll even if no notification arrives.
    async fn check_rate_limit(&self, weight: u32) {
        loop {
            {
                let mut guard = self.rate_limiter.lock().await;
                if guard.try_acquire_weighted(weight) {
                    return;
                }
            }

            warn!("Rate limit budget low, waiting for window reset notification");

            tokio::select! {
                _ = self.window_reset.notified() => {}
                _ = tokio::time::sleep(Duration::from_secs(WINDOW_RESET_POLL_SECS)) => {}
            }
        }
    }
}

async fn exponential_backoff(attempt: usize, base_wait: u64, cfg: &RetryConfig) {
    let wait = compute_backoff_secs(attempt, base_wait, cfg.max_backoff_secs);
    debug!(attempt, wait, "Exponential backoff");
    tokio::time::sleep(Duration::from_secs(wait)).await;
}

/// Computes the backoff duration for a given attempt (without sleeping).
/// Exposed for testing: `base_wait * 2^(attempt-1)`, capped at `max_backoff_secs`.
pub fn compute_backoff_secs(attempt: usize, base_wait: u64, max_backoff_secs: u64) -> u64 {
    let exp = 2u64.saturating_pow((attempt - 1) as u32);
    base_wait.saturating_mul(exp).min(max_backoff_secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_retry_config_default() {
        let cfg = RetryConfig::default();
        assert_eq!(cfg.max_retries, 2);
        assert_eq!(cfg.base_backoff_secs, 1);
        assert_eq!(cfg.max_backoff_secs, 30);
        assert!(cfg.retry_on_429);
        assert!(cfg.retry_on_timeout);
    }

    #[test]
    fn test_retry_config_no_retry() {
        let cfg = RetryConfig::no_retry();
        assert_eq!(cfg.max_retries, 0);
        assert!(!cfg.retry_on_429);
        assert!(!cfg.retry_on_timeout);
    }

    #[test]
    fn test_http_client_new_enabled() {
        let client = HttpClient::new(RetryConfig::default(), true).unwrap();
        assert!(client.enabled);
        assert_eq!(client.retry_config().max_retries, 2);
    }

    #[test]
    fn test_http_client_new_disabled() {
        let client = HttpClient::new(RetryConfig::no_retry(), false).unwrap();
        assert!(!client.enabled);
        assert_eq!(client.retry_config().max_retries, 0);
    }

    #[test]
    fn test_http_client_with_limiter_shares_limiter() {
        let limiter = Arc::new(Mutex::new(RateLimiter::default_limiter()));
        let client1 =
            HttpClient::with_limiter(limiter.clone(), RetryConfig::default(), true).unwrap();
        let client2 = HttpClient::with_limiter(limiter, RetryConfig::default(), true).unwrap();

        assert!(Arc::ptr_eq(client1.rate_limiter(), client2.rate_limiter()));
    }

    #[test]
    fn test_http_client_inner_accessible() {
        let client = HttpClient::new(RetryConfig::default(), true).unwrap();
        let _ = client.inner();
    }

    #[test]
    fn test_http_client_concurrency_semaphore() {
        let client = HttpClient::new(RetryConfig::default(), true).unwrap();
        assert_eq!(
            client.concurrency().available_permits(),
            DEFAULT_MAX_CONCURRENT
        );
    }

    #[test]
    fn test_compute_backoff_first_attempt() {
        assert_eq!(compute_backoff_secs(1, 1, 30), 1);
    }

    #[test]
    fn test_compute_backoff_second_attempt() {
        assert_eq!(compute_backoff_secs(2, 1, 30), 2);
    }

    #[test]
    fn test_compute_backoff_third_attempt() {
        assert_eq!(compute_backoff_secs(3, 1, 30), 4);
    }

    #[test]
    fn test_compute_backoff_capped_by_max() {
        assert_eq!(compute_backoff_secs(10, 1, 30), 30);
    }

    #[test]
    fn test_compute_backoff_large_base_wait() {
        assert_eq!(compute_backoff_secs(1, 100, 30), 30);
    }

    #[test]
    fn test_compute_backoff_zero_base() {
        assert_eq!(compute_backoff_secs(5, 0, 30), 0);
    }

    #[tokio::test]
    async fn test_check_rate_limit_under_budget() {
        let client = HttpClient::new(RetryConfig::no_retry(), true).unwrap();
        client.check_rate_limit(1).await;
        let guard = client.rate_limiter().lock().await;
        assert_eq!(guard.used_weight(), 1);
    }

    #[tokio::test]
    async fn test_check_rate_limit_weighted() {
        let client = HttpClient::new(RetryConfig::no_retry(), true).unwrap();
        client.check_rate_limit(10).await;
        let guard = client.rate_limiter().lock().await;
        assert_eq!(guard.used_weight(), 10);
    }

    #[test]
    fn test_exchange_client_uses_http_client() {
        use crate::exchange::config::BinanceConfig;
        let config = BinanceConfig::with_custom_url(
            "key".into(),
            "secret".into(),
            "https://testnet.binance.vision".into(),
        );
        let client = crate::exchange::client::ExchangeClient::new(config).unwrap();
        assert_eq!(client.config().api_key, "key");
    }
}
