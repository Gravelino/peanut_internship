use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::Mutex;
use tracing::{debug, warn};

use crate::chain::client::ChainClient;
use crate::chain::errors::ChainResult;
use crate::core::types::{Address, BlockId};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct NonceKey {
    chain_id: u64,
    address: String,
}

#[derive(Debug, Default)]
struct NonceState {
    next_nonce: Option<u64>,
}

#[derive(Debug, Clone, Default)]
pub struct NonceManager {
    states: Arc<Mutex<HashMap<NonceKey, NonceState>>>,
}

impl NonceManager {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn reserve_next(
        &self,
        client: &ChainClient,
        chain_id: u64,
        address: &Address,
    ) -> ChainResult<u64> {
        let key = NonceKey {
            chain_id,
            address: address.lower(),
        };
        let mut states = self.states.lock().await;
        let state = states.entry(key).or_default();
        let nonce = match state.next_nonce {
            Some(next) => next,
            None => {
                let pending = client.get_nonce(address, BlockId::Pending).await?;
                debug!(%address, chain_id, pending, "nonce manager synced pending nonce");
                pending
            }
        };
        state.next_nonce = Some(nonce.saturating_add(1));
        debug!(%address, chain_id, nonce, next = ?state.next_nonce, "nonce reserved");
        Ok(nonce)
    }

    pub async fn observe_pending(
        &self,
        client: &ChainClient,
        chain_id: u64,
        address: &Address,
    ) -> ChainResult<u64> {
        let pending = client.get_nonce(address, BlockId::Pending).await?;
        let key = NonceKey {
            chain_id,
            address: address.lower(),
        };
        let mut states = self.states.lock().await;
        let state = states.entry(key).or_default();
        if state.next_nonce.is_none_or(|next| next < pending) {
            state.next_nonce = Some(pending);
        }
        Ok(pending)
    }

    pub async fn mark_failed(&self, chain_id: u64, address: &Address, nonce: u64) {
        let key = NonceKey {
            chain_id,
            address: address.lower(),
        };
        let mut states = self.states.lock().await;
        if let Some(state) = states.get_mut(&key) {
            if state.next_nonce == Some(nonce.saturating_add(1)) {
                state.next_nonce = Some(nonce);
                debug!(%address, chain_id, nonce, "nonce reservation rolled back after failure");
            } else {
                warn!(
                    %address,
                    chain_id,
                    nonce,
                    next = ?state.next_nonce,
                    "nonce failure observed out of order; leaving reservation state unchanged"
                );
            }
        }
    }

    pub async fn reset(&self, chain_id: u64, address: &Address) {
        let key = NonceKey {
            chain_id,
            address: address.lower(),
        };
        self.states.lock().await.remove(&key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(hex: &str) -> Address {
        Address::new(hex).unwrap()
    }

    #[tokio::test]
    async fn failed_nonce_rolls_back_only_latest_reservation() {
        let manager = NonceManager::new();
        let address = addr("0x0000000000000000000000000000000000000001");
        let key = NonceKey {
            chain_id: 42161,
            address: address.lower(),
        };
        manager.states.lock().await.insert(
            key.clone(),
            NonceState {
                next_nonce: Some(10),
            },
        );

        manager.mark_failed(42161, &address, 9).await;
        assert_eq!(manager.states.lock().await[&key].next_nonce, Some(9));
    }

    #[tokio::test]
    async fn out_of_order_failure_does_not_rewind_past_reserved_nonce() {
        let manager = NonceManager::new();
        let address = addr("0x0000000000000000000000000000000000000001");
        let key = NonceKey {
            chain_id: 42161,
            address: address.lower(),
        };
        manager.states.lock().await.insert(
            key.clone(),
            NonceState {
                next_nonce: Some(12),
            },
        );

        manager.mark_failed(42161, &address, 9).await;
        assert_eq!(manager.states.lock().await[&key].next_nonce, Some(12));
    }
}
