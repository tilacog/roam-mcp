//! Meta-test: the integration harness must propagate panics from the
//! spawned client task. (It used to swallow them, which turned every
//! integration assertion into a no-op.)

mod common;

use org_roam_mcp::{Config, RoamServer};

#[tokio::test]
#[should_panic(expected = "assertion failure must propagate")]
async fn harness_propagates_client_panics() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = Config::from_args(dir.path(), true, true, None).unwrap();
    let server = RoamServer::new(cfg).unwrap();
    common::run_with_server(server, |_peer| async move {
        panic!("assertion failure must propagate");
    })
    .await;
}
