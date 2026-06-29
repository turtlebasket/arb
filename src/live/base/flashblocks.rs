//! Base-specific: **Flashblocks** preconfirmations.
//!
//! Flashblocks are an OP-stack/Base feature (rollup-boost): the sequencer emits
//! partial-block "preconfirmations" roughly every ~200ms, appended in `index`
//! order to the in-progress block, well before the sealed block (~2s). This
//! module is therefore **NOT chain-agnostic** — it only makes sense on Base (or
//! other Flashblocks-enabled OP chains). Other chains would model low-latency
//! visibility differently (e.g. pending-tx/mempool) and must not depend on this.
//!
//! Chain-agnostic types (`PoolEvent`, `PoolState`) are reused from
//! [`crate::live::event`]; only the Flashblocks framing lives here.

use std::pin::Pin;

use async_trait::async_trait;
use futures::stream::Stream;

use crate::live::event::PoolEvent;
use crate::live::source::SourceError;

/// Where a piece of state came from in Base's confirmation pipeline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Confirmation {
    /// Preconfirmation: the `index`-th Flashblock of `block` (0,1,2,…).
    Flashblock { index: u64 },
    /// Canonical sealed block.
    Sealed,
}

/// One Flashblock preconfirmation: a batch of pool-state updates appended to the
/// in-progress `block`. `events` are in intra-block order with block-cumulative
/// `log_index`, so ordering is consistent with the eventual sealed block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Flashblock {
    pub block: u64,
    pub index: u64,
    pub events: Vec<PoolEvent>,
}

pub type FlashblockStream = Pin<Box<dyn Stream<Item = Flashblock> + Send>>;

/// A source of Base Flashblock preconfirmations. Kept separate from the
/// sealed-block [`crate::live::source::ChainSource`] so the two tiers can be
/// mocked and reconciled independently.
#[async_trait]
pub trait PreconfSource: Send + Sync + 'static {
    async fn subscribe(&self) -> Result<FlashblockStream, SourceError>;
}

/// Deterministic in-memory preconfirmation source for tests.
pub struct MockPreconf {
    batches: Vec<Flashblock>,
}

impl MockPreconf {
    pub fn new(batches: Vec<Flashblock>) -> Self {
        Self { batches }
    }
}

#[async_trait]
impl PreconfSource for MockPreconf {
    async fn subscribe(&self) -> Result<FlashblockStream, SourceError> {
        Ok(Box::pin(futures::stream::iter(self.batches.clone())))
    }
}

/// Real Base Flashblocks source — **STUB**.
///
/// The production endpoint is the Base Flashblocks WebSocket
/// (`wss://mainnet.flashblocks.base.org/ws`, or Alchemy's Flashblocks API),
/// which streams `flashblocks_subscribe`-style payloads: per-flashblock diffs
/// with `index`, `base` metadata on index 0, and appended txs/receipts/logs.
/// Wiring = connect, parse each payload, decode pool logs into [`PoolEvent`]s,
/// and emit a [`Flashblock`]. Decoding reuses the same log-decode path as the
/// sealed source. Until then this yields an empty stream so the dual engine
/// runs sealed-only.
#[cfg(feature = "live-rpc")]
pub struct BaseFlashblocksSource {
    pub url: String,
}

#[cfg(feature = "live-rpc")]
impl BaseFlashblocksSource {
    /// Default public Base Flashblocks WS endpoint.
    pub const DEFAULT_URL: &'static str = "wss://mainnet.flashblocks.base.org/ws";

    pub fn new(url: impl Into<String>) -> Self {
        Self { url: url.into() }
    }
}

