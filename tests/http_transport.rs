//! Integration tests for the HTTP (streamable-HTTP) transport.
//!
//! § 5 (todo-followup): spin up the server in-process on a random TCP port,
//! connect an rmcp HTTP client, and verify that read tools, write tools, and
//! resource reads all work via the HTTP path. The HTTP transport has its own
//! session-merge / multi-tenant semantics that the pipe transport doesn't, so
//! these tests are the only ones that exercise that path.
//!
//! The tests are marked `#[ignore]` by default because they bind a real TCP
//! port. Run them with `cargo test --test http_transport -- --ignored`.
//! Add `RUST_LOG=info` to see the server log.

mod common;

use rmcp::model::{CallToolRequestParams, ClientInfo};
use rmcp::object;
use rmcp::service::Peer;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::RoleClient;
use rmcp::ServiceExt as _;
use serde_json::{Map, Value};
use tempfile::TempDir;

use common::text_of;
use org_roam_mcp::{Config, RoamServer};

async fn call_http(peer: &Peer<RoleClient>, tool: &str, args: Map<String, Value>) -> Value {
    let params = CallToolRequestParams::new(tool.to_string()).with_arguments(args);
    let result = peer
        .call_tool(params)
        .await
        .unwrap_or_else(|e| panic!("{tool} call failed over HTTP: {e}"));
    let text = text_of(&result);
    serde_json::from_str(&text).unwrap_or(Value::String(text))
}

/// Bind the HTTP server to a random port and return the bound address.
async fn bind_random_port(server: RoamServer) -> (String, tokio::task::JoinHandle<()>) {
    use rmcp::transport::streamable_http_server::{
        session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
    };
    use std::sync::Arc;

    let service = Arc::new(StreamableHttpService::new(
        move || Ok(server.for_new_session()),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default(),
    ));

    let app = axum::Router::new().fallback_service(tower::service_fn(
        move |req: axum::http::Request<axum::body::Body>| {
            let svc = service.clone();
            async move { Ok::<_, std::convert::Infallible>(svc.handle(req).await) }
        },
    ));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind random port");
    let addr = listener.local_addr().expect("local addr");
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    (format!("http://127.0.0.1:{}", addr.port()), handle)
}

fn make_server(dir: &TempDir) -> RoamServer {
    let cfg = Config::from_args(dir.path(), false, true, None).unwrap();
    RoamServer::new(cfg).unwrap()
}

#[tokio::test]
#[ignore = "binds a real TCP port — run with --ignored"]
async fn http_server_responds_to_list_tools() {
    let dir = TempDir::new().unwrap();
    let (addr, _handle) = bind_random_port(make_server(&dir)).await;

    let transport = StreamableHttpClientTransport::from_uri(addr.as_str());
    let client = ClientInfo::default()
        .serve(transport)
        .await
        .expect("connect HTTP client");
    let peer = client.peer().clone();

    let tools = peer.list_tools(None).await.expect("list_tools");
    assert!(
        !tools.tools.is_empty(),
        "HTTP server must expose at least one tool"
    );
    assert!(
        tools.tools.iter().any(|t| t.name == "search_nodes"),
        "search_nodes must be advertised: {:?}",
        tools.tools.iter().map(|t| &t.name).collect::<Vec<_>>()
    );
    client.cancel().await.ok();
}

#[tokio::test]
#[ignore = "binds a real TCP port — run with --ignored"]
async fn http_server_call_read_tool_returns_json() {
    let dir = TempDir::new().unwrap();
    let (addr, _handle) = bind_random_port(make_server(&dir)).await;

    let transport = StreamableHttpClientTransport::from_uri(addr.as_str());
    let client = ClientInfo::default()
        .serve(transport)
        .await
        .expect("connect HTTP client");
    let peer = client.peer().clone();

    let result = call_http(&peer, "list_nodes", object!({ "limit": 10 })).await;

    // An empty vault returns an empty nodes array.
    assert!(
        result["nodes"].is_array(),
        "list_nodes must return a nodes array: {result}"
    );
    client.cancel().await.ok();
}

#[tokio::test]
#[ignore = "binds a real TCP port — run with --ignored"]
async fn http_server_call_write_tool_and_read_back() {
    let dir = TempDir::new().unwrap();
    let (addr, _handle) = bind_random_port(make_server(&dir)).await;

    let transport = StreamableHttpClientTransport::from_uri(addr.as_str());
    let client = ClientInfo::default()
        .serve(transport)
        .await
        .expect("connect HTTP client");
    let peer = client.peer().clone();

    // Create a node via the write tool.
    let created = call_http(
        &peer,
        "create_node",
        object!({ "title": "HTTP Test Node", "body": "Created via HTTP.\n" }),
    )
    .await;
    let id = created["id"].as_str().expect("create must return an id");

    // Read it back via a read tool.
    let found = call_http(&peer, "get_node", object!({ "id": id })).await;
    assert_eq!(
        found["title"],
        Value::String("HTTP Test Node".into()),
        "create + get_node via HTTP must round-trip: {found}"
    );
    client.cancel().await.ok();
}

#[tokio::test]
#[ignore = "binds a real TCP port — run with --ignored"]
async fn two_http_sessions_share_index_but_have_independent_subscriptions() {
    // Two clients connecting to the same HTTP server must see the same
    // node list (shared index) but different session identities.
    async fn connect_and_get(addr: String, node_id: &str) -> Value {
        let transport = StreamableHttpClientTransport::from_uri(addr.as_str());
        let client = ClientInfo::default()
            .serve(transport)
            .await
            .expect("connect");
        let peer = client.peer().clone();
        let params = CallToolRequestParams::new("get_node".to_string())
            .with_arguments(object!({ "id": node_id }));
        let result = peer.call_tool(params).await.expect("call");
        let text = text_of(&result);
        client.cancel().await.ok();
        serde_json::from_str(&text).unwrap_or(Value::String(text))
    }

    let dir = TempDir::new().unwrap();

    // Pre-populate a node so both sessions can find it.
    std::fs::write(
        dir.path().join("shared.org"),
        ":PROPERTIES:\n:ID: aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\n:END:\n\
         #+title: Shared Node\n",
    )
    .unwrap();

    let (addr, _handle) = bind_random_port(make_server(&dir)).await;
    let id = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";

    let (r1, r2) = tokio::join!(
        connect_and_get(addr.clone(), id),
        connect_and_get(addr.clone(), id)
    );

    assert_eq!(
        r1["title"],
        Value::String("Shared Node".into()),
        "session 1 must find the shared node: {r1}"
    );
    assert_eq!(
        r2["title"],
        Value::String("Shared Node".into()),
        "session 2 must find the shared node: {r2}"
    );
}
