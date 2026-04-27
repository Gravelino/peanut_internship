use std::collections::HashMap;
use std::time::Instant;

use crate::core::types::DEFAULT_RETRY_AFTER_SECS;
use tracing::{debug, info, warn};

const SAFETY_MARGIN_RATIO: f64 = 0.9;
const DEFAULT_MAX_WEIGHT: u32 = 60;
const DEFAULT_WINDOW_SECS: u64 = 60;

/// Binance rate-limit type, matching the `rateLimitType` field in exchangeInfo.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LimitType {
    /// Per-IP request weight budget (e.g. 1200 weight / minute).
    RequestWeight,
    /// Per-account order budget (e.g. 10 orders / second).
    Orders,
    /// Per-IP raw request count (e.g. 6100 raw requests / 5 minutes).
    RawRequests,
}

impl LimitType {
    /// Parses a Binance `rateLimitType` string.
    pub fn from_binance_str(s: &str) -> Option<Self> {
        match s {
            "REQUEST_WEIGHT" => Some(Self::RequestWeight),
            "ORDERS" => Some(Self::Orders),
            "RAW_REQUESTS" => Some(Self::RawRequests),
            _ => None,
        }
    }
}

/// Time interval for a rate-limit window, matching Binance `interval` field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LimitInterval {
    /// 1-second window.
    Second,
    /// 1-minute window.
    Minute,
    /// 5-minute window.
    FiveMinute,
    /// 1-day window.
    Day,
}

impl LimitInterval {
    /// Parses a Binance `interval` string.
    pub fn from_binance_str(s: &str) -> Option<Self> {
        match s {
            "SECOND" => Some(Self::Second),
            "MINUTE" => Some(Self::Minute),
            "5MINUTE" => Some(Self::FiveMinute),
            "DAY" => Some(Self::Day),
            _ => None,
        }
    }

    /// Returns the interval duration in seconds.
    pub fn as_secs(self) -> u64 {
        match self {
            Self::Second => 1,
            Self::Minute => 60,
            Self::FiveMinute => 300,
            Self::Day => 86_400,
        }
    }
}

/// Composite key identifying a rate-limit bucket by type and interval.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LimitKey {
    /// The rate-limit type (weight, orders, raw requests).
    pub limit_type: LimitType,
    /// The time window interval.
    pub interval: LimitInterval,
}

impl LimitKey {
    /// Creates a new key from its components.
    pub fn new(limit_type: LimitType, interval: LimitInterval) -> Self {
        Self {
            limit_type,
            interval,
        }
    }

    /// Default key for request-weight tracking (REQUEST_WEIGHT / MINUTE).
    pub fn request_weight() -> Self {
        Self::new(LimitType::RequestWeight, LimitInterval::Minute)
    }

    /// Key for per-minute order tracking (ORDERS / MINUTE).
    pub fn orders_per_minute() -> Self {
        Self::new(LimitType::Orders, LimitInterval::Minute)
    }

    /// Key for per-second order tracking (ORDERS / SECOND).
    pub fn orders_per_second() -> Self {
        Self::new(LimitType::Orders, LimitInterval::Second)
    }

    /// Key for per-day order tracking (ORDERS / DAY).
    pub fn orders_per_day() -> Self {
        Self::new(LimitType::Orders, LimitInterval::Day)
    }

    /// Key for raw-requests tracking (RAW_REQUESTS / 5MINUTE).
    pub fn raw_requests() -> Self {
        Self::new(LimitType::RawRequests, LimitInterval::FiveMinute)
    }
}

impl Default for LimitKey {
    fn default() -> Self {
        Self::request_weight()
    }
}

/// Rate-limit quota describing the maximum request weight allowed per time window.
#[derive(Debug, Clone)]
pub struct ApiQuota {
    /// Maximum request weight allowed within the window.
    pub max_weight: u32,
    /// Length of the rate-limit window in seconds.
    pub window_secs: u64,
}

