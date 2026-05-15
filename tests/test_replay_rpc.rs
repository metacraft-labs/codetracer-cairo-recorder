//! M5 follow-up: live RPC + class-hash → Sierra resolution +
//! local re-execution integration tests.
//!
//! These tests pin the contract introduced by the M5 follow-up:
//!
//!  - [`StarknetRpcClient::trace_transaction`] performs a live JSON-RPC
//!    `starknet_traceTransaction` POST and parses the response into a
//!    [`TransactionTrace`] (test_trace_transaction_via_mock_server).
//!
//!  - [`StarknetRpcClient::get_class_at`] performs a live JSON-RPC
//!    `starknet_getClassAt` POST and parses the response into a
//!    [`SierraContractClass`] (test_get_class_at_via_mock_server).
//!
//!  - [`StarknetRpcClient::get_class_at`] surfaces JSON-RPC error
//!    envelopes as `eyre::Report` (test_get_class_at_rpc_error).
//!
//!  - [`StarknetRpcClient::get_class_at`] rejects Cairo 0 deprecated
//!    classes early (test_get_class_at_rejects_cairo0).
//!
//!  - [`find_entry_point`] resolves a hex selector against
//!    `entry_points_by_type` (test_find_entry_point_by_selector and
//!    test_find_entry_point_not_found).
//!
//!  - [`reexecute_entry_point`] surfaces a structured error when the
//!    requested selector is absent from the class
//!    (test_reexecute_entry_point_unknown_selector).
//!
//! Each test uses a hand-built [`std::net::TcpListener`] mock server
//! that handles a single canned JSON-RPC request — strictly local,
//! no network egress, no shared state between tests.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::thread;

use codetracer_cairo_recorder::starknet::{
    find_entry_point, reexecute_entry_point, write_replay_trace, SierraContractClass,
    StarknetRpcClient,
};

/// Canonical CTFS multi-stream container magic — matches
/// `tests/test_ctfs_audit.rs`.
const CTFS_MAGIC: [u8; 5] = [0xC0, 0xDE, 0x72, 0xAC, 0xE2];

// ---------------------------------------------------------------------------
// Mock JSON-RPC server
// ---------------------------------------------------------------------------

/// A one-shot JSON-RPC mock: binds to a random localhost port, accepts
/// exactly one POST, captures the request body, and replies with the
/// supplied JSON body.  Returns `(url, join_handle, request_body_rx)`.
///
/// The handle's `join().unwrap()` returns the full request body the
/// client sent — used by tests to assert the JSON-RPC envelope shape.
struct MockServerHandle {
    url: String,
    join: thread::JoinHandle<String>,
}

impl MockServerHandle {
    fn url(&self) -> &str {
        &self.url
    }

    fn join(self) -> String {
        self.join.join().expect("mock server thread panicked")
    }
}

fn spawn_mock_rpc(response_body: String) -> MockServerHandle {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind localhost");
    let port = listener.local_addr().expect("local_addr").port();
    let url = format!("http://127.0.0.1:{port}/");

    let join = thread::spawn(move || {
        let (mut stream, _addr) = listener.accept().expect("accept");
        // Parse HTTP request: read headers line-by-line until empty line,
        // then read Content-Length bytes.
        let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
        let mut content_length: usize = 0;
        let mut line = String::new();
        loop {
            line.clear();
            let n = reader.read_line(&mut line).expect("read header line");
            if n == 0 || line == "\r\n" || line == "\n" {
                break;
            }
            let lower = line.to_ascii_lowercase();
            if let Some(rest) = lower.strip_prefix("content-length:") {
                content_length = rest.trim().parse::<usize>().expect("parse content-length");
            }
        }
        let mut body = vec![0u8; content_length];
        reader.read_exact(&mut body).expect("read body");
        let body_str = String::from_utf8(body).expect("utf-8 body");

        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            response_body.len(),
            response_body,
        );
        stream
            .write_all(response.as_bytes())
            .expect("write response");
        stream.flush().expect("flush");
        body_str
    });

    MockServerHandle { url, join }
}

// ---------------------------------------------------------------------------
// trace_transaction tests
// ---------------------------------------------------------------------------

