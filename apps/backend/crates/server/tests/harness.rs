//! The test harness itself (R12 of the part 1 review): a PostgreSQL test that panics must not
//! leak its `rt_*` schema.

mod common;

use common::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_panicking_postgres_test_does_not_leak_its_schema() {
    if pg_url().is_none() {
        eprintln!("SKIPPED: TEST_DATABASE_URL is not set; PostgreSQL variant not run");
        return;
    }
    let db = TestDb::new(Kind::Postgres).await.expect("postgres");
    let schema = db.schema_name().expect("schema").to_owned();
    assert!(schema_exists(&schema).await, "schema {schema} not created");
    // The test body owns the database and panics before it can call `finish`.
    let test = tokio::spawn(async move {
        let _db = db;
        tokio::task::yield_now().await;
        panic!("deliberate failure inside a test body");
    });
    assert!(test.await.expect_err("the task panics").is_panic());
    assert!(
        !schema_exists(&schema).await,
        "schema {schema} was left behind by a panicking test"
    );
}