impl ApiQuota {
    /// Creates a quota with default values (60 weight / 60 s window).
    pub fn default_quota() -> Self {
        Self {
            max_weight: DEFAULT_MAX_WEIGHT,
            window_secs: DEFAULT_WINDOW_SECS,
        }
    }

    /// Creates a quota with a custom window duration and the default max weight.
    pub fn with_window(window_secs: u64) -> Self {
        Self {
            max_weight: DEFAULT_MAX_WEIGHT,
            window_secs,
        }
    }
}

/// Single token bucket tracking used weight against an [`ApiQuota`].
#[derive(Debug)]
struct TokenBucket {
    used_weight: u32,
    window_start: Instant,
    quota: ApiQuota,
    discovered: bool,
}

impl TokenBucket {
    fn new(quota: ApiQuota) -> Self {
        Self {
            used_weight: 0,
            window_start: Instant::now(),
            quota,
            discovered: false,
        }
    }

    fn reset_if_expired(&mut self) {
        if self.window_start.elapsed().as_secs() >= self.quota.window_secs {
            self.used_weight = 0;
            self.window_start = Instant::now();
        }
    }

    fn try_acquire_weighted(&mut self, weight: u32) -> bool {
        self.reset_if_expired();
        let threshold = (self.quota.max_weight as f64 * SAFETY_MARGIN_RATIO) as u32;
        if self.used_weight + weight <= threshold {
            self.used_weight += weight;
            true
        } else {
            false
        }
    }

    fn secs_until_reset(&self) -> u64 {
        self.quota
            .window_secs
            .saturating_sub(self.window_start.elapsed().as_secs())
    }

    fn update_used_weight(&mut self, weight: u32) {
        self.reset_if_expired();
        self.used_weight = weight;
        debug!(
            weight,
            max = self.quota.max_weight,
            discovered = self.discovered,
            "Rate limit weight updated from headers"
        );
    }

    fn update_quota_from_limit(&mut self, limit: u32) {
        if limit == 0 {
            return;
        }
        if !self.discovered || limit != self.quota.max_weight {
            info!(
                old_max = self.quota.max_weight,
                new_max = limit,
                "Rate limit quota discovered from headers"
            );
            self.quota.max_weight = limit;
            self.discovered = true;
        }
    }

    fn update_quota(&mut self, max_weight: u32, window_secs: u64) {
        if max_weight > 0 {
            self.quota.max_weight = max_weight;
            self.discovered = true;
        }
        if window_secs > 0 {
            self.quota.window_secs = window_secs;
        }
        info!(
            max_weight = self.quota.max_weight,
            window_secs = self.quota.window_secs,
            "Rate limit quota updated"
        );
    }

    fn reset_after_429(&mut self) {
        self.used_weight = 0;
        self.window_start = Instant::now();
    }

    fn used_weight(&self) -> u32 {
        self.used_weight
    }

    fn max_weight(&self) -> u32 {
        self.quota.max_weight
    }

    fn quota(&self) -> &ApiQuota {
        &self.quota
    }

    fn is_discovered(&self) -> bool {
        self.discovered
    }
}

/// Multi-bucket rate limiter that tracks used weight across independent [`LimitKey`] buckets.
///
/// Each bucket (identified by a [`LimitKey`]) tracks its own window and weight
/// independently. The default bucket is [`LimitKey::request_weight`] and is
/// used by the backward-compatible single-bucket methods.
#[derive(Debug)]
pub struct RateLimiter {
    buckets: HashMap<LimitKey, TokenBucket>,
}

impl RateLimiter {
    /// Creates a new limiter with a single default REQUEST_WEIGHT / MINUTE bucket.
    pub fn new(quota: ApiQuota) -> Self {
        let mut buckets = HashMap::new();
        buckets.insert(LimitKey::request_weight(), TokenBucket::new(quota));
        Self { buckets }
    }

    /// Creates a limiter with the default quota in the default bucket.
    pub fn default_limiter() -> Self {
        Self::new(ApiQuota::default_quota())
    }