/// `StarknetRpcClient::trace_transaction` must POST a JSON-RPC envelope
/// to the configured `rpc_url` and parse the response into a
/// `TransactionTrace`.  This pins the live-RPC contract using a
/// localhost mock server that returns the same fixture body the
/// offline `--trace-file` path consumes.
#[test]
fn test_trace_transaction_via_mock_server() {
    // Re-use the mock_tx_trace.json fixture so both the live and
    // offline paths share the same canonical schema.
    let fixture_path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("test-programs/starknet/mock_tx_trace.json");
    let fixture_body = std::fs::read_to_string(&fixture_path).expect("read fixture");

    let server = spawn_mock_rpc(fixture_body.clone());
    let url = server.url().to_string();

    let client = StarknetRpcClient::new(&url);
    let tx_hash = "0x12345abc";
    let trace = client
        .trace_transaction(tx_hash)
        .expect("trace_transaction should succeed against mock server");

    // Pin the request envelope the client sent: must be a JSON-RPC 2.0
    // POST naming the right method with the tx hash as the single
    // positional param.
    let request_body = server.join();
    let request_json: serde_json::Value =
        serde_json::from_str(&request_body).expect("request body is JSON");
    assert_eq!(request_json["jsonrpc"], "2.0");
    assert_eq!(request_json["method"], "starknet_traceTransaction");
    assert_eq!(request_json["params"][0], tx_hash);
    assert_eq!(
        request_json["params"]
            .as_array()
            .expect("params array")
            .len(),
        1
    );

    // Pin the parsed trace shape: same assertions as the offline path
    // in `tests/test_ctfs_audit.rs::test_replay_writes_ctfs_bundle_from_tx_trace`.
    assert_eq!(trace.tx_type, "INVOKE");
    assert_eq!(
        trace.execute_invocation.contract_address,
        "0x049d36570d4e46f48e99674bd3fcc84644ddd6b96f7c741b1562b82f9e004dc7"
    );
    assert_eq!(trace.execute_invocation.calls.len(), 1);
}

/// End-to-end: a [`TransactionTrace`] fetched live via the mock server
/// drives [`write_replay_trace`] into a CTFS bundle whose magic-prefix
/// matches the canonical container shape.  This is the strict
/// integration test the M5 follow-up asks for: live fetch →
/// re-execution-aware write → on-disk `.ct` file.
#[test]
fn test_replay_pipeline_live_fetch_to_ctfs_bundle() {
    let fixture_path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("test-programs/starknet/mock_tx_trace.json");
    let fixture_body = std::fs::read_to_string(&fixture_path).expect("read fixture");

    let server = spawn_mock_rpc(fixture_body);
    let url = server.url().to_string();

    let client = StarknetRpcClient::new(&url);
    let trace = client
        .trace_transaction("0xfeed")
        .expect("trace_transaction should succeed");

    // Drive the trace-write half of the M5 pipeline against the
    // live-fetched trace.  The bundle must contain exactly one .ct
    // file whose magic prefix matches the CTFS contract.
    let tmp_dir = tempfile::tempdir().expect("tempdir");
    let out_dir = tmp_dir.path().join("replay-traces");
    std::fs::create_dir_all(&out_dir).unwrap();

    write_replay_trace("0xfeed", &trace, &out_dir).expect("write_replay_trace should succeed");

    let ct_files: Vec<_> = std::fs::read_dir(&out_dir)
        .expect("read out_dir")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "ct"))
        .collect();
    assert_eq!(ct_files.len(), 1);

    let container = std::fs::read(&ct_files[0]).expect("read .ct container");
    let prefix_len = 5;
    assert_eq!(container.len().min(prefix_len), prefix_len);
    assert_eq!(&container[..prefix_len], &CTFS_MAGIC);

    // Drain the mock server thread so the test owns the full request
    // round-trip cleanup.
    let _request_body = server.join();
}

// ---------------------------------------------------------------------------
// get_class_at tests
// ---------------------------------------------------------------------------

/// Build a minimal Sierra `ContractClass` JSON envelope suitable for
/// pinning the `starknet_getClassAt` parse path.  The Sierra program
/// itself is empty (zero felts) — sufficient because the JSON
/// deserialiser only validates the schema; downstream consumers
/// (`reexecute_entry_point`) get tested separately against a real
/// program.
fn minimal_sierra_class_json() -> String {
    serde_json::json!({
        "result": {
            "sierra_program": [],
            "contract_class_version": "0.1.0",
            "entry_points_by_type": {
                "EXTERNAL": [
                    {
                        "selector": "0x83afd3f4caedc6eebf44246fe54e38c95e3179a5ec9ea81740eca5b482d12e",
                        "function_idx": 0
                    }
                ],
                "L1_HANDLER": [],
                "CONSTRUCTOR": []
            },
            "abi": null
        },
        "jsonrpc": "2.0",
        "id": 1
    })
    .to_string()
}

/// `get_class_at` POSTs `starknet_getClassAt` with `[block_id, address]`
/// params and parses the response into a [`SierraContractClass`].
#[test]
fn test_get_class_at_via_mock_server() {
    let server = spawn_mock_rpc(minimal_sierra_class_json());
    let url = server.url().to_string();

    let client = StarknetRpcClient::new(&url);
    let block_id = serde_json::json!("latest");
    let contract = "0xabc";
    let class = client
        .get_class_at(&block_id, contract)
        .expect("get_class_at should succeed against mock server");

    let request_body = server.join();
    let request_json: serde_json::Value =
        serde_json::from_str(&request_body).expect("request body is JSON");
    assert_eq!(request_json["jsonrpc"], "2.0");
    assert_eq!(request_json["method"], "starknet_getClassAt");
    assert_eq!(request_json["params"][0], "latest");
    assert_eq!(request_json["params"][1], contract);
    assert_eq!(
        request_json["params"]
            .as_array()
            .expect("params array")
            .len(),
        2
    );

    // Pin the parsed class: one external entry point matching the
    // hex selector in the fixture.
    assert_eq!(class.contract_class_version, "0.1.0");
    assert_eq!(class.entry_points_by_type.external.len(), 1);
    assert_eq!(class.entry_points_by_type.l1_handler.len(), 0);
    assert_eq!(class.entry_points_by_type.constructor.len(), 0);
    assert_eq!(class.entry_points_by_type.external[0].function_idx, 0);
}

