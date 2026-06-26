//! Shared harness for the in-process MCP integration tests: serve a
//! `RoamServer` over duplex pipes and drive it with the rmcp client.

use rmcp::service::ServiceExt;

use org_roam_mcp::RoamServer;

/// Collect the text content of a tool result.
// Each test binary compiles this module independently; not all of them
// use every helper.
#[allow(dead_code)]
pub fn text_of(call: &rmcp::model::CallToolResult) -> String {
    call.content
        .iter()
        .filter_map(|c| c.as_text().map(|t| t.text.as_str()))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Collect the text content of a prompt result's messages.
#[allow(dead_code)]
pub fn prompt_text(result: &rmcp::model::GetPromptResult) -> String {
    result
        .messages
        .iter()
        .filter_map(|m| match &m.content {
            rmcp::model::PromptMessageContent::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Resume a panic from a spawned task, or report a non-panic `JoinError`.
fn resume_task_panic(error: tokio::task::JoinError, label: &str) {
    if error.is_panic() {
        std::panic::resume_unwind(error.into_panic());
    }
    panic!("{label} task failed: {error}");
}

/// Await a `JoinHandle` and propagate any panic.
async fn await_joined(handle: tokio::task::JoinHandle<()>, label: &'static str) {
    if let Err(e) = handle.await {
        resume_task_panic(e, label);
    }
}

/// Unwrap a timeout-wrapped client result and propagate any panic.
fn unwrap_client_result(
    result: Result<Result<(), tokio::task::JoinError>, tokio::time::error::Elapsed>,
) {
    let joined = result.unwrap_or_else(|_| panic!("test timed out after 10s"));
    if let Err(e) = joined {
        resume_task_panic(e, "client");
    }
}

/// Serve `server` in-process and run `test_fn` against it as a client.
///
/// Panics from `test_fn` (and timeouts) propagate and fail the test —
/// see `tests/harness_check.rs`, which pins that behavior.
#[allow(dead_code)]
pub async fn run_with_server<F, Fut>(server: RoamServer, test_fn: F)
where
    F: FnOnce(rmcp::service::Peer<rmcp::RoleClient>) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send,
{
    // Two duplex pipes: one for c->s, one for s->c.
    let (a_to_b, a_read) = tokio::io::duplex(8192);
    let (b_to_a, b_read) = tokio::io::duplex(8192);

    let server_handle = tokio::spawn(async move {
        // Keep the running service alive until the test ends — dropping
        // it tears the transport down and every client call fails.
        let running = server
            .serve((a_to_b, b_read))
            .await
            .expect("server failed to start");
        let _ = running.waiting().await;
    });

    // Wait briefly for the server task to come up.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let client_handle = tokio::spawn(async move {
        let client = ().serve((b_to_a, a_read)).await.expect("client connect");
        test_fn(client.peer().clone()).await;
        client.cancel().await.ok();
    });

    let result = tokio::time::timeout(std::time::Duration::from_secs(10), client_handle).await;
    server_handle.abort();
    // Surface what happened inside the spawned tasks — a swallowed panic
    // on either side would turn every assertion in `test_fn` into a no-op.
    await_joined(server_handle, "server").await;
    unwrap_client_result(result);
}
