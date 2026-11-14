//! Store semantics, identical on both implementations: transactions, savepoints, constraints,
//! identity, optimistic concurrency, ON DELETE rules. PostgreSQL-only: the migration runner.

mod common;

use serde_json::json;
use ss_core::{Market, ProjectContext};
use ss_server::store::repo::{users, User, PROJECTS};
use ss_server::store::schema::{self, ALL_TABLES};
use ss_server::store::{Query, Row, SqlValue, StoreError, Tx, UpdateOutcome};
use uuid::Uuid;

use common::*;

fn user(id: Uuid, email: Option<&str>) -> User {
    User {
        id,
        display_name: "u".into(),
        email: email.map(str::to_owned),
        phone: None,
    }
}

async fn project(tx: &mut dyn Tx) -> ProjectContext {
    let owner = Uuid::new_v4();
    users::insert(tx, &user(owner, None)).await.unwrap();
    let p = ProjectContext::new(owner, "p", Market::new("Los Angeles", "CA"));
    PROJECTS.add(tx, &p).await.unwrap();
    p
}

fn event_row(project_id: Uuid, key: &str) -> Row {
    Row::new()
        .with("id", Uuid::new_v4())
        .with("project_id", project_id)
        .with("event_type", "task.started")
        .with("correlation_id", Uuid::new_v4())
        .with("idempotency_key", key)
        .with("envelope", json!({"k": key}))
}

both_backends!(commit_rollback_and_drop, |db| {
    let s = &db.store;
    let (a, b, c) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    let mut tx = s.begin().await.unwrap();
    users::insert(&mut *tx, &user(a, None)).await.unwrap();
    tx.commit().await.unwrap();
    let mut tx = s.begin().await.unwrap();
    users::insert(&mut *tx, &user(b, None)).await.unwrap();
    tx.rollback().await.unwrap();
    {
        let mut tx = s.begin().await.unwrap();
        users::insert(&mut *tx, &user(c, None)).await.unwrap();
        // dropped without commit
    }
    let mut tx = s.begin().await.unwrap();
    assert!(users::get(&mut *tx, a).await.unwrap().is_some());
    assert!(users::get(&mut *tx, b).await.unwrap().is_none());
    assert!(users::get(&mut *tx, c).await.unwrap().is_none());
});

both_backends!(unique_constraints_are_typed, |db| {
    let mut txb = db.store.begin().await.unwrap();
    let tx = &mut *txb;
    let id = Uuid::new_v4();
    users::insert(tx, &user(id, Some("a@x.invalid")))
        .await
        .unwrap();
    // Each violation inside its own savepoint, as callers must do to continue the transaction.
    for (u, constraint) in [
        (user(id, None), "pk_users"),
        (user(Uuid::new_v4(), Some("a@x.invalid")), "uq_users_email"),
    ] {
        let sp = tx.savepoint().await.unwrap();
        let e = users::insert(tx, &u).await.unwrap_err();
        assert_eq!(
            e,
            StoreError::UniqueViolation {
                table: "users".into(),
                constraint: constraint.into()
            }
        );
        tx.rollback_to_savepoint(sp).await.unwrap();
    }
    // NULLs never conflict.
    users::insert(tx, &user(Uuid::new_v4(), None))
        .await
        .unwrap();
    users::insert(tx, &user(Uuid::new_v4(), None))
        .await
        .unwrap();
    // Composite unique with a NULL member does not apply (property_sources.dataset is nullable).
    let p = project(tx).await;
    let prop = Uuid::new_v4();
    tx.insert(
        &schema::PROPERTIES,
        Row::new()
            .with("id", prop)
            .with("project_id", p.project_id)
            .with("normalized_address", "1 MAIN ST")
            .with("data_origin", "DERIVED")
            .with("data", json!({})),
    )
    .await
    .unwrap();
    let source = |dataset: Option<&str>| {
        Row::new()
            .with("id", Uuid::new_v4())
            .with("property_id", prop)
            .with("provider", "p")
            .with("dataset", dataset)
            .with("source_record_id", "r1")
            .with("data_origin", "LIVE")
            .with("raw_payload", json!({}))
    };
    tx.insert(&schema::PROPERTY_SOURCES, source(None))
        .await
        .unwrap();
    tx.insert(&schema::PROPERTY_SOURCES, source(None))
        .await
        .unwrap();
    tx.insert(&schema::PROPERTY_SOURCES, source(Some("d")))
        .await
        .unwrap();
    let sp = tx.savepoint().await.unwrap();
    let e = tx
        .insert(&schema::PROPERTY_SOURCES, source(Some("d")))
        .await
        .unwrap_err();
    assert!(
        e.is_unique_violation("uq_property_sources_provider"),
        "{e:?}"
    );
    tx.rollback_to_savepoint(sp).await.unwrap();
    let sp = tx.savepoint().await.unwrap();
    let e = tx
        .insert(
            &schema::PROPERTIES,
            Row::new()
                .with("id", Uuid::new_v4())
                .with("project_id", p.project_id)
                .with("normalized_address", "1 MAIN ST")
                .with("data_origin", "DERIVED")
                .with("data", json!({})),
        )
        .await
        .unwrap_err();
    assert!(
        e.is_unique_violation("uq_property_project_address"),
        "{e:?}"
    );
    tx.rollback_to_savepoint(sp).await.unwrap();
    txb.commit().await.unwrap();
});

