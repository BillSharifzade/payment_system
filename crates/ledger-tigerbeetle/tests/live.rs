//! The shared suites against a real cluster. Needs `TIGERBEETLE_ADDRESSES` (default 3000),
//! `TIGERBEETLE_CLUSTER_ID` (default 0) and `DATABASE_URL`. Run serially (`--test-threads=1`): a
//! recovery pass settles every reservation in the cluster, including another test's.

use ledger_tigerbeetle::testkit::*;
use ledger_tigerbeetle::{LiveTb, SimTb};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires TigerBeetle (TIGERBEETLE_ADDRESSES)"]
async fn the_model_and_the_cluster_agree() {
    let (sim, live) = (SimTb::new(), LiveTb::from_test_env());
    for seed in [1, 2, 3] {
        fidelity(&sim, &live, seed).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires TigerBeetle (TIGERBEETLE_ADDRESSES)"]
async fn direct_path_matches_the_in_memory_model() {
    let (seeds, ops) = seeds_from_env();
    direct_differential(LiveTb::from_test_env(), &seeds, ops).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires TigerBeetle (TIGERBEETLE_ADDRESSES) and PostgreSQL (DATABASE_URL)"]
async fn hybrid_matches_the_in_memory_model() {
    let (seeds, ops) = seeds_from_env();
    hybrid_differential(LiveTb::from_test_env(), &postgres().await, &seeds, ops).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires TigerBeetle (TIGERBEETLE_ADDRESSES) and PostgreSQL (DATABASE_URL)"]
async fn a_crash_at_any_step_settles_to_posted_once_or_not_at_all() {
    crash_recovery(LiveTb::from_test_env(), &postgres().await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires TigerBeetle (TIGERBEETLE_ADDRESSES) and PostgreSQL (DATABASE_URL)"]
async fn the_tombstone_serialises_a_request_with_recovery() {
    tombstone_race(LiveTb::from_test_env(), &postgres().await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires TigerBeetle (TIGERBEETLE_ADDRESSES) and PostgreSQL (DATABASE_URL)"]
async fn pending_timeouts_are_a_backstop_only() {
    expiry(LiveTb::from_test_env(), &postgres().await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires TigerBeetle (TIGERBEETLE_ADDRESSES) and PostgreSQL (DATABASE_URL)"]
async fn double_spend_same_id_and_aml_races() {
    concurrency(LiveTb::from_test_env(), &postgres().await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires TigerBeetle (TIGERBEETLE_ADDRESSES) and PostgreSQL (DATABASE_URL)"]
async fn postgres_balances_cut_over_into_tigerbeetle() {
    import_cutover(LiveTb::from_test_env()).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires TigerBeetle (TIGERBEETLE_ADDRESSES) and PostgreSQL (DATABASE_URL)"]
async fn timestamps_bound_what_a_read_saw() {
    timestamps(LiveTb::from_test_env(), &postgres().await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires TigerBeetle (TIGERBEETLE_ADDRESSES) and PostgreSQL (DATABASE_URL)"]
async fn a_post_waits_for_holds_in_flight_like_for_a_row_lock() {
    holds(LiveTb::from_test_env(), &postgres().await).await;
}
