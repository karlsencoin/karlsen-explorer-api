//! Node-first path of `GET /blocks/{hash}` against a mock karlsend.
//!
//! The mock serves the captured legacy responses (tests/fixtures/golden-*.json)
//! converted back into karlsend's gRPC messages. Since legacy `rest-v2` built those
//! responses from the node, our output must reproduce them exactly. The database
//! pool never connects, which also proves the node path does not touch it.

#[allow(dead_code)]
mod common;

use std::{collections::HashMap, net::SocketAddr, sync::Arc, time::Duration};

use common::{dead_pool, request};
use karlsen_explorer_api::{
    node::{
        proto::{
            karlsend_request::Payload as Req,
            karlsend_response::Payload as Resp,
            rpc_server::{Rpc, RpcServer},
            GetBlockResponseMessage, GetCurrentBlockColorResponseMessage, KarlsendRequest,
            KarlsendResponse, RpcBlock, RpcBlockHeader, RpcBlockLevelParents, RpcBlockVerboseData,
            RpcError, RpcOutpoint, RpcScriptPublicKey, RpcTransaction, RpcTransactionInput,
            RpcTransactionOutput, RpcTransactionOutputVerboseData, RpcTransactionVerboseData,
        },
        NodeClient,
    },
    routes, AppState,
};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_stream::wrappers::{ReceiverStream, TcpListenerStream};
use tonic::{Request, Response, Status, Streaming};

const COINBASE_ONLY: &str = include_str!("fixtures/golden-block.json");
const WITH_TXS: &str = include_str!("fixtures/golden-block-with-txs.json");
const WITH_COLOR: &str = include_str!("fixtures/golden-block-color.json");
const WITHOUT_TXS: &str = include_str!("fixtures/golden-block-notx.json");

// ---- golden JSON -> karlsend gRPC messages ----

fn s(v: &Value) -> String {
    v.as_str().unwrap_or_default().to_string()
}
fn strs(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|a| a.iter().map(s).collect())
        .unwrap_or_default()
}
fn n(v: &Value) -> u64 {
    v.as_u64()
        .or_else(|| v.as_str().and_then(|x| x.parse().ok()))
        .unwrap_or_default()
}

/// The difficulty literal as written in the capture. Parsed with `str::parse`, which
/// is exact, unlike serde_json's default float parser.
fn difficulty_literal(raw: &str) -> &str {
    raw.split("\"difficulty\":")
        .nth(1)
        .and_then(|rest| rest.split(',').next())
        .expect("difficulty field")
}