both_backends!(failed_statement_aborts_until_savepoint_rollback, |db| {
    let id = Uuid::new_v4();
    let mut tx = db.store.begin().await.unwrap();
    users::insert(&mut *tx, &user(id, None)).await.unwrap();
    assert!(users::insert(&mut *tx, &user(id, None)).await.is_err());
    // Like PostgreSQL: everything fails until the transaction ends...
    let e = tx.get(&schema::USERS, id).await.unwrap_err();
    assert!(
        e.to_string().contains("current transaction is aborted"),
        "{e}"
    );
    // Through a repository the same failure is an opaque 500.
    assert_eq!(
        users::get(&mut *tx, id)
            .await
            .unwrap_err()
            .status()
            .as_u16(),
        500
    );
    // ...and a commit is a reported rollback.
    let e = tx.commit().await.unwrap_err();
    assert!(e.to_string().contains("rolled back"), "{e}");
    let mut tx = db.store.begin().await.unwrap();
    assert!(users::get(&mut *tx, id).await.unwrap().is_none());
});

both_backends!(nested_savepoints, |db| {
    let mut txb = db.store.begin().await.unwrap();
    let tx = &mut *txb;
    let ids: Vec<Uuid> = (0..4).map(|_| Uuid::new_v4()).collect();
    users::insert(tx, &user(ids[0], None)).await.unwrap();
    let outer = tx.savepoint().await.unwrap();
    users::insert(tx, &user(ids[1], None)).await.unwrap();
    let inner = tx.savepoint().await.unwrap();
    users::insert(tx, &user(ids[2], None)).await.unwrap();
    tx.rollback_to_savepoint(inner).await.unwrap();
    let inner2 = tx.savepoint().await.unwrap();
    users::insert(tx, &user(ids[3], None)).await.unwrap();
    tx.release_savepoint(inner2).await.unwrap();
    assert!(users::get(tx, ids[2]).await.unwrap().is_none());
    assert!(users::get(tx, ids[3]).await.unwrap().is_some());
    tx.rollback_to_savepoint(outer).await.unwrap();
    for id in &ids[1..] {
        assert!(users::get(tx, *id).await.unwrap().is_none());
    }
    // A released savepoint cannot be rolled back to.
    let sp = tx.savepoint().await.unwrap();
    tx.release_savepoint(sp).await.unwrap();
    assert!(tx.rollback_to_savepoint(sp).await.is_err());
    drop(txb);
    let mut tx = db.store.begin().await.unwrap();
    let sp = tx.savepoint().await.unwrap();
    users::insert(&mut *tx, &user(ids[0], None)).await.unwrap();
    tx.release_savepoint(sp).await.unwrap();
    tx.commit().await.unwrap();
    let mut tx = db.store.begin().await.unwrap();
    assert!(
        users::get(&mut *tx, ids[0]).await.unwrap().is_some(),
        "released work commits"
    );
});

