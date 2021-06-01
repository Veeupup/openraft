mod fixtures;

use std::sync::Arc;

use anyhow::Result;
use async_raft::Config;
use async_raft::State;
use fixtures::RaftRouter;
use maplit::hashset;

/// Cluster initialization test.
///
/// What does this test do?
///
/// - brings 3 nodes online with only knowledge of themselves.
/// - asserts that they remain in non-voter state with no activity (they should be completely passive).
/// - initializes the cluster with membership config including all nodes.
/// - asserts that the cluster was able to come online, elect a leader and maintain a stable state.
/// - asserts that the leader was able to successfully commit its initial payload and that all
///   followers have successfully replicated the payload.
///
/// RUST_LOG=async_raft,memstore,initialization=trace cargo test -p async-raft --test initialization
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn initialization() -> Result<()> {
    fixtures::init_tracing();

    // Setup test dependencies.
    let config = Arc::new(
        Config::build("test".into())
            .validate()
            .expect("failed to build Raft config"),
    );
    let router = Arc::new(RaftRouter::new(config.clone()));
    router.new_raft_node(0).await;
    router.new_raft_node(1).await;
    router.new_raft_node(2).await;

    let mut want = 0;

    // Assert all nodes are in non-voter state & have no entries.
    router
        .wait_for_log(&hashset![0, 1, 2], want, "empty")
        .await?;
    router
        .wait_for_state(&hashset![0, 1, 2], State::NonVoter, "empty")
        .await?;
    router.assert_pristine_cluster().await;

    // Initialize the cluster, then assert that a stable cluster was formed & held.
    tracing::info!("--- initializing cluster");
    router.initialize_from_single_node(0).await?;
    want += 1;

    router
        .wait_for_log(&hashset![0, 1, 2], want, "init")
        .await?;
    router.assert_stable_cluster(Some(1), Some(want)).await;

    Ok(())
}