/// Diagnostic: connect to the Flashblocks WS and dump the first `n` messages'
/// structure (type, length, top-level JSON keys, index/block_number, whether
/// logs are present) so we can decode against the ACTUAL current format.
#[cfg(feature = "live-rpc")]
pub async fn probe(url: &str, n: usize) -> Result<(), SourceError> {
    use futures::StreamExt;
    use tokio_tungstenite::tungstenite::Message;

    eprintln!("connecting to {url} ...");
    let (mut ws, _resp) = tokio_tungstenite::connect_async(url)
        .await
        .map_err(|e| SourceError::Rpc(format!("connect: {e}")))?;
    eprintln!("connected; reading {n} messages...\n");

    let mut count = 0usize;
    while let Some(msg) = ws.next().await {
        let msg = msg.map_err(|e| SourceError::Rpc(e.to_string()))?;
        let bytes: Vec<u8> = match msg {
            Message::Text(t) => t.into_bytes(),
            Message::Binary(b) => b,
            Message::Ping(_) | Message::Pong(_) => continue,
            Message::Close(_) => break,
            _ => continue,
        };
        // The proxy may brotli-compress; try raw UTF-8 first, then brotli.
        let text = match std::str::from_utf8(&bytes) {
            Ok(s) if s.trim_start().starts_with('{') => s.to_string(),
            _ => {
                let mut out = Vec::new();
                match brotli_decompress(&bytes, &mut out) {
                    Ok(()) => String::from_utf8_lossy(&out).to_string(),
                    Err(_) => {
                        eprintln!(
                            "msg #{count}: {} bytes, not UTF-8 JSON and not brotli; first bytes: {:02x?}",
                            bytes.len(),
                            &bytes[..bytes.len().min(16)]
                        );
                        count += 1;
                        if count >= n {
                            break;
                        }
                        continue;
                    }
                }
            }
        };
        match serde_json::from_str::<serde_json::Value>(&text) {
            Ok(v) => {
                let keys: Vec<&str> = v.as_object().map(|o| o.keys().map(|k| k.as_str()).collect()).unwrap_or_default();
                let index = v.get("index").and_then(|x| x.as_u64());
                let block = v
                    .get("metadata")
                    .and_then(|m| m.get("block_number"))
                    .and_then(|x| x.as_u64())
                    .or_else(|| v.get("diff").and_then(|d| d.get("block_number")).and_then(|x| x.as_u64()));
                let has_meta_receipts = v.get("metadata").and_then(|m| m.get("receipts")).is_some();
                let n_tx = v.get("diff").and_then(|d| d.get("transactions")).and_then(|t| t.as_array()).map(|a| a.len());
                eprintln!(
                    "msg #{count}: keys={keys:?} index={index:?} block={block:?} meta.receipts={has_meta_receipts} diff.txs={n_tx:?}"
                );
                if count == 0 {
                    // dump the full first message (truncated) to see exact shape
                    let pretty = serde_json::to_string_pretty(&v).unwrap_or(text);
                    eprintln!("--- first message (truncated 2KB) ---\n{}\n---", &pretty[..pretty.len().min(2048)]);
                }
            }
            Err(e) => eprintln!("msg #{count}: JSON parse error: {e}; len={}", text.len()),
        }
        count += 1;
        if count >= n {
            break;
        }
    }
    Ok(())
}

/// Decompress a brotli stream (the Flashblocks proxy brotli-compresses frames).
#[cfg(feature = "live-rpc")]
fn brotli_decompress(input: &[u8], out: &mut Vec<u8>) -> Result<(), ()> {
    use std::io::Read;
    let mut d = brotli::Decompressor::new(input, 4096);
    d.read_to_end(out).map(|_| ()).map_err(|_| ())
}

/// Decode one WS frame (brotli-or-raw JSON) into `(block_number, index)`.
/// Returns `None` for control frames / unparseable data.
#[cfg(feature = "live-rpc")]
fn decode_frame(bytes: &[u8]) -> Option<(u64, u64)> {
    let text = match std::str::from_utf8(bytes) {
        Ok(s) if s.trim_start().starts_with('{') => s.to_string(),
        _ => {
            let mut out = Vec::new();
            brotli_decompress(bytes, &mut out).ok()?;
            String::from_utf8(out).ok()?
        }
    };
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    let index = v.get("index")?.as_u64()?;
    // `metadata.block_number` is a plain integer; `base.block_number` (index 0
    // only) is a 0x-hex string. Accept either.
    let as_num = |x: Option<&serde_json::Value>| -> Option<u64> {
        let x = x?;
        x.as_u64()
            .or_else(|| x.as_str().and_then(|s| u64::from_str_radix(s.trim_start_matches("0x"), 16).ok()))
    };
    let block = as_num(v.get("metadata").and_then(|m| m.get("block_number")))
        .or_else(|| as_num(v.get("base").and_then(|b| b.get("block_number"))))?;
    Some((block, index))
}

#[cfg(feature = "live-rpc")]
#[async_trait]
impl PreconfSource for BaseFlashblocksSource {
    async fn subscribe(&self) -> Result<FlashblockStream, SourceError> {
        use futures::StreamExt;
        use tokio_tungstenite::tungstenite::Message;

        let (mut ws, _) = tokio_tungstenite::connect_async(&self.url)
            .await
            .map_err(|e| SourceError::Rpc(format!("flashblocks connect: {e}")))?;

        // NOTE: the public Flashblocks stream carries RLP transactions but NOT
        // receipts/logs (v0.8.0), so we can't derive pool-state deltas from it.
        // We emit each flashblock's (block, index) as a ~200ms cadence signal
        // with empty `events`; acting on preconfirmed state is done by quoting
        // against the `pending` block tag (a flashblocks-aware RPC) or a
        // logs-bearing stream (e.g. bloXroute `newFlashblockTransactions`).
        let (tx, rx) = futures::channel::mpsc::unbounded::<Flashblock>();
        tokio::spawn(async move {
            while let Some(Ok(msg)) = ws.next().await {
                let bytes = match msg {
                    Message::Text(t) => t.into_bytes(),
                    Message::Binary(b) => b,
                    Message::Close(_) => break,
                    _ => continue,
                };
                if let Some((block, index)) = decode_frame(&bytes) {
                    if tx.unbounded_send(Flashblock { block, index, events: Vec::new() }).is_err() {
                        break;
                    }
                }
            }
        });
        Ok(Box::pin(rx))
    }
}
