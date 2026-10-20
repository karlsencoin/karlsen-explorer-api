//! Live feed: node subscription, batching, reconnect and the SSE endpoint.

#[allow(dead_code)]
mod common;

use std::{
    net::SocketAddr,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

use axum::{body::Body, http::Request};
use common::{dead_pool, request};
use http_body_util::BodyExt;
use karlsen_explorer_api::{
    address::address_to_script,
    live::{live_block, LiveConfig, LiveHub},
    node::{
        proto::{
            karlsend_request::Payload as Req,
            karlsend_response::Payload as Resp,
            rpc_server::{Rpc, RpcServer},
            BlockAddedNotificationMessage, KarlsendRequest, KarlsendResponse,
            NotifyBlockAddedResponseMessage, RpcBlock, RpcBlockHeader, RpcBlockVerboseData,
            RpcOutpoint, RpcScriptPublicKey, RpcTransaction, RpcTransactionInput,
            RpcTransactionOutput, RpcTransactionVerboseData,
        },
        NodeClient,
    },
    routes, AppState,
};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_stream::wrappers::{ReceiverStream, TcpListenerStream};
use tonic::{Request as TReq, Response, Status, Streaming};
use tower::ServiceExt;

const ADDR: &str = "karlsen:qpyztau9s83yfr4qskwz4ws07a0pm6plp3h6v7fx33mshpln87qjzh4hu9rxv";

fn output(amount: u64) -> RpcTransactionOutput {
    RpcTransactionOutput {
        amount,
        script_public_key: Some(RpcScriptPublicKey {
            version: 0,
            script_public_key: hex::encode(address_to_script(ADDR).expect("valid address")),
        }),
        verbose_data: None,
    }
}

fn block(n: u64) -> RpcBlock {
    let coinbase = RpcTransaction {
        outputs: vec![output(1_951_236_555)],
        verbose_data: Some(RpcTransactionVerboseData {
            transaction_id: format!("{n:064x}"),
            ..Default::default()
        }),
        ..Default::default()
    };
    let transfer = RpcTransaction {
        inputs: vec![RpcTransactionInput {
            previous_outpoint: Some(RpcOutpoint {
                transaction_id: "ab".repeat(32),
                index: 0,
            }),
            ..Default::default()
        }],
        // Sum exceeds 2^53 to prove amounts are not routed through f64.
        outputs: vec![output(9_007_199_254_740_993), output(7)],
        verbose_data: Some(RpcTransactionVerboseData {
            transaction_id: format!("{:064x}", n + 1_000),
            ..Default::default()
        }),
        ..Default::default()
    };
    RpcBlock {
        header: Some(RpcBlockHeader {
            timestamp: 1_760_000_000_000 + n as i64,
            blue_score: 93_000_000 + n,
            daa_score: 95_000_000 + n,
            ..Default::default()
        }),
        transactions: vec![coinbase, transfer],
        verbose_data: Some(RpcBlockVerboseData {
            hash: format!("{:064x}", 0xb10c_0000 + n),
            transaction_ids: vec!["x".into(), "y".into()],
            ..Default::default()
        }),
    }
}

/// Mock karlsend: acknowledges `notifyBlockAdded`, then sends `per_session` blocks,
/// then closes the stream (to exercise reconnects).
struct MockNode {
    per_session: u64,
    sessions: Arc<AtomicUsize>,
}

#[tonic::async_trait]
impl Rpc for MockNode {
    type MessageStreamStream = ReceiverStream<Result<KarlsendResponse, Status>>;

    async fn message_stream(
        &self,
        request: TReq<Streaming<KarlsendRequest>>,
    ) -> Result<Response<Self::MessageStreamStream>, Status> {
        let mut inbound = request.into_inner();
        let (tx, rx) = mpsc::channel(16);
        let session = self.sessions.fetch_add(1, Ordering::SeqCst) as u64;
        let per_session = self.per_session;
        tokio::spawn(async move {
            let Ok(Some(req)) = inbound.message().await else {
                return;
            };
            assert!(matches!(req.payload, Some(Req::NotifyBlockAddedRequest(_))));
            let send = |p| KarlsendResponse {
                id: req.id,
                payload: Some(p),
            };
            let _ = tx
                .send(Ok(send(Resp::NotifyBlockAddedResponse(
                    NotifyBlockAddedResponseMessage { error: None },
                ))))
                .await;
            for i in 0..per_session {
                let n = session * per_session + i;
                let _ = tx
                    .send(Ok(send(Resp::BlockAddedNotification(
                        BlockAddedNotificationMessage {
                            block: Some(block(n)),
                        },
                    ))))
                    .await;
            }
            // Dropping `tx` ends the reply stream.
        });
        Ok(Response::new(ReceiverStream::new(rx)))
    }
}

async fn start_mock(per_session: u64) -> (SocketAddr, Arc<AtomicUsize>) {
    let sessions = Arc::new(AtomicUsize::new(0));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let service = RpcServer::new(MockNode {
        per_session,
        sessions: sessions.clone(),
    });
    tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(service)
            .serve_with_incoming(TcpListenerStream::new(listener))
            .await
            .unwrap();
    });
    (addr, sessions)
}

