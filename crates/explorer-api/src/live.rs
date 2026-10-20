//! Live block feed.
//!
//! One background task keeps a single persistent `BlockAdded` subscription to
//! karlsend, converts each block into a compact summary and fans it out to every
//! connected browser through a broadcast channel. Blocks are batched (default
//! 250 ms) so that at 10 BPS a client receives four small messages per second
//! instead of ten, and the per-block cost on the node stays constant regardless
//! of how many browsers are watching.

use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use serde::Serialize;
use tokio::sync::broadcast;

use crate::{
    address::{script_to_address, MAINNET_PREFIX},
    node::{
        proto::{karlsend_response::Payload as Resp, RpcBlock, RpcTransaction},
        NodeClient,
    },
};

/// Transactions listed per block in the feed; `txCount` always carries the total.
const MAX_TXS_PER_BLOCK: usize = 50;

#[derive(Debug, Clone)]
pub struct LiveConfig {
    /// How long blocks are collected before one batch is sent.
    pub batch_interval: Duration,
    /// Blocks kept for the snapshot a new client receives first.
    pub recent_blocks: usize,
    /// Reconnect when the node has sent nothing for this long.
    pub idle_timeout: Duration,
    /// Upper bound on concurrent stream clients.
    pub max_clients: usize,
}

impl Default for LiveConfig {
    fn default() -> Self {
        Self {
            batch_interval: Duration::from_millis(250),
            recent_blocks: 50,
            idle_timeout: Duration::from_secs(60),
            max_clients: 500,
        }
    }
}

