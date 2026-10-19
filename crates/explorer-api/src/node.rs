//! Minimal karlsend gRPC client.
//!
//! karlsend exposes a single bidirectional `MessageStream`; every call here opens
//! its own stream on a shared HTTP/2 channel, sends one request, reads one reply
//! and closes the stream. The request side stays open until the reply arrives,
//! because the node may drop a stream whose request side has already ended.

use std::time::Duration;

use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::{Channel, Endpoint};

#[allow(clippy::all, clippy::pedantic)]
pub mod proto {
    tonic::include_proto!("protowire");
}

use proto::{
    karlsend_request::Payload as Req, karlsend_response::Payload as Resp, rpc_client::RpcClient,
    GetBalanceByAddressRequestMessage, GetBalancesByAddressesRequestMessage,
    GetBlockRequestMessage, GetCurrentBlockColorRequestMessage, GetUtxosByAddressesRequestMessage,
    KarlsendRequest, RpcBalancesByAddressesEntry, RpcBlock, RpcUtxosByAddressesEntry,
};

#[derive(Debug, thiserror::Error)]
pub enum NodeError {
    #[error("invalid node url")]
    InvalidUrl,
    #[error("node call timed out")]
    Timeout,
    #[error("node transport error: {0}")]
    Transport(#[from] tonic::Status),
    #[error("node closed the stream without a reply")]
    NoReply,
    #[error("node replied with an unexpected message")]
    UnexpectedReply,
    /// The node answered with an RPC error (for example utxoindex disabled).
    #[error("node error: {0}")]
    Rpc(String),
}

#[derive(Clone)]
pub struct NodeClient {
    client: RpcClient<Channel>,
    timeout: Duration,
}

impl NodeClient {
    /// Creates a lazily connecting client; nothing is dialled until the first call,
    /// so the API starts even while the node is down.
    pub fn new(url: &str, timeout: Duration) -> Result<Self, NodeError> {
        let channel = Endpoint::from_shared(url.to_string())
            .map_err(|_| NodeError::InvalidUrl)?
            .connect_timeout(Duration::from_secs(2))
            .tcp_nodelay(true)
            .connect_lazy();
        Ok(Self {
            // Blocks with many transactions exceed tonic's 4 MiB default.
            client: RpcClient::new(channel).max_decoding_message_size(64 * 1024 * 1024),
            timeout,
        })
    }

    async fn call(&self, payload: Req) -> Result<Resp, NodeError> {
        let mut client = self.client.clone();
        let fut = async move {
            let (tx, rx) = mpsc::channel(1);
            tx.send(KarlsendRequest {
                id: 1,
                payload: Some(payload),
            })
            .await
            .map_err(|_| NodeError::NoReply)?;
            let mut replies = client
                .message_stream(ReceiverStream::new(rx))
                .await?
                .into_inner();
            let reply = replies.message().await?.ok_or(NodeError::NoReply)?;
            // Keep the request side open until the reply has been read.
            drop(tx);
            reply.payload.ok_or(NodeError::UnexpectedReply)
        };
        tokio::time::timeout(self.timeout, fut)
            .await
            .map_err(|_| NodeError::Timeout)?
    }

    /// Returns the block if the node has its body. `Ok(None)` means the node does not
    /// know the block, or only has its header (pruned); the caller then falls back to
    /// the database.
    pub async fn get_block(
        &self,
        hash: &str,
        include_transactions: bool,
    ) -> Result<Option<RpcBlock>, NodeError> {
        let reply = self
            .call(Req::GetBlockRequest(GetBlockRequestMessage {
                hash: hash.to_string(),
                include_transactions,
            }))
            .await?;
        let Resp::GetBlockResponse(r) = reply else {
            return Err(NodeError::UnexpectedReply);
        };
        if r.error.is_some() {
            return Ok(None);
        }
        Ok(r.block
            .filter(|b| b.verbose_data.as_ref().is_some_and(|v| !v.is_header_only)))
    }

    /// Confirmed balance in sompi. Needs karlsend running with `--utxoindex`.
    pub async fn get_balance_by_address(&self, address: &str) -> Result<u64, NodeError> {
        let reply = self
            .call(Req::GetBalanceByAddressRequest(
                GetBalanceByAddressRequestMessage {
                    address: address.to_string(),
                },
            ))
            .await?;
        let Resp::GetBalanceByAddressResponse(r) = reply else {
            return Err(NodeError::UnexpectedReply);
        };
        match r.error {
            Some(e) => Err(NodeError::Rpc(e.message)),
            None => Ok(r.balance),
        }
    }

    pub async fn get_balances_by_addresses(
        &self,
        addresses: &[String],
    ) -> Result<Vec<RpcBalancesByAddressesEntry>, NodeError> {
        let reply = self
            .call(Req::GetBalancesByAddressesRequest(
                GetBalancesByAddressesRequestMessage {
                    addresses: addresses.to_vec(),
                },
            ))
            .await?;
        let Resp::GetBalancesByAddressesResponse(r) = reply else {
            return Err(NodeError::UnexpectedReply);
        };
        match r.error {
            Some(e) => Err(NodeError::Rpc(e.message)),
            None => Ok(r.entries),
        }
    }

    pub async fn get_utxos_by_addresses(
        &self,
        addresses: &[String],
    ) -> Result<Vec<RpcUtxosByAddressesEntry>, NodeError> {
        let reply = self
            .call(Req::GetUtxosByAddressesRequest(
                GetUtxosByAddressesRequestMessage {
                    addresses: addresses.to_vec(),
                },
            ))
            .await?;
        let Resp::GetUtxosByAddressesResponse(r) = reply else {
            return Err(NodeError::UnexpectedReply);
        };
        match r.error {
            Some(e) => Err(NodeError::Rpc(e.message)),
            None => Ok(r.entries),
        }
    }

    /// `Some(true)` for blue, `Some(false)` for red, `None` when the node cannot tell
    /// (for example while the block is not merged yet).
    pub async fn get_current_block_color(&self, hash: &str) -> Result<Option<bool>, NodeError> {
        let reply = self
            .call(Req::GetCurrentBlockColorRequest(
                GetCurrentBlockColorRequestMessage {
                    hash: hash.to_string(),
                },
            ))
            .await?;
        let Resp::GetCurrentBlockColorResponse(r) = reply else {
            return Err(NodeError::UnexpectedReply);
        };
        Ok(r.error.is_none().then_some(r.blue))
    }
}
