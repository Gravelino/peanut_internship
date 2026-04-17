use std::sync::Mutex;
use std::time::Instant;

const DEFAULT_MAX_WEIGHT: u32 = 1200;
const WEIGHT_RESET_SECS: u64 = 60;

#[derive(Debug)]
struct LimiterState {
    current_weight: u32,
    window_start: Instant,
}

#[derive(Debug)]
pub struct RateLimiter {
    state: Mutex<LimiterState>,
    max_weight: u32,
    window_secs: u64,
}

impl RateLimiter {
    pub fn new(max_weight: u32) -> Self {
        Self {
            state: Mutex::new(LimiterState {
                current_weight: 0,
                window_start: Instant::now(),
            }),
            max_weight,
            window_secs: WEIGHT_RESET_SECS,
        }
    }

    pub fn acquire(&self, weight: u32) -> bool {
        let mut state = self.state.lock().unwrap();
        let now = Instant::now();
        let elapsed = now.duration_since(state.window_start).as_secs();

        if elapsed >= self.window_secs {
            state.current_weight = 0;
            state.window_start = now;
        }

        if state.current_weight + weight <= self.max_weight {
            state.current_weight += weight;
            true
        } else {
            false
        }
    }

    pub fn wait_and_acquire(&self, weight: u32) {
        while !self.acquire(weight) {
            let state = self.state.lock().unwrap();
            let elapsed = state.window_start.elapsed().as_secs();
            let remaining = self.window_secs.saturating_sub(elapsed);
            drop(state);
            let sleep_secs = remaining.max(1);
            std::thread::sleep(std::time::Duration::from_secs(sleep_secs));
        }
    }

    pub fn current_weight(&self) -> u32 {
        let state = self.state.lock().unwrap();
        state.current_weight
    }

    pub fn max_weight(&self) -> u32 {
        self.max_weight
    }
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_WEIGHT)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_acquire_within_limit() {
        let limiter = RateLimiter::new(100);
        assert!(limiter.acquire(50));
        assert!(limiter.acquire(50));
    }

    #[test]
    fn test_acquire_blocks_when_exhausted() {
        let limiter = RateLimiter::new(100);
        assert!(limiter.acquire(80));
        assert!(!limiter.acquire(30));
    }

    #[test]
    fn test_acquire_exact_limit() {
        let limiter = RateLimiter::new(100);
        assert!(limiter.acquire(100));
        assert!(!limiter.acquire(1));
    }

    #[test]
    fn test_current_weight_tracking() {
        let limiter = RateLimiter::new(100);
        limiter.acquire(30);
        assert_eq!(limiter.current_weight(), 30);
        limiter.acquire(20);
        assert_eq!(limiter.current_weight(), 50);
    }
}