impl LiveConfig {
    pub fn from_env() -> Self {
        let d = Self::default();
        let num = |k: &str| std::env::var(k).ok().and_then(|v| v.parse::<u64>().ok());
        Self {
            batch_interval: num("LIVE_BATCH_MS").map_or(d.batch_interval, Duration::from_millis),
            recent_blocks: num("LIVE_RECENT_BLOCKS").map_or(d.recent_blocks, |v| v as usize),
            idle_timeout: num("LIVE_IDLE_TIMEOUT_S").map_or(d.idle_timeout, Duration::from_secs),
            max_clients: num("LIVE_MAX_CLIENTS").map_or(d.max_clients, |v| v as usize),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveTx {
    pub id: String,
    /// Sum of all outputs, sompi, as a string (exceeds JS safe integers).
    pub amount: String,
    /// Address of the first output, derived from its script.
    pub to: String,
    pub outputs: usize,
    pub coinbase: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveBlock {
    pub hash: String,
    /// Block timestamp, unix milliseconds.
    pub timestamp: i64,
    pub blue_score: String,
    pub daa_score: String,
    pub tx_count: usize,
    pub txs: Vec<LiveTx>,
}

fn live_tx(t: RpcTransaction) -> LiveTx {
    let amount: u128 = t.outputs.iter().map(|o| u128::from(o.amount)).sum();
    let to = t
        .outputs
        .first()
        .and_then(|o| o.script_public_key.as_ref())
        .and_then(|spk| hex::decode(&spk.script_public_key).ok())
        .and_then(|script| script_to_address(MAINNET_PREFIX, &script))
        .unwrap_or_default();
    LiveTx {
        id: t.verbose_data.map(|v| v.transaction_id).unwrap_or_default(),
        amount: amount.to_string(),
        to,
        outputs: t.outputs.len(),
        coinbase: t.inputs.is_empty(),
    }
}

pub fn live_block(b: RpcBlock) -> LiveBlock {
    let h = b.header.unwrap_or_default();
    let v = b.verbose_data.unwrap_or_default();
    let tx_count = v.transaction_ids.len().max(b.transactions.len());
    LiveBlock {
        hash: v.hash,
        timestamp: h.timestamp,
        blue_score: h.blue_score.to_string(),
        daa_score: h.daa_score.to_string(),
        tx_count,
        txs: b
            .transactions
            .into_iter()
            .take(MAX_TXS_PER_BLOCK)
            .map(live_tx)
            .collect(),
    }
}

#[derive(Serialize)]
struct Batch<'a> {
    blocks: &'a [LiveBlock],
}

pub struct LiveHub {
    config: LiveConfig,
    tx: broadcast::Sender<Arc<str>>,
    recent: Mutex<VecDeque<LiveBlock>>,
    connected: AtomicBool,
}

impl LiveHub {
    pub fn new(config: LiveConfig) -> Arc<Self> {
        // Capacity in batches: ~16 s of history at 250 ms before a slow client lags.
        let (tx, _) = broadcast::channel(64);
        Arc::new(Self {
            recent: Mutex::new(VecDeque::with_capacity(config.recent_blocks)),
            config,
            tx,
            connected: AtomicBool::new(false),
        })
    }

    /// Starts the node subscription task; it reconnects on its own forever.
    pub fn spawn(config: LiveConfig, node: NodeClient) -> Arc<Self> {
        let hub = Self::new(config);
        tokio::spawn(run(hub.clone(), node));
        hub
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Arc<str>> {
        self.tx.subscribe()
    }

    pub fn clients(&self) -> usize {
        self.tx.receiver_count()
    }

    pub fn max_clients(&self) -> usize {
        self.config.max_clients
    }

    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    /// Recent blocks, newest last, serialized as a batch.
    pub fn snapshot_json(&self) -> String {
        let recent = self.recent.lock().expect("live recent lock");
        let blocks: Vec<LiveBlock> = recent.iter().cloned().collect();
        serde_json::to_string(&Batch { blocks: &blocks })
            .unwrap_or_else(|_| "{\"blocks\":[]}".into())
    }

    /// Records a batch and fans it out. Public so tests can drive the hub without a node.
    pub fn publish(&self, blocks: Vec<LiveBlock>) {
        if blocks.is_empty() {
            return;
        }
        {
            let mut recent = self.recent.lock().expect("live recent lock");
            for b in &blocks {
                if recent.len() == self.config.recent_blocks {
                    recent.pop_front();
                }
                recent.push_back(b.clone());
            }
        }
        if let Ok(json) = serde_json::to_string(&Batch { blocks: &blocks }) {
            // No receivers is not an error: nobody is watching right now.
            let _ = self.tx.send(Arc::from(json));
        }
    }

    pub fn set_connected(&self, v: bool) {
        self.connected.store(v, Ordering::Relaxed);
    }
}

async fn run(hub: Arc<LiveHub>, node: NodeClient) {
    let mut backoff = Duration::from_secs(1);
    loop {
        match node.subscribe_block_added().await {
            Ok((keep_open, mut stream)) => {
                tracing::info!("live: subscribed to block-added notifications");
                hub.set_connected(true);
                backoff = Duration::from_secs(1);

                let mut pending: Vec<LiveBlock> = Vec::new();
                let mut flush = tokio::time::interval(hub.config.batch_interval);
                flush.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                let mut last_message = tokio::time::Instant::now();

                let reason = loop {
                    tokio::select! {
                        msg = stream.message() => match msg {
                            Ok(Some(m)) => {
                                last_message = tokio::time::Instant::now();
                                if let Some(Resp::BlockAddedNotification(n)) = m.payload {
                                    if let Some(b) = n.block {
                                        pending.push(live_block(b));
                                    }
                                }
                            }
                            Ok(None) => break "node closed the stream".to_string(),
                            Err(e) => break format!("stream error: {e}"),
                        },
                        _ = flush.tick() => {
                            if !pending.is_empty() {
                                hub.publish(std::mem::take(&mut pending));
                            }
                            if last_message.elapsed() > hub.config.idle_timeout {
                                break "no blocks from the node within the idle timeout".to_string();
                            }
                        }
                    }
                };
                hub.publish(pending);
                drop(keep_open);
                hub.set_connected(false);
                tracing::warn!(%reason, "live: subscription ended, reconnecting");
            }
            Err(e) => {
                hub.set_connected(false);
                tracing::warn!(error = %e, retry_in = ?backoff, "live: cannot subscribe");
            }
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
}