    /// Ensures a bucket exists for the given key, creating one with default quota if needed.
    fn ensure_bucket(&mut self, key: LimitKey) {
        self.buckets.entry(key).or_insert_with(|| {
            let quota = ApiQuota {
                max_weight: DEFAULT_MAX_WEIGHT,
                window_secs: key.interval.as_secs(),
            };
            TokenBucket::new(quota)
        });
    }

    /// Returns a mutable reference to the bucket for `key`, creating it if absent.
    fn bucket_mut(&mut self, key: LimitKey) -> &mut TokenBucket {
        self.ensure_bucket(key);
        self.buckets.get_mut(&key).expect("just ensured")
    }

    /// Returns a reference to the bucket for `key`, if it exists.
    fn bucket_ref(&self, key: &LimitKey) -> Option<&TokenBucket> {
        self.buckets.get(key)
    }

    // --- Backward-compatible methods (default REQUEST_WEIGHT bucket) ---

    /// Attempts to acquire one unit of weight in the default bucket.
    pub fn try_acquire(&mut self) -> bool {
        self.try_acquire_weighted(1)
    }

    /// Attempts to acquire `weight` units in the default REQUEST_WEIGHT bucket.
    pub fn try_acquire_weighted(&mut self, weight: u32) -> bool {
        self.try_acquire_for(LimitKey::request_weight(), weight)
    }

    /// Returns seconds until the default bucket's window resets.
    pub fn secs_until_reset(&self) -> u64 {
        self.secs_until_reset_for(&LimitKey::request_weight())
    }

    /// Updates the used weight counter in the default bucket.
    pub fn update_used_weight(&mut self, weight: u32) {
        self.update_used_weight_for(LimitKey::request_weight(), weight)
    }

    /// Updates `max_weight` from a header value in the default bucket.
    pub fn update_quota_from_limit(&mut self, limit: u32) {
        self.update_quota_from_limit_for(LimitKey::request_weight(), limit)
    }

    /// Updates both `max_weight` and `window_secs` of the default bucket.
    pub fn update_quota(&mut self, max_weight: u32, window_secs: u64) {
        self.update_quota_for(LimitKey::request_weight(), max_weight, window_secs)
    }

    /// Resets all buckets after a 429 response.
    pub fn reset_after_429(&mut self) {
        warn!("Rate limiter reset after 429 response");
        for bucket in self.buckets.values_mut() {
            bucket.reset_after_429();
        }
    }

    /// Returns the used weight in the default bucket.
    pub fn used_weight(&self) -> u32 {
        self.used_weight_for(&LimitKey::request_weight())
    }

    /// Returns the maximum weight of the default bucket.
    pub fn max_weight(&self) -> u32 {
        self.max_weight_for(&LimitKey::request_weight())
    }

    /// Returns a reference to the default bucket's quota.
    pub fn quota(&self) -> &ApiQuota {
        self.quota_for(&LimitKey::request_weight())
    }

    /// Returns whether the default bucket's quota has been discovered.
    pub fn is_discovered(&self) -> bool {
        self.is_discovered_for(&LimitKey::request_weight())
    }

    // --- Per-bucket methods ---

    /// Attempts to acquire `weight` units in the bucket identified by `key`.
    pub fn try_acquire_for(&mut self, key: LimitKey, weight: u32) -> bool {
        self.bucket_mut(key).try_acquire_weighted(weight)
    }

    /// Returns seconds until the bucket for `key` resets.
    pub fn secs_until_reset_for(&self, key: &LimitKey) -> u64 {
        self.bucket_ref(key)
            .map(|b| b.secs_until_reset())
            .unwrap_or(0)
    }

    /// Updates the used weight counter in the bucket for `key`.
    pub fn update_used_weight_for(&mut self, key: LimitKey, weight: u32) {
        self.bucket_mut(key).update_used_weight(weight)
    }

    /// Updates `max_weight` from a header value in the bucket for `key`.
    pub fn update_quota_from_limit_for(&mut self, key: LimitKey, limit: u32) {
        self.bucket_mut(key).update_quota_from_limit(limit)
    }