fn fast_config() -> LiveConfig {
    LiveConfig {
        batch_interval: Duration::from_millis(20),
        recent_blocks: 5,
        idle_timeout: Duration::from_secs(30),
        max_clients: 2,
    }
}

fn snapshot(hub: &LiveHub) -> Value {
    serde_json::from_str(&hub.snapshot_json()).unwrap()
}

#[test]
fn block_summary_derives_address_and_keeps_exact_amounts() {
    let b = live_block(block(1));
    assert_eq!(b.tx_count, 2);
    assert_eq!(b.blue_score, "93000001");
    assert!(b.txs[0].coinbase);
    assert!(!b.txs[1].coinbase);
    assert_eq!(b.txs[0].to, ADDR);
    assert_eq!(b.txs[1].amount, "9007199254741000");
    assert_eq!(b.txs[1].outputs, 2);
}

#[tokio::test]
async fn hub_subscribes_batches_and_reconnects() {
    let (addr, sessions) = start_mock(3).await;
    let node = NodeClient::new(&format!("http://{addr}"), Duration::from_secs(5)).unwrap();
    let hub = LiveHub::spawn(fast_config(), node);

    // First session delivers 3 blocks and closes; the hub reconnects (1 s backoff)
    // and the second session delivers 3 more. The snapshot keeps the newest 5.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let blocks = snapshot(&hub)["blocks"].as_array().unwrap().len();
        if sessions.load(Ordering::SeqCst) >= 2 && blocks == 5 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "hub did not reconnect in time"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let s = snapshot(&hub);
    let scores: Vec<&str> = s["blocks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["blueScore"].as_str().unwrap())
        .collect();
    assert_eq!(
        scores,
        ["93000001", "93000002", "93000003", "93000004", "93000005"]
    );
}

async fn next_event(body: &mut Body) -> String {
    let mut buf = String::new();
    loop {
        let frame = tokio::time::timeout(Duration::from_secs(5), body.frame())
            .await
            .expect("event in time")
            .expect("stream open")
            .unwrap();
        if let Some(data) = frame.data_ref() {
            buf.push_str(std::str::from_utf8(data).unwrap());
            if buf.ends_with("\n\n") {
                return buf;
            }
        }
    }
}

#[tokio::test]
async fn sse_sends_snapshot_then_batches_and_caps_clients() {
    let hub = LiveHub::new(fast_config());
    hub.publish(vec![live_block(block(1))]);
    let mut state = AppState::new(dead_pool(), None);
    state.live = Some(hub.clone());
    let app = routes::router(state);

    let get = || {
        Request::builder()
            .uri("/live/stream")
            .body(Body::empty())
            .unwrap()
    };
    let res = app.clone().oneshot(get()).await.unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(res.headers()["content-type"], "text/event-stream");
    assert_eq!(res.headers()["x-accel-buffering"], "no");
    let mut body = res.into_body();

    let ev = next_event(&mut body).await;
    assert!(ev.starts_with("event: snapshot\n"), "{ev}");
    assert!(ev.contains("\"blueScore\":\"93000001\""));

    hub.publish(vec![live_block(block(2)), live_block(block(3))]);
    let ev = next_event(&mut body).await;
    assert!(ev.starts_with("event: blocks\n"), "{ev}");
    let data: Value =
        serde_json::from_str(ev.trim_end().strip_prefix("event: blocks\ndata: ").unwrap()).unwrap();
    assert_eq!(data["blocks"].as_array().unwrap().len(), 2);

    // max_clients = 2: a second client fits, a third is refused.
    let second = app.clone().oneshot(get()).await.unwrap();
    assert_eq!(second.status(), 200);
    let third = app.clone().oneshot(get()).await.unwrap();
    assert_eq!(third.status(), 503);
    drop(second);
    drop(body);
}

#[tokio::test]
async fn live_endpoints_are_unavailable_without_a_node() {
    let app = routes::router(AppState::new(dead_pool(), None));
    assert_eq!(request(&app, "/live/recent").await.status, 503);
    assert_eq!(request(&app, "/live/stream").await.status, 503);
}

#[tokio::test]
async fn recent_returns_the_snapshot_as_json() {
    let hub = LiveHub::new(fast_config());
    hub.publish(vec![live_block(block(7))]);
    let mut state = AppState::new(dead_pool(), None);
    state.live = Some(hub);
    let app = routes::router(state);
    let r = request(&app, "/live/recent").await;
    assert_eq!(r.status, 200);
    let v: Value = serde_json::from_slice(&r.body).unwrap();
    assert_eq!(v["blocks"][0]["txs"][0]["to"], ADDR);
}
