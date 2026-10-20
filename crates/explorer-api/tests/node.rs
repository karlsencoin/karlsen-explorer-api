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
            GetBalanceByAddressResponseMessage, GetBalancesByAddressesResponseMessage,
            GetBlockDagInfoResponseMessage, GetBlockResponseMessage, GetCoinSupplyResponseMessage,
            GetCurrentBlockColorResponseMessage, GetInfoResponseMessage,
            GetUtxosByAddressesResponseMessage, KarlsendRequest, KarlsendResponse,
            RpcBalancesByAddressesEntry, RpcBlock, RpcBlockHeader, RpcBlockLevelParents,
            RpcBlockVerboseData, RpcError, RpcOutpoint, RpcScriptPublicKey, RpcTransaction,
            RpcTransactionInput, RpcTransactionOutput, RpcTransactionOutputVerboseData,
            RpcTransactionVerboseData, RpcUtxoEntry, RpcUtxosByAddressesEntry,
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
    balances: HashMap<String, u64>,
    utxos: Vec<RpcUtxosByAddressesEntry>,
    /// Simulates karlsend without --utxoindex.
    no_utxoindex: bool,
    dag_info: Option<GetBlockDagInfoResponseMessage>,
    coin_supply: Option<GetCoinSupplyResponseMessage>,
    info: Option<GetInfoResponseMessage>,
}

