use crate::types::{MarketSnapshot, Signal};
use anyhow::Result;
use async_trait::async_trait;

/// Adapter for the user's existing independent strategy. No webhook or LLM dependency.
#[async_trait]
pub trait IndependentStrategy: Send + Sync {
    async fn evaluate(&self, snapshot: &MarketSnapshot) -> Result<Option<Signal>>;
}

/// Safe placeholder until the user's existing strategy implementation is supplied.
pub struct NoopStrategy;
#[async_trait]
impl IndependentStrategy for NoopStrategy {
    async fn evaluate(&self, _snapshot: &MarketSnapshot) -> Result<Option<Signal>> {
        Ok(None)
    }
}