both_backends!(seq_is_monotonic_and_never_reused, |db| {
    let mut tx = db.store.begin().await.unwrap();
    let p = project(&mut *tx).await;
    let s1 = tx
        .insert(&schema::ACTIVITY_EVENTS, event_row(p.project_id, "a"))
        .await
        .unwrap()
        .int("seq")
        .unwrap();
    let s2 = tx
        .insert(&schema::ACTIVITY_EVENTS, event_row(p.project_id, "b"))
        .await
        .unwrap()
        .int("seq")
        .unwrap();
    assert!(s2 > s1);
    tx.commit().await.unwrap();
    let mut tx = db.store.begin().await.unwrap();
    let s3 = tx
        .insert(&schema::ACTIVITY_EVENTS, event_row(p.project_id, "c"))
        .await
        .unwrap()
        .int("seq")
        .unwrap();
    tx.rollback().await.unwrap();
    let mut tx = db.store.begin().await.unwrap();
    let s4 = tx
        .insert(&schema::ACTIVITY_EVENTS, event_row(p.project_id, "c"))
        .await
        .unwrap()
        .int("seq")
        .unwrap();
    assert!(
        s3 > s2 && s4 > s3,
        "rolled-back values are not reused: {s2} {s3} {s4}"
    );
    let sp = tx.savepoint().await.unwrap();
    let e = tx
        .insert(&schema::ACTIVITY_EVENTS, event_row(p.project_id, "c"))
        .await
        .unwrap_err();
    assert!(e.is_unique_violation("uq_activity_events_idempotency_key"));
    tx.rollback_to_savepoint(sp).await.unwrap();
    let rows = tx
        .select(
            &schema::ACTIVITY_EVENTS,
            &Query::new()
                .eq("project_id", p.project_id)
                .gt("seq", s1)
                .order_asc("seq"),
        )
        .await
        .unwrap();
    let seqs: Vec<i64> = rows.iter().map(|r| r.int("seq").unwrap()).collect();
    assert_eq!(seqs, [s2, s4]);
    tx.commit().await.unwrap();
});