    /// Updates both `max_weight` and `window_secs` of the bucket for `key`.
    pub fn update_quota_for(&mut self, key: LimitKey, max_weight: u32, window_secs: u64) {
        self.bucket_mut(key).update_quota(max_weight, window_secs)
    }

    /// Returns the used weight in the bucket for `key`.
    pub fn used_weight_for(&self, key: &LimitKey) -> u32 {
        self.bucket_ref(key).map(|b| b.used_weight()).unwrap_or(0)
    }

    /// Returns the maximum weight of the bucket for `key`.
    pub fn max_weight_for(&self, key: &LimitKey) -> u32 {
        self.bucket_ref(key).map(|b| b.max_weight()).unwrap_or(0)
    }

    /// Returns a reference to the quota for the bucket at `key`.
    ///
    /// Panics if the bucket does not exist (call [`ensure_bucket`] first).
    pub fn quota_for(&self, key: &LimitKey) -> &ApiQuota {
        self.bucket_ref(key)
            .expect("bucket must exist; call ensure_bucket first")
            .quota()
    }

    /// Returns whether the quota for `key` has been discovered from headers.
    pub fn is_discovered_for(&self, key: &LimitKey) -> bool {
        self.bucket_ref(key)
            .map(|b| b.is_discovered())
            .unwrap_or(false)
    }

    /// Registers a bucket for `key` with the given max weight, deriving window from the interval.
    pub fn register_bucket(&mut self, key: LimitKey, max_weight: u32) {
        let quota = ApiQuota {
            max_weight,
            window_secs: key.interval.as_secs(),
        };
        self.buckets.insert(key, TokenBucket::new(quota));
        info!(
            ?key,
            max_weight,
            window_secs = key.interval.as_secs(),
            "Registered rate-limit bucket"
        );
    }

    /// Returns the number of registered buckets.
    pub fn bucket_count(&self) -> usize {
        self.buckets.len()
    }

    /// Returns whether a bucket for `key` exists.
    pub fn has_bucket(&self, key: &LimitKey) -> bool {
        self.buckets.contains_key(key)
    }
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self::default_limiter()
    }
}

/// Extracts the rate-limit maximum from standard rate-limit headers.
pub fn extract_rate_limit_from_headers(headers: &reqwest::header::HeaderMap) -> Option<u32> {
    for key in ["x-ratelimit-limit", "x-rate-limit-limit", "ratelimit-limit"] {
        if let Some(val) = headers.get(key).and_then(|v| v.to_str().ok()) {
            let parsed = val
                .split(';')
                .next()
                .unwrap_or(val)
                .trim()
                .parse::<u32>()
                .ok();
            if parsed.is_some() {
                return parsed;
            }
        }
    }
    None
}

/// Extracts the used weight from remaining-weight headers or Binance-specific headers.
pub fn extract_used_weight_from_headers(headers: &reqwest::header::HeaderMap) -> Option<u32> {
    for key in [
        "x-ratelimit-remaining",
        "x-rate-limit-remaining",
        "ratelimit-remaining",
    ] {
        if let Some(val) = headers.get(key).and_then(|v| v.to_str().ok()) {
            let remaining = val
                .split(';')
                .next()
                .unwrap_or(val)
                .trim()
                .parse::<u32>()
                .ok();
            if let Some(r) = remaining
                && let Some(limit) = extract_rate_limit_from_headers(headers)
            {
                return Some(limit.saturating_sub(r));
            }
        }
    }

    if let Some(value) = extract_used_weight_for_interval(headers, LimitInterval::Minute) {
        return Some(value);
    }

    None
}

/// Extracts Binance request-weight usage for a specific interval.
pub fn extract_used_weight_for_interval(
    headers: &reqwest::header::HeaderMap,
    interval: LimitInterval,
) -> Option<u32> {
    let header_name = match interval {
        LimitInterval::Second => "x-mbx-used-weight-1s",
        LimitInterval::Minute => "x-mbx-used-weight-1m",
        LimitInterval::FiveMinute => "x-mbx-used-weight-5m",
        LimitInterval::Day => "x-mbx-used-weight-1d",
    };
    headers
        .get(header_name)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u32>().ok())
}

