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

/// The deadlock this guard once had: the test panics while one of its pool connections still
/// holds an uncommitted (dropped) transaction, on a current-thread runtime.
#[tokio::test]
async fn a_panic_with_an_open_transaction_does_not_hang_or_leak() {
    if pg_url().is_none() {
        eprintln!("SKIPPED: TEST_DATABASE_URL is not set; PostgreSQL variant not run");
        return;
    }
    let db = TestDb::new(Kind::Postgres).await.expect("postgres");
    let schema = db.schema_name().expect("schema").to_owned();
    let store = db.store.clone();
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(60), async move {
        let mut tx = store.begin().await.expect("begin");
        let _ = tx
            .select(
                &ss_server::store::schema::PROJECTS,
                &ss_server::store::Query::new(),
            )
            .await
            .expect("select");
        // The transaction is dropped uncommitted, then the "test" panics.
        drop(tx);
        let r = std::panic::AssertUnwindSafe(async move {
            let _db = db;
            panic!("deliberate failure with an open transaction");
        });
        futures_util_catch(r).await
    })
    .await;
    assert!(outcome.expect("the drop guard hung").is_err());
    assert!(!schema_exists(&schema).await, "schema {schema} leaked");
}

/// Run a future, turning a panic into `Err` (the drop guard runs during the unwind).
async fn futures_util_catch<F: std::future::Future<Output = ()>>(
    f: std::panic::AssertUnwindSafe<F>,
) -> Result<(), ()> {
    let mut f = Box::pin(f.0);
    std::future::poll_fn(move |cx| {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f.as_mut().poll(cx))) {
            Ok(std::task::Poll::Ready(())) => std::task::Poll::Ready(Ok(())),
            Ok(std::task::Poll::Pending) => std::task::Poll::Pending,
            Err(_) => std::task::Poll::Ready(Err(())),
        }
    })
    .await
}