fn rpc_block_from_golden(raw: &str) -> RpcBlock {
    let g: Value = serde_json::from_str(raw).unwrap();
    let h = &g["header"];
    let vd = &g["verboseData"];
    RpcBlock {
        header: Some(RpcBlockHeader {
            version: n(&h["version"]) as u32,
            parents: h["parents"]
                .as_array()
                .unwrap()
                .iter()
                .map(|p| RpcBlockLevelParents {
                    parent_hashes: strs(&p["parentHashes"]),
                })
                .collect(),
            hash_merkle_root: s(&h["hashMerkleRoot"]),
            accepted_id_merkle_root: s(&h["acceptedIdMerkleRoot"]),
            utxo_commitment: s(&h["utxoCommitment"]),
            timestamp: n(&h["timestamp"]) as i64,
            bits: n(&h["bits"]) as u32,
            nonce: n(&h["nonce"]),
            daa_score: n(&h["daaScore"]),
            // The node sends blue work zero-padded; the API must trim it.
            blue_work: format!("{:0>48}", s(&h["blueWork"])),
            pruning_point: s(&h["pruningPoint"]),
            blue_score: n(&h["blueScore"]),
        }),
        transactions: g["transactions"]
            .as_array()
            .map(|txs| {
                txs.iter()
                    .map(|tx| RpcTransaction {
                        version: n(&tx["version"]) as u32,
                        inputs: tx["inputs"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .map(|i| RpcTransactionInput {
                                previous_outpoint: Some(RpcOutpoint {
                                    transaction_id: s(&i["previousOutpoint"]["transactionId"]),
                                    index: n(&i["previousOutpoint"]["index"]) as u32,
                                }),
                                signature_script: s(&i["signatureScript"]),
                                sequence: n(&i["sequence"]),
                                sig_op_count: n(&i["sigOpCount"]) as u32,
                                verbose_data: None,
                            })
                            .collect(),
                        outputs: tx["outputs"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .map(|o| RpcTransactionOutput {
                                amount: n(&o["amount"]),
                                script_public_key: Some(RpcScriptPublicKey {
                                    version: 0,
                                    script_public_key: s(&o["scriptPublicKey"]["scriptPublicKey"]),
                                }),
                                verbose_data: Some(RpcTransactionOutputVerboseData {
                                    script_public_key_type: s(
                                        &o["verboseData"]["scriptPublicKeyType"]
                                    ),
                                    script_public_key_address: s(
                                        &o["verboseData"]["scriptPublicKeyAddress"]
                                    ),
                                }),
                            })
                            .collect(),
                        lock_time: n(&tx["lockTime"]),
                        subnetwork_id: s(&tx["subnetworkId"]),
                        gas: n(&tx["gas"]),
                        payload: s(&tx["payload"]),
                        verbose_data: Some(RpcTransactionVerboseData {
                            transaction_id: s(&tx["verboseData"]["transactionId"]),
                            hash: s(&tx["verboseData"]["hash"]),
                            compute_mass: n(&tx["verboseData"]["computeMass"]),
                            block_hash: s(&tx["verboseData"]["blockHash"]),
                            block_time: n(&tx["verboseData"]["blockTime"]),
                        }),
                        mass: n(&tx["mass"]),
                    })
                    .collect()
            })
            .unwrap_or_default(),
        verbose_data: Some(RpcBlockVerboseData {
            hash: s(&vd["hash"]),
            difficulty: difficulty_literal(raw).parse().unwrap(),
            selected_parent_hash: s(&vd["selectedParentHash"]),
            transaction_ids: strs(&vd["transactionIds"]),
            is_header_only: false,
            blue_score: n(&vd["blueScore"]),
            children_hashes: strs(&vd["childrenHashes"]),
            merge_set_blues_hashes: strs(&vd["mergeSetBluesHashes"]),
            merge_set_reds_hashes: strs(&vd["mergeSetRedsHashes"]),
            is_chain_block: vd["isChainBlock"].as_bool().unwrap(),
        }),
    }
}

// ---- mock karlsend ----

#[derive(Default)]
struct MockNode {
    blocks: HashMap<String, RpcBlock>,
    colors: HashMap<String, bool>,
}

impl MockNode {
    fn reply(&self, req: Req) -> Resp {
        match req {
            Req::GetBlockRequest(r) => {
                let block = self.blocks.get(&r.hash).cloned().map(|mut b| {
                    if !r.include_transactions {
                        b.transactions.clear();
                    }
                    b
                });
                let error = block.is_none().then(|| RpcError {
                    message: format!("Block {} not found", r.hash),
                });
                Resp::GetBlockResponse(GetBlockResponseMessage { block, error })
            }
            Req::GetCurrentBlockColorRequest(r) => {
                let blue = self.colors.get(&r.hash).copied();
                Resp::GetCurrentBlockColorResponse(GetCurrentBlockColorResponseMessage {
                    blue: blue.unwrap_or_default(),
                    error: blue.is_none().then(|| RpcError {
                        message: "color unknown".into(),
                    }),
                })
            }
            other => panic!("unexpected request {other:?}"),
        }
    }
}

struct MockService(Arc<MockNode>);

#[tonic::async_trait]
impl Rpc for MockService {
    type MessageStreamStream = ReceiverStream<Result<KarlsendResponse, Status>>;

    async fn message_stream(
        &self,
        request: Request<Streaming<KarlsendRequest>>,
    ) -> Result<Response<Self::MessageStreamStream>, Status> {
        let mut inbound = request.into_inner();
        let (tx, rx) = mpsc::channel(4);
        let node = self.0.clone();
        tokio::spawn(async move {
            while let Ok(Some(req)) = inbound.message().await {
                let payload = node.reply(req.payload.expect("payload"));
                let reply = KarlsendResponse {
                    id: req.id,
                    payload: Some(payload),
                };
                if tx.send(Ok(reply)).await.is_err() {
                    break;
                }
            }
        });
        Ok(Response::new(ReceiverStream::new(rx)))
    }
}

async fn start_mock(node: MockNode) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let service = RpcServer::new(MockService(Arc::new(node)));
    tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(service)
            .serve_with_incoming(TcpListenerStream::new(listener))
            .await
            .unwrap();
    });
    addr
}

async fn app_with_mock(node: MockNode) -> axum::Router {
    let addr = start_mock(node).await;
    let client = NodeClient::new(&format!("http://{addr}"), Duration::from_secs(5)).unwrap();
    routes::router(AppState {
        db: dead_pool(),
        node: Some(client),
    })
}