impl MockNode {
    fn index_error(&self) -> Option<RpcError> {
        self.no_utxoindex.then(|| RpcError {
            message: "Method unavailable. Run the node with the --utxoindex argument.".into(),
        })
    }
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
            Req::GetBalanceByAddressRequest(r) => {
                Resp::GetBalanceByAddressResponse(GetBalanceByAddressResponseMessage {
                    balance: self.balances.get(&r.address).copied().unwrap_or_default(),
                    error: self.index_error(),
                })
            }
            Req::GetBalancesByAddressesRequest(r) => {
                Resp::GetBalancesByAddressesResponse(GetBalancesByAddressesResponseMessage {
                    entries: r
                        .addresses
                        .iter()
                        .map(|a| RpcBalancesByAddressesEntry {
                            address: a.clone(),
                            balance: self.balances.get(a).copied().unwrap_or_default(),
                            error: None,
                        })
                        .collect(),
                    error: self.index_error(),
                })
            }
            Req::GetUtxosByAddressesRequest(r) => {
                Resp::GetUtxosByAddressesResponse(GetUtxosByAddressesResponseMessage {
                    entries: self
                        .utxos
                        .iter()
                        .filter(|u| r.addresses.contains(&u.address))
                        .cloned()
                        .collect(),
                    error: self.index_error(),
                })
            }
            Req::GetBlockDagInfoRequest(_) => {
                Resp::GetBlockDagInfoResponse(self.dag_info.clone().expect("dag_info set"))
            }
            Req::GetCoinSupplyRequest(_) => {
                Resp::GetCoinSupplyResponse(self.coin_supply.clone().expect("coin_supply set"))
            }
            Req::GetInfoRequest(_) => Resp::GetInfoResponse(self.info.clone().expect("info set")),
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
    routes::router(AppState::new(dead_pool(), Some(client)))
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

// ---- addresses ----

const BALANCE: &str = include_str!("fixtures/golden-address-balance.json");
const BALANCES: &str = include_str!("fixtures/golden-addresses-balances.json");
const UTXO_ENTRY: &str = include_str!("fixtures/golden-address-utxo-entry.json");
const A1: &str = "karlsen:qrc9rgx9x8gyekvrd7533hj3tl0cxd5r9ggxkg5l4a6xgllethrtuvp48fc3n";
const A2: &str = "karlsen:qzsflg24vlkuhqzgjm2v39rkmhjg8e3qwtaylccpxqu97n72vx0s7faphh8a7";

fn address_node() -> MockNode {
    let mut node = MockNode::default();
    for e in serde_json::from_str::<Vec<Value>>(BALANCES).unwrap() {
        node.balances
            .insert(s(&e["address"]), e["balance"].as_u64().unwrap());
    }
    let u: Value = serde_json::from_str(UTXO_ENTRY).unwrap();
    node.utxos.push(RpcUtxosByAddressesEntry {
        address: s(&u["address"]),
        outpoint: Some(RpcOutpoint {
            transaction_id: s(&u["outpoint"]["transactionId"]),
            index: n(&u["outpoint"]["index"]) as u32,
        }),
        utxo_entry: Some(RpcUtxoEntry {
            amount: n(&u["utxoEntry"]["amount"]),
            script_public_key: Some(RpcScriptPublicKey {
                version: 0,
                script_public_key: s(&u["utxoEntry"]["scriptPublicKey"]["scriptPublicKey"]),
            }),
            block_daa_score: n(&u["utxoEntry"]["blockDaaScore"]),
            is_coinbase: u["utxoEntry"]["isCoinbase"].as_bool().unwrap(),
        }),
    });
    // A UTXO of another address must not leak into a single-address answer.
    let mut other = node.utxos[0].clone();
    other.address = A2.to_string();
    node.utxos.push(other);
    node
}

async fn post(app: &axum::Router, uri: &str, body: &str) -> common::Reply {
    use axum::{body::Body, http::Request};
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    let resp = app
        .clone()
        .oneshot(
            Request::post(uri)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    common::Reply {
        status: resp.status().as_u16(),
        headers: resp.headers().clone(),
        body: resp
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    }
}

#[tokio::test]
async fn address_responses_are_byte_identical_to_captures() {
    let app = app_with_mock(address_node()).await;

    let r = request(&app, &format!("/addresses/{A1}/balance")).await;
    assert_eq!(
        (r.status, std::str::from_utf8(&r.body).unwrap()),
        (200, BALANCE)
    );

    let body = format!(r#"{{"addresses":["{A1}","{A2}"]}}"#);
    let r = post(&app, "/addresses/balances", &body).await;
    assert_eq!(
        (r.status, std::str::from_utf8(&r.body).unwrap()),
        (200, BALANCES)
    );

    let r = request(&app, &format!("/addresses/{A1}/utxos")).await;
    assert_eq!(r.status, 200);
    assert_eq!(
        std::str::from_utf8(&r.body).unwrap(),
        format!("[{UTXO_ENTRY}]"),
        "only the requested address, in the legacy shape"
    );

    let r = post(
        &app,
        "/addresses/utxos",
        &format!(r#"{{"addresses":["{A1}","{A2}"]}}"#),
    )
    .await;
    assert_eq!(r.status, 200);
    assert_eq!(r.json().as_array().unwrap().len(), 2);

    let r = post(&app, "/addresses/balances", r#"{"addresses":[]}"#).await;
    assert_eq!((r.status, r.json()), (200, serde_json::json!([])));
}

#[tokio::test]
async fn invalid_addresses_are_rejected_not_answered_with_zero() {
    let app = app_with_mock(address_node()).await;
    // Legacy: 422 for a malformed address (same here).
    let r = request(&app, "/addresses/karlsen:qqqqinvalid/balance").await;
    assert_eq!(r.status, 422);
    // Legacy answers a bad checksum with 200 and balance 0, so a typo looks like an
    // empty wallet. Here it is a 422.
    let bad_checksum = format!("{}x", &A1[..A1.len() - 1]);
    let r = request(&app, &format!("/addresses/{bad_checksum}/balance")).await;
    assert_eq!(r.status, 422);
    assert_eq!(
        r.json()["detail"],
        format!("Invalid address: {bad_checksum}")
    );
    // One bad address fails the whole batch.
    let body = format!(r#"{{"addresses":["{A1}","{bad_checksum}"]}}"#);
    assert_eq!(post(&app, "/addresses/balances", &body).await.status, 422);
}

#[tokio::test]
async fn node_failures_are_503_never_zero() {
    let mut node = address_node();
    node.no_utxoindex = true;
    let app = app_with_mock(node).await;
    for uri in [
        format!("/addresses/{A1}/balance"),
        format!("/addresses/{A1}/utxos"),
    ] {
        let r = request(&app, &uri).await;
        assert_eq!(r.status, 503, "{uri}");
        assert_eq!(r.json()["detail"], "Service temporarily unavailable");
    }
    let body = format!(r#"{{"addresses":["{A1}"]}}"#);
    assert_eq!(post(&app, "/addresses/balances", &body).await.status, 503);

    // No node configured at all.
    let app = routes::router(AppState::new(dead_pool(), None));
    assert_eq!(
        request(&app, &format!("/addresses/{A1}/balance"))
            .await
            .status,
        503
    );
}

// ---- /info ----

const INFO_BLOCKDAG: &str = include_str!("fixtures/golden-info-blockdag.json");

fn info_node() -> MockNode {
    let g: Value = serde_json::from_str(INFO_BLOCKDAG).unwrap();
    let supply: Value =
        serde_json::from_str(include_str!("fixtures/golden-info-coinsupply.json")).unwrap();
    let k: Value =
        serde_json::from_str(include_str!("fixtures/golden-info-karlsend.json")).unwrap();
    MockNode {
        dag_info: Some(GetBlockDagInfoResponseMessage {
            // The node may report the bare name; the API must prefix it.
            network_name: "mainnet".into(),
            block_count: n(&g["blockCount"]),
            header_count: n(&g["headerCount"]),
            tip_hashes: strs(&g["tipHashes"]),
            difficulty: difficulty_literal(INFO_BLOCKDAG).parse().unwrap(),
            past_median_time: n(&g["pastMedianTime"]) as i64,
            virtual_parent_hashes: strs(&g["virtualParentHashes"]),
            pruning_point_hash: s(&g["pruningPointHash"]),
            virtual_daa_score: n(&g["virtualDaaScore"]),
            sink: s(&g["sink"]),
            error: None,
        }),
        coin_supply: Some(GetCoinSupplyResponseMessage {
            max_sompi: 0,
            circulating_sompi: n(&supply["circulatingSupply"]),
            error: None,
        }),
        info: Some(GetInfoResponseMessage {
            p2p_id: "test-p2p-id".into(),
            mempool_size: n(&k["mempoolSize"]),
            server_version: s(&k["serverVersion"]),
            is_utxo_indexed: true,
            is_synced: true,
            has_notify_command: true,
            has_message_id: true,
            error: None,
        }),
        ..MockNode::default()
    }
}

fn text(r: &common::Reply) -> &str {
    std::str::from_utf8(&r.body).unwrap()
}

#[tokio::test]
async fn info_answers_are_byte_identical_to_production() {
    let app = app_with_mock(info_node()).await;
    let cases = [
        ("/info/blockdag", INFO_BLOCKDAG),
        ("/info/network", INFO_BLOCKDAG),
        (
            "/info/hashrate",
            include_str!("fixtures/golden-info-hashrate.json"),
        ),
        (
            "/info/hashrate?stringOnly=true",
            include_str!("fixtures/golden-info-hashrate-string.json"),
        ),
        (
            "/info/coinsupply",
            include_str!("fixtures/golden-info-coinsupply.json"),
        ),
        (
            "/info/coinsupply/circulating",
            include_str!("fixtures/golden-info-circulating.txt"),
        ),
        (
            "/info/coinsupply/total",
            include_str!("fixtures/golden-info-circulating.txt"),
        ),
        (
            "/info/blockreward",
            include_str!("fixtures/golden-info-blockreward.json"),
        ),
    ];
    for (uri, want) in cases {
        let r = request(&app, uri).await;
        assert_eq!(r.status, 200, "{uri}");
        assert_eq!(text(&r), want, "{uri}");
    }
    let r = request(&app, "/info/coinsupply/circulating?in_billion=true").await;
    assert_eq!(text(&r), "3.13");
    let r = request(&app, "/info/blockreward?stringOnly=true").await;
    assert_eq!(text(&r), "\"19.51\"");
}

#[tokio::test]
async fn halving_and_karlsend_follow_production_shape() {
    let app = app_with_mock(info_node()).await;
    let want: Value =
        serde_json::from_str(include_str!("fixtures/golden-info-halving.json")).unwrap();
    let got = request(&app, "/info/halving").await.json();
    // The timestamp is "now + remaining time", so only the amount is fixed.
    assert_eq!(got["nextHalvingAmount"], want["nextHalvingAmount"]);
    let keys = |v: &Value| v.as_object().unwrap().keys().cloned().collect::<Vec<_>>();
    assert_eq!(keys(&got), keys(&want));
    let ts = got["nextHalvingTimestamp"].as_i64().unwrap();
    assert!(got["nextHalvingDate"].as_str().unwrap().ends_with(" UTC"));
    let r = request(&app, "/info/halving?field=nextHalvingAmount").await;
    assert_eq!(text(&r), "18.97285061");
    let r = request(&app, "/info/halving?field=nextHalvingTimestamp").await;
    let plain: f64 = text(&r).parse().unwrap();
    assert!((plain - ts as f64).abs() < 5.0);

    let got = request(&app, "/info/karlsend").await.json();
    let mut want: Value =
        serde_json::from_str(include_str!("fixtures/golden-info-karlsend.json")).unwrap();
    // Production hashes its own P2P id; the mock's id is "test-p2p-id".
    want["p2pIdHashed"] = "e4ef16763cddf835522936c34598e73c49a16a66a8944319ac4c2fe9a682c265".into();
    assert_eq!(got, want);
}

// ---- /info/price, /info/market-data ----

/// NonKYC ticker values behind the production capture (numbers as strings, as
/// NonKYC sends them).
const TICKER: &str = r#"{"ticker_id":"KLS_USDT","last_price":"0.000083549","high":"0.00015","low":"0.000069",
"bid":"0.000083244","ask":"0.00008355","base_volume":"13718051.1385","usd_volume_est":"1390.75",
"change_percent":"-30.35","previous_day_price":"0.000119966"}"#;

async fn start_ticker(body: &'static str, status: u16) -> String {
    use axum::{http::StatusCode, routing::get, Router};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = Router::new().route(
        "/ticker",
        get(move || async move { (StatusCode::from_u16(status).unwrap(), body) }),
    );
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}/ticker")
}

async fn app_with_market(ticker_url: &str) -> axum::Router {
    let addr = start_mock(info_node()).await;
    let client = NodeClient::new(&format!("http://{addr}"), Duration::from_secs(5)).unwrap();
    let mut state = AppState::new(dead_pool(), Some(client));
    state.market = karlsen_explorer_api::market::MarketFeed::new(ticker_url);
    routes::router(state)
}

#[tokio::test]
async fn price_and_market_data_are_byte_identical_to_production() {
    let app = app_with_market(&start_ticker(TICKER, 200).await).await;
    let r = request(&app, "/info/price").await;
    assert_eq!(text(&r), include_str!("fixtures/golden-info-price.json"));
    let r = request(&app, "/info/price?stringOnly=true").await;
    assert_eq!(text(&r), "8.3549e-05");
    let r = request(&app, "/info/market-data").await;
    assert_eq!(r.status, 200, "{}", text(&r));
    assert_eq!(
        text(&r),
        include_str!("fixtures/golden-info-market-data.json")
    );
}

#[tokio::test]
async fn price_source_down_is_503_not_zero() {
    // Legacy answered {"price": 0} here.
    let app = app_with_market(&start_ticker("upstream error", 502).await).await;
    for uri in ["/info/price", "/info/market-data"] {
        let r = request(&app, uri).await;
        assert_eq!(r.status, 503, "{uri}");
        assert_eq!(r.json()["detail"], "Service temporarily unavailable");
    }
}

#[tokio::test]
async fn utxos_paged_newest_first_with_totals() {
    let mut node = address_node();
    let base = node.utxos[0].clone();
    // Three more UTXOs of A1 with distinct DAA scores and amounts.
    for (i, daa) in [(1u32, 500u64), (2, 900), (3, 100)] {
        let mut u = base.clone();
        u.outpoint.as_mut().unwrap().index = i;
        let e = u.utxo_entry.as_mut().unwrap();
        e.block_daa_score = daa;
        e.amount = 1_000 * u64::from(i);
        node.utxos.push(u);
    }
    let base_amount = u128::from(base.utxo_entry.as_ref().unwrap().amount);
    let base_daa = base.utxo_entry.as_ref().unwrap().block_daa_score;
    let app = app_with_mock(node).await;

    let r = request(&app, &format!("/addresses/{A1}/utxos?limit=2")).await;
    assert_eq!(r.status, 200);
    assert_eq!(r.headers["x-total-count"], "4");
    assert_eq!(
        r.headers["x-total-amount"],
        (base_amount + 6_000).to_string().as_str()
    );
    let page: Vec<Value> = serde_json::from_slice(&r.body).unwrap();
    assert_eq!(page.len(), 2);
    let mut daas = [base_daa, 500, 900, 100];
    daas.sort_unstable_by(|a, b| b.cmp(a));
    assert_eq!(page[0]["utxoEntry"]["blockDaaScore"], daas[0].to_string());
    assert_eq!(page[1]["utxoEntry"]["blockDaaScore"], daas[1].to_string());

    let r = request(&app, &format!("/addresses/{A1}/utxos?limit=2&offset=3")).await;
    let page: Vec<Value> = serde_json::from_slice(&r.body).unwrap();
    assert_eq!(page.len(), 1);
    assert_eq!(page[0]["utxoEntry"]["blockDaaScore"], daas[3].to_string());

    assert_eq!(
        request(&app, &format!("/addresses/{A1}/utxos?limit=0"))
            .await
            .status,
        422
    );
    assert_eq!(
        request(&app, &format!("/addresses/{A1}/utxos?limit=501"))
            .await
            .status,
        422
    );
}
