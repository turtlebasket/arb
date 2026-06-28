//! Real WebSocket `ChainSource` for EVM chains (Base / BSC), via alloy.
//!
//! Enabled with the `live-rpc` feature. The operator supplies a WS endpoint
//! (e.g. `BASE_WSS_URL`) and the set of pool addresses to track. Implements the
//! subscribe-first pattern: [`ChainSource::subscribe`] opens the log + header
//! subscriptions; [`ChainSource::snapshot_pool`] reads pinned state via
//! `eth_call`.
//!
//! Events are decoded from raw log bytes (topic0 + data) rather than generated
//! ABI types, which keeps this robust against alloy minor-version churn. Only
//! UniV2-style `Sync` is wired here; V3 `Swap` decoding follows the same shape.

use alloy::eips::BlockId;
use alloy::providers::{Provider, ProviderBuilder, WsConnect};
use alloy::rpc::types::{Filter, TransactionInput, TransactionRequest};
use alloy_primitives::{keccak256, Bytes, TxKind, B256};
use async_trait::async_trait;
use futures::StreamExt;

use crate::live::event::{ChainItem, EventKind, PoolEvent, PoolState};
use crate::live::source::{ChainSource, ItemStream, SourceError};
use crate::types::{Address, U256};

/// `getReserves()` selector (UniswapV2 pair).
const GET_RESERVES_SELECTOR: [u8; 4] = [0x09, 0x02, 0xf1, 0xac];

/// topic0 for `Sync(uint112,uint112)`.
fn sync_topic() -> B256 {
    keccak256("Sync(uint112,uint112)")
}

pub struct WsChainSource {
    url: String,
    pools: Vec<Address>,
}

impl WsChainSource {
    pub fn new(url: impl Into<String>, pools: Vec<Address>) -> Self {
        Self {
            url: url.into(),
            pools,
        }
    }

    async fn connect(&self) -> Result<impl Provider, SourceError> {
        let ws = WsConnect::new(self.url.clone());
        ProviderBuilder::new()
            .connect_ws(ws)
            .await
            .map_err(|e| SourceError::Rpc(e.to_string()))
    }
}

/// Decode a UniV2 `Sync(uint112,uint112)` log into a [`PoolEvent`]. Returns
/// `None` for any other event or an incomplete log.
fn decode_sync(log: &alloy::rpc::types::Log) -> Option<PoolEvent> {
    let topics = log.topics();
    let t0 = topics.first()?;
    if *t0 != sync_topic() {
        return None;
    }
    let data = log.data().data.as_ref();
    if data.len() < 64 {
        return None;
    }
    Some(PoolEvent {
        pool: log.address(),
        block: log.block_number?,
        log_index: log.log_index?,
        kind: EventKind::SyncV2 {
            reserve0: U256::from_be_slice(&data[0..32]),
            reserve1: U256::from_be_slice(&data[32..64]),
        },
    })
}

#[async_trait]
impl ChainSource for WsChainSource {
    async fn subscribe(&self) -> Result<ItemStream, SourceError> {
        let provider = self.connect().await?;

        let blocks_sub = provider
            .subscribe_blocks()
            .await
            .map_err(|e| SourceError::Rpc(e.to_string()))?;
        let filter = Filter::new()
            .address(self.pools.clone())
            .event_signature(sync_topic());
        let logs_sub = provider
            .subscribe_logs(&filter)
            .await
            .map_err(|e| SourceError::Rpc(e.to_string()))?;

        // A task owns the provider + subscriptions and forwards normalized items.
        let (tx, rx) = futures::channel::mpsc::unbounded::<ChainItem>();
        tokio::spawn(async move {
            let mut blocks = blocks_sub.into_stream();
            let mut logs = logs_sub.into_stream();
            loop {
                tokio::select! {
                    Some(h) = blocks.next() => {
                        let item = ChainItem::NewHead {
                            number: h.number,
                            hash: h.hash,
                            parent_hash: h.parent_hash,
                        };
                        if tx.unbounded_send(item).is_err() { break; }
                    }
                    Some(log) = logs.next() => {
                        if let Some(ev) = decode_sync(&log) {
                            if tx.unbounded_send(ChainItem::Event(ev)).is_err() { break; }
                        }
                    }
                    else => break,
                }
            }
            drop(provider); // keep alive for the stream's lifetime
        });

        Ok(Box::pin(rx))
    }

    async fn snapshot_pool(&self, pool: Address, at: u64) -> Result<PoolState, SourceError> {
        let provider = self.connect().await?;
        let tx = TransactionRequest {
            to: Some(TxKind::Call(pool)),
            input: TransactionInput::new(Bytes::from(GET_RESERVES_SELECTOR.to_vec())),
            ..Default::default()
        };
        let bytes = provider
            .call(tx)
            .block(BlockId::number(at))
            .await
            .map_err(|e| SourceError::Rpc(e.to_string()))?;
        let raw = bytes.as_ref();
        if raw.len() < 64 {
            return Err(SourceError::Rpc(format!(
                "getReserves returned {} bytes",
                raw.len()
            )));
        }
        Ok(PoolState::UniV2 {
            reserve0: U256::from_be_slice(&raw[0..32]),
            reserve1: U256::from_be_slice(&raw[32..64]),
        })
    }

    async fn head(&self) -> Result<u64, SourceError> {
        let provider = self.connect().await?;
        provider
            .get_block_number()
            .await
            .map_err(|e| SourceError::Rpc(e.to_string()))
    }
}