fn golden_node() -> MockNode {
    let mut node = MockNode::default();
    for raw in [COINBASE_ONLY, WITH_TXS] {
        let b = rpc_block_from_golden(raw);
        node.blocks
            .insert(b.verbose_data.as_ref().unwrap().hash.clone(), b);
    }
    node
}

fn hash_of(raw: &str) -> String {
    serde_json::from_str::<Value>(raw).unwrap()["verboseData"]["hash"]
        .as_str()
        .unwrap()
        .to_string()
}

/// Our response must equal the capture, except `minerAddress` / `minerInfo`, which
/// legacy leaves null and we decode from the coinbase payload.
fn assert_matches_capture(body: &[u8], raw: &str) {
    let mut got: Value = serde_json::from_slice(body).unwrap();
    let mut want: Value = serde_json::from_str(raw).unwrap();

    let body_text = std::str::from_utf8(body).unwrap();
    assert_eq!(difficulty_literal(body_text), difficulty_literal(raw));
    got["verboseData"]["difficulty"] = Value::Null;
    want["verboseData"]["difficulty"] = Value::Null;

    if want["transactions"].is_array() {
        assert_eq!(
            got["extra"]["minerAddress"],
            want["transactions"][0]["outputs"][0]["verboseData"]["scriptPublicKeyAddress"]
        );
        assert!(got["extra"]["minerInfo"]
            .as_str()
            .is_some_and(|i| i.contains("WoolyPooly")));
        want["extra"]["minerAddress"] = got["extra"]["minerAddress"].clone();
        want["extra"]["minerInfo"] = got["extra"]["minerInfo"].clone();
    }
    assert_eq!(got, want);
}

#[tokio::test]
async fn node_block_matches_every_capture() {
    let app = app_with_mock(golden_node()).await;
    let cases = [
        (COINBASE_ONLY, "includeTransactions=true"),
        (WITH_TXS, "includeTransactions=true"),
        (WITH_COLOR, "includeTransactions=true&includeColor=true"),
        (WITHOUT_TXS, "includeTransactions=false"),
    ];
    for (raw, query) in cases {
        let r = request(&app, &format!("/blocks/{}?{query}", hash_of(raw))).await;
        assert_eq!(r.status, 200, "{query}: {}", r.json());
        assert_eq!(r.data_source(), None, "served from the node");
        assert_matches_capture(&r.body, raw);
    }
}

#[tokio::test]
async fn node_values_that_the_database_lacks() {
    let app = app_with_mock(golden_node()).await;
    let v = request(&app, &format!("/blocks/{}", hash_of(WITH_TXS)))
        .await
        .json();
    assert_eq!(v["header"]["parents"].as_array().unwrap().len(), 41);
    assert_eq!(v["transactions"][1]["mass"], 172);
    assert_eq!(v["transactions"][1]["verboseData"]["computeMass"], 2036);
    // Node order, not sorted: the capture lists 9cce... before 9ba1...
    assert_eq!(
        v["verboseData"]["childrenHashes"][0],
        "9ccea35000dbba399bf660852f99eedceb979b143e33a75b7094f527c485ef66"
    );
}

#[tokio::test]
async fn non_chain_block_color_comes_from_the_node() {
    let mut red = rpc_block_from_golden(WITH_TXS);
    let vd = red.verbose_data.as_mut().unwrap();
    vd.hash = "ab".repeat(32);
    vd.is_chain_block = false;
    let mut unknown = red.clone();
    unknown.verbose_data.as_mut().unwrap().hash = "cd".repeat(32);

    let mut node = MockNode::default();
    node.blocks.insert("ab".repeat(32), red);
    node.blocks.insert("cd".repeat(32), unknown);
    node.colors.insert("ab".repeat(32), false);
    let app = app_with_mock(node).await;

    let color = |id: String| {
        let app = app.clone();
        async move {
            request(&app, &format!("/blocks/{id}?includeColor=true"))
                .await
                .json()["extra"]["color"]
                .clone()
        }
    };
    assert_eq!(color("ab".repeat(32)).await, "red");
    // The node cannot tell yet: null, not an error.
    assert_eq!(color("cd".repeat(32)).await, Value::Null);
}

#[tokio::test]
async fn block_unknown_to_node_falls_back_and_reports_database_outage() {
    // Not in the node, and the database is down: a real 503, never a fake 404.
    let app = app_with_mock(golden_node()).await;
    let r = request(&app, &format!("/blocks/{}", "ee".repeat(32))).await;
    assert_eq!(r.status, 503);
    assert_eq!(r.json()["detail"], "Service temporarily unavailable");
}
