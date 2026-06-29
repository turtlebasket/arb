//! Shared WS provider construction with a retry+backoff layer.
//!
//! Alchemy (and most providers) return HTTP 429 / "compute units per second"
//! errors under burst load. The `RetryBackoffLayer` transparently retries those
//! with exponential backoff and self-throttles to a CU/s budget, so the heavy
//! scan/verify/watch loops survive free-tier limits without per-call handling.

use alloy::providers::{Provider, ProviderBuilder, WsConnect};
use alloy::rpc::client::ClientBuilder;
use alloy::transports::layers::RetryBackoffLayer;

use crate::live::source::SourceError;

/// Connect a WS provider that retries on rate-limit (429/CUPS) errors.
pub async fn connect(url: &str) -> Result<impl Provider + Clone, SourceError> {
    // (max_rate_limit_retries, initial_backoff_ms, compute_units_per_second)
    let client = ClientBuilder::default()
        .layer(RetryBackoffLayer::new(20, 400, 300))
        .ws(WsConnect::new(url.to_string()))
        .await
        .map_err(|e| SourceError::Rpc(e.to_string()))?;
    Ok(ProviderBuilder::new().connect_client(client))
}