both_backends!(versioned_updates, |db| {
    let mut txb = db.store.begin().await.unwrap();
    let tx = &mut *txb;
    let mut p = project(tx).await;
    assert_eq!(
        PROJECTS
            .get_versioned(tx, p.project_id)
            .await
            .unwrap()
            .unwrap()
            .1,
        1
    );
    p.project_name = "renamed".into();
    PROJECTS.upsert(tx, &p).await.unwrap();
    let (got, v) = PROJECTS
        .get_versioned(tx, p.project_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!((got.project_name.as_str(), v), ("renamed", 2));
    // The document's own record_version is not touched by upsert (as Python).
    assert_eq!(got.record_version, 1);
    p.project_name = "v3".into();
    assert_eq!(PROJECTS.update_versioned(tx, &p, 2).await.unwrap(), 3);
    let e = PROJECTS.update_versioned(tx, &p, 2).await.unwrap_err();
    assert_eq!(e.error_code(), "STALE_VERSION");
    assert_eq!(e.status().as_u16(), 409);
    assert_eq!(e.public_details()["expected"], 2);
    assert_eq!(e.public_details()["actual"], 3);
    let ghost = ProjectContext::new(p.owner_id, "ghost", Market::new("LA", "CA"));
    let e = PROJECTS.update_versioned(tx, &ghost, 1).await.unwrap_err();
    assert_eq!(e.error_code(), "NOT_FOUND");
    assert_eq!(e.to_string(), "ProjectContext not found");
    let e = PROJECTS.require(tx, ghost.project_id).await.unwrap_err();
    assert_eq!(
        e.to_string(),
        format!("ProjectContext {} not found", ghost.project_id)
    );
    // The indexed columns follow the document.
    let row = tx
        .get(&schema::PROJECTS, p.project_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.text("status").unwrap(), "ONBOARDING");
    assert_eq!(row.text("schema_version").unwrap(), "1.0");
    assert_eq!(row.json("data").unwrap()["project_name"], "v3");
    assert!(row.timestamp("updated_at").unwrap() >= row.timestamp("created_at").unwrap());
    match tx
        .update(
            &schema::PROJECTS,
            Uuid::new_v4(),
            Row::new().with("status", "X"),
            None,
        )
        .await
        .unwrap()
    {
        UpdateOutcome::NotFound => {}
        other => panic!("{other:?}"),
    }
});

both_backends!(not_null_types_and_lengths, |db| {
    let mut txb = db.store.begin().await.unwrap();
    let tx = &mut *txb;
    let cases: Vec<(Row, &str)> = vec![
        (Row::new().with("id", Uuid::new_v4()), "not null"),
        (
            Row::new()
                .with("id", Uuid::new_v4())
                .with("display_name", "x".repeat(201)),
            "too long",
        ),
        (
            Row::new()
                .with("id", Uuid::new_v4())
                .with("display_name", 5i64),
            "type",
        ),
        (
            Row::new()
                .with("id", Uuid::new_v4())
                .with("display_name", "x")
                .with("nope", 1i64),
            "unknown column",
        ),
        (
            Row::new()
                .with("id", Uuid::new_v4())
                .with("display_name", "a\0b"),
            "nul",
        ),
    ];
    for (row, what) in cases {
        let sp = tx.savepoint().await.unwrap();
        let e = tx.insert(&schema::USERS, row).await.unwrap_err();
        let ok = match what {
            "not null" => matches!(e, StoreError::NotNullViolation { .. }),
            "too long" => matches!(e, StoreError::ValueTooLong { .. }),
            "type" | "nul" => matches!(e, StoreError::InvalidValue { .. }),
            _ => matches!(e, StoreError::UnknownColumn { .. }),
        };
        assert!(ok, "{what}: {e:?}");
        tx.rollback_to_savepoint(sp).await.unwrap();
    }
    // 32-bit INTEGER range and JSON NUL are refused like PostgreSQL does.
    let p = project(tx).await;
    let sp = tx.savepoint().await.unwrap();
    let e = tx
        .insert(
            &schema::PROPERTY_RANKINGS,
            Row::new()
                .with("id", Uuid::new_v4())
                .with("project_id", p.project_id)
                .with("ranking_version", 1i64 << 40)
                .with("data", json!({})),
        )
        .await
        .unwrap_err();
    assert!(matches!(e, StoreError::InvalidValue { .. }), "{e:?}");
    tx.rollback_to_savepoint(sp).await.unwrap();
    let sp = tx.savepoint().await.unwrap();
    let e = tx
        .insert(
            &schema::PROPERTY_RANKINGS,
            Row::new()
                .with("id", Uuid::new_v4())
                .with("project_id", p.project_id)
                .with("ranking_version", 1i64)
                .with("data", json!({"s": "a\u{0}b"})),
        )
        .await
        .unwrap_err();
    assert!(matches!(e, StoreError::InvalidValue { .. }), "{e:?}");
    tx.rollback_to_savepoint(sp).await.unwrap();
    // Python-side defaults fill omitted columns.
    let id = Uuid::new_v4();
    let prop = Uuid::new_v4();
    tx.insert(
        &schema::PROPERTIES,
        Row::new()
            .with("id", prop)
            .with("project_id", p.project_id)
            .with("normalized_address", "A")
            .with("data_origin", "DERIVED")
            .with("data", json!({})),
    )
    .await
    .unwrap();
    let row = tx
        .insert(
            &schema::COST_SCENARIOS,
            Row::new()
                .with("id", id)
                .with("project_id", p.project_id)
                .with("property_id", prop)
                .with("long_term_cost", 5i64)
                .with("data", json!({})),
        )
        .await
        .unwrap();
    assert_eq!(row.get("superseded"), Some(&SqlValue::Bool(false)));
    assert_eq!(row.int("record_version").unwrap(), 1);
});

both_backends!(foreign_keys_and_on_delete, |db| {
    let mut txb = db.store.begin().await.unwrap();
    let tx = &mut *txb;
    let sp = tx.savepoint().await.unwrap();
    let orphan = ProjectContext::new(Uuid::new_v4(), "orphan", Market::new("LA", "CA"));
    let e = PROJECTS.add(tx, &orphan).await.unwrap_err();
    assert!(
        e.to_string().contains("foreign key") || e.public_message().contains("unexpected"),
        "{e}"
    );
    tx.rollback_to_savepoint(sp).await.unwrap();
    let p = project(tx).await;
    tx.insert(&schema::ACTIVITY_EVENTS, event_row(p.project_id, "e1"))
        .await
        .unwrap();
    let prop = Uuid::new_v4();
    tx.insert(
        &schema::PROPERTIES,
        Row::new()
            .with("id", prop)
            .with("project_id", p.project_id)
            .with("normalized_address", "A")
            .with("data_origin", "DERIVED")
            .with("data", json!({})),
    )
    .await
    .unwrap();
    let dv = |id: Uuid, parent: Option<Uuid>, n: i64| {
        Row::new()
            .with("id", id)
            .with("project_id", p.project_id)
            .with("property_id", prop)
            .with("parent_version_id", parent)
            .with("version_number", n)
            .with("approval_status", "DRAFT")
            .with("valid", true)
            .with("data", json!({}))
    };
    let (v1, v2) = (Uuid::new_v4(), Uuid::new_v4());
    tx.insert(&schema::DESIGN_VERSIONS, dv(v1, None, 1))
        .await
        .unwrap();
    tx.insert(&schema::DESIGN_VERSIONS, dv(v2, Some(v1), 2))
        .await
        .unwrap();
    // SET NULL on the self reference.
    assert_eq!(
        tx.delete_where(
            &schema::DESIGN_VERSIONS,
            &[ss_server::store::Filter::Eq("id", v1.into())]
        )
        .await
        .unwrap(),
        1
    );
    assert_eq!(
        tx.get(&schema::DESIGN_VERSIONS, v2)
            .await
            .unwrap()
            .unwrap()
            .get("parent_version_id"),
        Some(&SqlValue::Null)
    );
    // RESTRICT: a user with projects cannot be deleted.
    let sp = tx.savepoint().await.unwrap();
    let e = tx
        .delete_where(
            &schema::USERS,
            &[ss_server::store::Filter::Eq("id", p.owner_id.into())],
        )
        .await
        .unwrap_err();
    assert!(matches!(e, StoreError::ForeignKeyViolation { .. }), "{e:?}");
    tx.rollback_to_savepoint(sp).await.unwrap();
    // CASCADE: deleting the project removes its events, properties and their design versions.
    assert_eq!(
        tx.delete_where(
            &schema::PROJECTS,
            &[ss_server::store::Filter::Eq("id", p.project_id.into())]
        )
        .await
        .unwrap(),
        1
    );
    assert!(tx
        .select(
            &schema::ACTIVITY_EVENTS,
            &Query::new().eq("project_id", p.project_id)
        )
        .await
        .unwrap()
        .is_empty());
    assert!(tx.get(&schema::PROPERTIES, prop).await.unwrap().is_none());
    assert!(tx
        .get(&schema::DESIGN_VERSIONS, v2)
        .await
        .unwrap()
        .is_none());
    assert!(users::get(tx, p.owner_id).await.unwrap().is_some());
});

both_backends!(bulk_update_and_ordering, |db| {
    let mut txb = db.store.begin().await.unwrap();
    let tx = &mut *txb;
    let p = project(tx).await;
    let prop = Uuid::new_v4();
    tx.insert(
        &schema::PROPERTIES,
        Row::new()
            .with("id", prop)
            .with("project_id", p.project_id)
            .with("normalized_address", "A")
            .with("parcel_id", Option::<String>::None)
            .with("data_origin", "DERIVED")
            .with("data", json!({})),
    )
    .await
    .unwrap();
    for cost in [30i64, 10, 20] {
        tx.insert(
            &schema::COST_SCENARIOS,
            Row::new()
                .with("id", Uuid::new_v4())
                .with("project_id", p.project_id)
                .with("property_id", prop)
                .with("long_term_cost", cost)
                .with("data", json!({"c": cost})),
        )
        .await
        .unwrap();
    }
    let n = tx
        .update_where(
            &schema::COST_SCENARIOS,
            &[ss_server::store::Filter::Eq("property_id", prop.into())],
            Row::new().with("superseded", true),
        )
        .await
        .unwrap();
    assert_eq!(n, 3);
    let live = tx
        .select(
            &schema::COST_SCENARIOS,
            &Query::new().eq("property_id", prop).eq("superseded", false),
        )
        .await
        .unwrap();
    assert!(live.is_empty());
    let rows = tx
        .select(
            &schema::COST_SCENARIOS,
            &Query::new()
                .eq("property_id", prop)
                .order_desc("long_term_cost")
                .limit(2),
        )
        .await
        .unwrap();
    let costs: Vec<i64> = rows
        .iter()
        .map(|r| r.int("long_term_cost").unwrap())
        .collect();
    assert_eq!(costs, [30, 20]);
    assert!(
        rows.iter().all(|r| r.int("record_version").unwrap() == 1),
        "bulk updates do not bump versions"
    );
    let nulls = tx
        .select(
            &schema::PROPERTIES,
            &Query::new().eq("parcel_id", SqlValue::Null),
        )
        .await
        .unwrap();
    assert_eq!(nulls.len(), 1);
    let not_null = tx
        .select(
            &schema::PROPERTIES,
            &Query::new().ne("parcel_id", SqlValue::Null),
        )
        .await
        .unwrap();
    assert!(not_null.is_empty());
    let none = tx
        .select(&schema::PROPERTIES, &Query::new().ne("parcel_id", "x"))
        .await
        .unwrap();
    assert!(none.is_empty(), "NULL <> 'x' is not true in SQL");
    let e = tx
        .select(&schema::PROPERTIES, &Query::new().eq("nope", 1i64))
        .await
        .unwrap_err();
    assert!(matches!(e, StoreError::UnknownColumn { .. }));
});

#[test]
fn every_models_py_table_is_declared() {
    let names: Vec<&str> = ALL_TABLES.iter().map(|t| t.name).collect();
    for t in [
        "users",
        "projects",
        "business_requirements",
        "properties",
        "property_sources",
        "property_identity_matches",
        "permit_records",
        "property_observations",
        "infrastructure_assessments",
        "feasibility_reports",
        "renovation_work_items",
        "cost_scenarios",
        "property_rankings",
        "site_geometries",
        "design_versions",
        "rooms",
        "design_objects",
        "bills_of_materials",
        "bom_items",
        "supplier_offers",
        "communications",
        "owner_channels",
        "call_sessions",
        "owner_handoff_requests",
        "approvals",
        "tasks",
        "activity_events",
        "processed_webhook_events",
    ] {
        assert!(names.contains(&t), "{t}");
    }
    assert_eq!(names.len(), 28);
}

#[tokio::test]
async fn postgres_schema_matches_metadata_and_migrations_are_idempotent() {
    if pg_url().is_none() {
        eprintln!("SKIPPED: TEST_DATABASE_URL is not set");
        return;
    }
    let (store, schema_name) = pg_store().await;
    let pool = store.pool();
    // Second run: nothing to do.
    assert_eq!(
        store.migrate().await.unwrap(),
        ss_server::store::postgres::MigrationOutcome::UpToDate
    );
    let version: String = sqlx::query_scalar("SELECT version_num FROM alembic_version")
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(version, "0002");
    for t in ALL_TABLES {
        let cols: Vec<(String, String, String, Option<i32>)> = sqlx::query_as(
            "SELECT column_name::text, data_type::text, is_nullable::text, character_maximum_length::int \
             FROM information_schema.columns WHERE table_schema = current_schema() AND table_name = $1 ORDER BY ordinal_position",
        )
        .bind(t.name)
        .fetch_all(pool)
        .await
        .unwrap();
        assert_eq!(cols.len(), t.columns.len(), "{}", t.name);
        for (c, (name, ty, nullable, len)) in t.columns.iter().zip(&cols) {
            assert_eq!(c.name, name, "{}", t.name);
            assert_eq!(c.ty.pg_data_type(), ty, "{}.{}", t.name, c.name);
            assert_eq!(c.nullable, nullable == "YES", "{}.{}", t.name, c.name);
            if let ss_server::store::ColType::Varchar(n) = c.ty {
                assert_eq!(Some(n as i32), *len, "{}.{}", t.name, c.name);
            }
        }
        let constraints: Vec<String> = sqlx::query_scalar(
            "SELECT conname::text FROM pg_constraint WHERE conrelid = to_regclass($1) ORDER BY 1",
        )
        .bind(t.name)
        .fetch_all(pool)
        .await
        .unwrap();
        let mut want: Vec<String> = vec![t.primary_key_name()];
        want.extend(t.uniques.iter().map(|u| u.name.to_owned()));
        want.extend(t.foreign_keys.iter().map(|f| f.name.to_owned()));
        want.sort();
        assert_eq!(constraints, want, "{}", t.name);
        let indexes: Vec<String> = sqlx::query_scalar(
            "SELECT indexname::text FROM pg_indexes WHERE schemaname = current_schema() AND tablename = $1 \
             AND indexname NOT IN (SELECT conname FROM pg_constraint WHERE conrelid = to_regclass($1)) ORDER BY 1",
        )
        .bind(t.name)
        .fetch_all(pool)
        .await
        .unwrap();
        let mut want: Vec<String> = t.indexes.iter().map(|i| i.name.to_owned()).collect();
        want.sort();
        assert_eq!(indexes, want, "{}", t.name);
    }
    drop_schema(&schema_name).await;
}

#[tokio::test]
async fn postgres_migration_is_a_noop_on_an_alembic_database() {
    // Reproduce what `alembic upgrade head` leaves behind (its own DDL, which our migration files
    // are, plus alembic_version = 0002), then run the Rust runner.
    let Some(url) = pg_url() else {
        eprintln!("SKIPPED: TEST_DATABASE_URL is not set");
        return;
    };
    use sqlx::ConnectOptions;
    use std::str::FromStr;
    let base = sqlx::postgres::PgConnectOptions::from_str(
        &ss_server::config::normalize_database_url(&url),
    )
    .unwrap();
    let schema_name = format!("rt_{}", Uuid::new_v4().simple());
    let mut admin = base.clone().connect().await.unwrap();
    sqlx::Executor::execute(&mut admin, format!("CREATE SCHEMA {schema_name}").as_str())
        .await
        .unwrap();
    let opts = base.options([("search_path", schema_name.as_str())]);
    let mut conn = opts.clone().connect().await.unwrap();
    let mut alembic = String::from("BEGIN;\nCREATE TABLE alembic_version (\n    version_num VARCHAR(32) NOT NULL, \n    CONSTRAINT alembic_version_pkc PRIMARY KEY (version_num)\n);\n");
    alembic.push_str(include_str!("../migrations/0001_foundation_schema.sql"));
    alembic.push_str("\nINSERT INTO alembic_version (version_num) VALUES ('0001') RETURNING alembic_version.version_num;\n");
    alembic.push_str(include_str!("../migrations/0002_owner_channels.sql"));
    alembic.push_str("\nUPDATE alembic_version SET version_num='0002' WHERE alembic_version.version_num = '0001';\nCOMMIT;\n");
    sqlx::Executor::execute(&mut conn, alembic.as_str())
        .await
        .unwrap();
    sqlx::Executor::execute(&mut conn, "INSERT INTO users (id, display_name) VALUES ('00000000-0000-4000-8000-000000000001', 'kept')").await.unwrap();
    let store = ss_server::store::postgres::PgStore::connect_with(opts.clone(), 2)
        .await
        .unwrap();
    assert_eq!(
        store.migrate().await.unwrap(),
        ss_server::store::postgres::MigrationOutcome::UpToDate
    );
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM users")
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(n, 1, "data untouched");
    // A database at a revision this binary does not know is left alone.
    sqlx::Executor::execute(&mut conn, "UPDATE alembic_version SET version_num = '0099'")
        .await
        .unwrap();
    assert_eq!(
        store.migrate().await.unwrap(),
        ss_server::store::postgres::MigrationOutcome::UnknownRevision("0099".into())
    );
    // From 0001 only, the runner applies exactly 0002.
    sqlx::Executor::execute(
        &mut conn,
        "DROP TABLE owner_channels; UPDATE alembic_version SET version_num = '0001'",
    )
    .await
    .unwrap();
    assert_eq!(
        store.migrate().await.unwrap(),
        ss_server::store::postgres::MigrationOutcome::Applied(vec!["0002"])
    );
    let version: String = sqlx::query_scalar("SELECT version_num FROM alembic_version")
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(version, "0002");
    drop(conn);
    drop_schema(&schema_name).await;
}