/// Extracts the Binance order count for a specific interval from response headers.
pub fn extract_order_count_from_headers(
    headers: &reqwest::header::HeaderMap,
    interval: LimitInterval,
) -> Option<u32> {
    let header_name = match interval {
        LimitInterval::Second => "x-mbx-order-count-1s",
        LimitInterval::Minute => "x-mbx-order-count-1m",
        LimitInterval::Day => "x-mbx-order-count-1d",
        LimitInterval::FiveMinute => return None,
    };
    headers
        .get(header_name)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u32>().ok())
}

/// Extracts the `retry-after` value from headers, falling back to a default.
pub fn extract_retry_after_from_headers(headers: &reqwest::header::HeaderMap) -> u64 {
    headers
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(DEFAULT_RETRY_AFTER_SECS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_try_acquire_within_limit() {
        let mut limiter = RateLimiter::new(ApiQuota::default_quota());
        assert!(limiter.try_acquire());
        assert!(limiter.try_acquire());
    }

    #[test]
    fn test_try_acquire_blocks_when_exhausted() {
        let mut limiter = RateLimiter::new(ApiQuota::default_quota());
        for _ in 0..54 {
            assert!(limiter.try_acquire());
        }
        assert!(!limiter.try_acquire());
    }

    #[test]
    fn test_used_weight_tracking() {
        let mut limiter = RateLimiter::new(ApiQuota::default_quota());
        limiter.try_acquire();
        assert_eq!(limiter.used_weight(), 1);
        limiter.try_acquire();
        assert_eq!(limiter.used_weight(), 2);
    }

    #[test]
    fn test_update_used_weight_from_headers() {
        let mut limiter = RateLimiter::new(ApiQuota::default_quota());
        limiter.try_acquire();
        assert_eq!(limiter.used_weight(), 1);
        limiter.update_used_weight(50);
        assert_eq!(limiter.used_weight(), 50);
    }

    #[test]
    fn test_reset_after_429() {
        let mut limiter = RateLimiter::new(ApiQuota::default_quota());
        for _ in 0..53 {
            limiter.try_acquire();
        }
        assert_eq!(limiter.used_weight(), 53);
        limiter.reset_after_429();
        assert_eq!(limiter.used_weight(), 0);
        assert!(limiter.try_acquire());
    }

    #[test]
    fn test_discover_quota_from_limit() {
        let mut limiter = RateLimiter::new(ApiQuota::default_quota());
        assert!(!limiter.is_discovered());
        assert_eq!(limiter.max_weight(), DEFAULT_MAX_WEIGHT);

        limiter.update_quota_from_limit(1200);
        assert!(limiter.is_discovered());
        assert_eq!(limiter.max_weight(), 1200);
    }

    #[test]
    fn test_discover_quota_ignores_zero() {
        let mut limiter = RateLimiter::new(ApiQuota::default_quota());
        limiter.update_quota_from_limit(0);
        assert!(!limiter.is_discovered());
        assert_eq!(limiter.max_weight(), DEFAULT_MAX_WEIGHT);
    }

    #[test]
    fn test_update_quota_full() {
        let mut limiter = RateLimiter::new(ApiQuota::default_quota());
        limiter.update_quota(500, 30);
        assert!(limiter.is_discovered());
        assert_eq!(limiter.max_weight(), 500);
        assert_eq!(limiter.quota().window_secs, 30);
    }

    #[test]
    fn test_default_quota_values() {
        let quota = ApiQuota::default_quota();
        assert_eq!(quota.max_weight, 60);
        assert_eq!(quota.window_secs, 60);
    }

    #[test]
    fn test_extract_rate_limit_from_headers() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("x-ratelimit-limit", "100".parse().unwrap());
        assert_eq!(extract_rate_limit_from_headers(&headers), Some(100));
    }

    #[test]
    fn test_extract_rate_limit_from_headers_ratelimit() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("ratelimit-limit", "200".parse().unwrap());
        assert_eq!(extract_rate_limit_from_headers(&headers), Some(200));
    }

    #[test]
    fn test_extract_rate_limit_from_headers_with_extras() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("x-ratelimit-limit", "100; window=60".parse().unwrap());
        assert_eq!(extract_rate_limit_from_headers(&headers), Some(100));
    }

    #[test]
    fn test_extract_used_weight_from_headers_remaining() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("x-ratelimit-limit", "100".parse().unwrap());
        headers.insert("x-ratelimit-remaining", "80".parse().unwrap());
        assert_eq!(extract_used_weight_from_headers(&headers), Some(20));
    }

    #[test]
    fn test_extract_used_weight_from_headers_mbx() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("x-mbx-used-weight-1m", "50".parse().unwrap());
        assert_eq!(extract_used_weight_from_headers(&headers), Some(50));
    }

    #[test]
    fn test_extract_used_weight_for_interval_all_supported() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("x-mbx-used-weight-1s", "7".parse().unwrap());
        headers.insert("x-mbx-used-weight-1m", "70".parse().unwrap());
        headers.insert("x-mbx-used-weight-5m", "300".parse().unwrap());
        headers.insert("x-mbx-used-weight-1d", "5000".parse().unwrap());

        assert_eq!(
            extract_used_weight_for_interval(&headers, LimitInterval::Second),
            Some(7)
        );
        assert_eq!(
            extract_used_weight_for_interval(&headers, LimitInterval::Minute),
            Some(70)
        );
        assert_eq!(
            extract_used_weight_for_interval(&headers, LimitInterval::FiveMinute),
            Some(300)
        );
        assert_eq!(
            extract_used_weight_for_interval(&headers, LimitInterval::Day),
            Some(5000)
        );
    }

    #[test]
    fn test_extract_retry_after() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("retry-after", "30".parse().unwrap());
        assert_eq!(extract_retry_after_from_headers(&headers), 30);
    }

    #[test]
    fn test_extract_retry_after_default() {
        let headers = reqwest::header::HeaderMap::new();
        assert_eq!(extract_retry_after_from_headers(&headers), 10);
    }

    #[test]
    fn test_limit_type_from_binance_str() {
        assert_eq!(
            LimitType::from_binance_str("REQUEST_WEIGHT"),
            Some(LimitType::RequestWeight)
        );
        assert_eq!(
            LimitType::from_binance_str("ORDERS"),
            Some(LimitType::Orders)
        );
        assert_eq!(
            LimitType::from_binance_str("RAW_REQUESTS"),
            Some(LimitType::RawRequests)
        );
        assert_eq!(LimitType::from_binance_str("UNKNOWN"), None);
    }

    #[test]
    fn test_limit_interval_from_binance_str() {
        assert_eq!(
            LimitInterval::from_binance_str("SECOND"),
            Some(LimitInterval::Second)
        );
        assert_eq!(
            LimitInterval::from_binance_str("MINUTE"),
            Some(LimitInterval::Minute)
        );
        assert_eq!(
            LimitInterval::from_binance_str("5MINUTE"),
            Some(LimitInterval::FiveMinute)
        );
        assert_eq!(
            LimitInterval::from_binance_str("DAY"),
            Some(LimitInterval::Day)
        );
        assert_eq!(LimitInterval::from_binance_str("HOUR"), None);
    }

    #[test]
    fn test_limit_interval_as_secs() {
        assert_eq!(LimitInterval::Second.as_secs(), 1);
        assert_eq!(LimitInterval::Minute.as_secs(), 60);
        assert_eq!(LimitInterval::FiveMinute.as_secs(), 300);
        assert_eq!(LimitInterval::Day.as_secs(), 86_400);
    }

    #[test]
    fn test_multi_bucket_register_and_acquire() {
        let mut limiter = RateLimiter::default_limiter();
        limiter.register_bucket(LimitKey::orders_per_second(), 10);
        limiter.register_bucket(LimitKey::orders_per_day(), 100_000);

        assert!(limiter.has_bucket(&LimitKey::request_weight()));
        assert!(limiter.has_bucket(&LimitKey::orders_per_second()));
        assert!(limiter.has_bucket(&LimitKey::orders_per_day()));
        assert_eq!(limiter.bucket_count(), 3);

        assert!(limiter.try_acquire_for(LimitKey::request_weight(), 1));
        assert!(limiter.try_acquire_for(LimitKey::orders_per_second(), 1));
        assert_eq!(limiter.used_weight_for(&LimitKey::request_weight()), 1);
        assert_eq!(limiter.used_weight_for(&LimitKey::orders_per_second()), 1);
    }

    #[test]
    fn test_multi_bucket_default_bucket_auto_created() {
        let mut limiter = RateLimiter::default_limiter();
        assert!(limiter.try_acquire_for(LimitKey::raw_requests(), 1));
        assert_eq!(limiter.used_weight_for(&LimitKey::raw_requests()), 1);
    }

    #[test]
    fn test_reset_after_429_clears_all_buckets() {
        let mut limiter = RateLimiter::default_limiter();
        limiter.register_bucket(LimitKey::orders_per_minute(), 100);
        limiter.try_acquire_weighted(50);
        limiter.try_acquire_for(LimitKey::orders_per_minute(), 50);

        limiter.reset_after_429();
        assert_eq!(limiter.used_weight(), 0);
        assert_eq!(limiter.used_weight_for(&LimitKey::orders_per_minute()), 0);
    }

    #[test]
    fn test_update_used_weight_for_specific_bucket() {
        let mut limiter = RateLimiter::default_limiter();
        limiter.register_bucket(LimitKey::orders_per_minute(), 100);
        limiter.update_used_weight_for(LimitKey::orders_per_minute(), 80);
        assert_eq!(limiter.used_weight_for(&LimitKey::orders_per_minute()), 80);
    }

    #[test]
    fn test_register_bucket_overwrites_existing() {
        let mut limiter = RateLimiter::default_limiter();
        assert_eq!(limiter.max_weight(), DEFAULT_MAX_WEIGHT);
        limiter.register_bucket(LimitKey::request_weight(), 1200);
        assert_eq!(limiter.max_weight(), 1200);
    }

    #[test]
    fn test_extract_order_count_from_headers_per_second() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("x-mbx-order-count-1s", "5".parse().unwrap());
        assert_eq!(
            extract_order_count_from_headers(&headers, LimitInterval::Second),
            Some(5)
        );
    }

    #[test]
    fn test_extract_order_count_from_headers_per_minute() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("x-mbx-order-count-1m", "80".parse().unwrap());
        assert_eq!(
            extract_order_count_from_headers(&headers, LimitInterval::Minute),
            Some(80)
        );
    }

    #[test]
    fn test_extract_order_count_from_headers_per_day() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("x-mbx-order-count-1d", "50000".parse().unwrap());
        assert_eq!(
            extract_order_count_from_headers(&headers, LimitInterval::Day),
            Some(50_000)
        );
    }

    #[test]
    fn test_extract_order_count_from_headers_missing() {
        let headers = reqwest::header::HeaderMap::new();
        assert_eq!(
            extract_order_count_from_headers(&headers, LimitInterval::Minute),
            None
        );
    }

    #[test]
    fn test_extract_order_count_five_minute_not_supported() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("x-mbx-order-count-1m", "50".parse().unwrap());
        assert_eq!(
            extract_order_count_from_headers(&headers, LimitInterval::FiveMinute),
            None
        );
    }

    #[test]
    fn test_weighted_acquire_default_bucket() {
        let mut limiter = RateLimiter::new(ApiQuota {
            max_weight: 1200,
            window_secs: 60,
        });
        assert!(limiter.try_acquire_weighted(50));
        assert_eq!(limiter.used_weight(), 50);
    }

    #[test]
    fn test_weighted_acquire_blocks_at_threshold() {
        let mut limiter = RateLimiter::new(ApiQuota {
            max_weight: 100,
            window_secs: 60,
        });
        assert!(limiter.try_acquire_weighted(90));
        assert!(!limiter.try_acquire_weighted(1));
    }
}