/// `get_class_at` must surface JSON-RPC error envelopes as
/// `eyre::Report` rather than a generic deserialise failure.  This
/// pins the upstream-error contract so a malformed contract address
/// surfaces as the node's `ContractNotFound` error code.
#[test]
fn test_get_class_at_rpc_error() {
    let error_body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "error": {
            "code": 20,
            "message": "Contract not found"
        }
    })
    .to_string();
    let server = spawn_mock_rpc(error_body);
    let url = server.url().to_string();

    let client = StarknetRpcClient::new(&url);
    let block_id = serde_json::json!("latest");
    let result = client.get_class_at(&block_id, "0xdead");
    let _request_body = server.join();

    let err = result.expect_err("get_class_at must propagate JSON-RPC errors");
    let msg = err.to_string();
    // Pin the exact error message: the JSON-RPC error envelope's
    // `error` object surfaces verbatim via `serde_json::Value`'s
    // Display impl (compact, key-sorted by insertion order).
    assert_eq!(
        msg,
        "starknet_getClassAt returned JSON-RPC error: {\"code\":20,\"message\":\"Contract not found\"}"
    );
}

/// `get_class_at` must reject Cairo 0 deprecated classes (which carry
/// a `program` field instead of `sierra_program`) early so the caller
/// gets a clear message rather than a confusing serde error.
#[test]
fn test_get_class_at_rejects_cairo0() {
    let cairo0_body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "result": {
            "program": "base64-encoded-deprecated-program",
            "entry_points_by_type": {}
        }
    })
    .to_string();
    let server = spawn_mock_rpc(cairo0_body);
    let url = server.url().to_string();

    let client = StarknetRpcClient::new(&url);
    let block_id = serde_json::json!("latest");
    let result = client.get_class_at(&block_id, "0xcafe");
    let _request_body = server.join();

    let err = result.expect_err("Cairo 0 classes must be rejected");
    let msg = err.to_string();
    assert_eq!(
        msg,
        "contract at 0xcafe is a Cairo 0 deprecated class — \
         the recorder's local re-execution path supports Sierra (Cairo 1+) only"
    );
}

// ---------------------------------------------------------------------------
// find_entry_point / reexecute_entry_point tests
// ---------------------------------------------------------------------------

fn build_test_class() -> SierraContractClass {
    // Re-use the same minimal class envelope used by the get_class_at
    // mock test — strip the JSON-RPC wrapper to get the bare class.
    let envelope: serde_json::Value =
        serde_json::from_str(&minimal_sierra_class_json()).expect("parse envelope");
    let result = envelope["result"].clone();
    serde_json::from_value::<SierraContractClass>(result).expect("parse class")
}

/// `find_entry_point` resolves a `0x`-prefixed hex selector by matching
/// against the `BigUint` `selector` field of `entry_points_by_type`.
#[test]
fn test_find_entry_point_by_selector() {
    let class = build_test_class();

    // The fixture's external entry point has this selector (value
    // matches the increase_balance selector in the snforge fixture
    // mock_trace.json).
    let selector = "0x83afd3f4caedc6eebf44246fe54e38c95e3179a5ec9ea81740eca5b482d12e";
    let ep = find_entry_point(&class, selector).expect("entry point should be resolvable");
    assert_eq!(ep.function_idx, 0);
}

/// `find_entry_point` returns a structured error when the requested
/// selector is absent from every entry-point group.
#[test]
fn test_find_entry_point_not_found() {
    let class = build_test_class();
    let result = find_entry_point(&class, "0xdeadbeef");
    let err = result.expect_err("missing selector must error");
    let msg = err.to_string();
    assert_eq!(
        msg,
        "no entry point with selector 0xdeadbeef found in class \
         (external: 1, l1_handler: 0, constructor: 0)"
    );
}

/// `reexecute_entry_point` propagates the `find_entry_point` error
/// when the selector is absent — the runner is never invoked.
///
/// We don't pin the runner-side success path here because it requires
/// a real Sierra program; building one synthetically is the M5
/// follow-up 2 task.  This negative path is still strict: the error
/// must originate from the entry-point lookup and must not surface
/// as a runner panic.
#[test]
fn test_reexecute_entry_point_unknown_selector() {
    let class = build_test_class();
    let result = reexecute_entry_point(&class, "0xdeadbeef", &[]);
    let err = result.expect_err("unknown selector must error");
    let msg = err.to_string();
    assert_eq!(
        msg,
        "no entry point with selector 0xdeadbeef found in class \
         (external: 1, l1_handler: 0, constructor: 0)"
    );
}
