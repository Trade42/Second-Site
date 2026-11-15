//! In-memory `Store`. Data is not durable: it lives only as long as the process.
//!
//! Transactions are serialized: `begin` takes an exclusive lock that the transaction holds until
//! it commits, rolls back or is dropped (so every read is effectively `FOR UPDATE`). Writes are
//! applied in place and recorded in an undo log; rollback (explicit, on drop, or to a savepoint)
//! replays the log backwards. Constraints are checked like PostgreSQL does: primary key, unique
//! (NULLs never conflict), not-null, `VARCHAR(n)` length, 32-bit `INTEGER` range, foreign keys
//! (with `ON DELETE` cascade / set null / restrict). Identity values are never reused.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use tokio::sync::{Mutex, OwnedMutexGuard};
use uuid::Uuid;

use super::schema::{self, ColDefault, OnDelete, TableDef};
use super::{
    check_row_types, micros, Backend, Filter, Order, Query, Row, Savepoint, SqlValue, Store,
    StoreError, StoreResult, Tx, UpdateOutcome,
};

#[derive(Debug, Clone)]
struct MemRow {
    /// Insertion sequence: a stable tie-breaker for ordering.
    ins: u64,
    row: Row,
}

#[derive(Debug, Default)]
struct MemTable {
    rows: HashMap<Uuid, MemRow>,
}

#[derive(Debug, Default)]
struct MemState {
    tables: HashMap<&'static str, MemTable>,
    /// constraint name -> key -> row id
    unique: HashMap<&'static str, HashMap<String, Uuid>>,
    /// (table, column) -> last issued identity value
    identity: HashMap<(&'static str, &'static str), i64>,
    next_ins: u64,
}

#[derive(Debug)]
enum Undo {
    Inserted {
        table: &'static TableDef,
        id: Uuid,
    },
    Updated {
        table: &'static TableDef,
        before: MemRow,
    },
    Deleted {
        table: &'static TableDef,
        before: MemRow,
    },
}

fn key_part(v: &SqlValue) -> Option<String> {
    Some(match v {
        SqlValue::Null => return None,
        SqlValue::Uuid(u) => format!("u:{u}"),
        SqlValue::Text(s) => format!("t:{}:{s}", s.len()),
        SqlValue::Int(i) => format!("i:{i}"),
        SqlValue::Bool(b) => format!("b:{b}"),
        SqlValue::Float(f) => format!("f:{}", f.to_bits()),
        SqlValue::Json(j) => format!("j:{j}"),
        SqlValue::Timestamp(t) => format!("ts:{}", t.timestamp_micros()),
    })
}

fn unique_key(row: &Row, cols: &[&str]) -> Option<String> {
    let mut parts = Vec::with_capacity(cols.len());
    for c in cols {
        parts.push(key_part(row.get(c).unwrap_or(&SqlValue::Null))?);
    }
    Some(parts.join("\u{1f}"))
}

impl MemState {
    fn table_mut(&mut self, t: &'static TableDef) -> &mut MemTable {
        self.tables.entry(t.name).or_default()
    }

    fn row(&self, t: &TableDef, id: Uuid) -> Option<&MemRow> {
        self.tables.get(t.name).and_then(|tb| tb.rows.get(&id))
    }

    fn exists(&self, table: &str, id: Uuid) -> bool {
        self.tables
            .get(table)
            .is_some_and(|tb| tb.rows.contains_key(&id))
    }

    /// Check unique constraints for `row` (which will have id `id`), ignoring the row itself.
    fn check_unique(&self, t: &TableDef, id: Uuid, row: &Row) -> StoreResult<()> {
        for u in t.uniques {
            if let Some(k) = unique_key(row, u.columns) {
                if let Some(other) = self.unique.get(u.name).and_then(|m| m.get(&k)) {
                    if *other != id {
                        return Err(StoreError::UniqueViolation {
                            table: t.name.into(),
                            constraint: u.name.into(),
                        });
                    }
                }
            }
        }
        Ok(())
    }

    fn check_foreign_keys(&self, t: &TableDef, row: &Row) -> StoreResult<()> {
        for f in t.foreign_keys {
            if let Some(SqlValue::Uuid(target)) = row.get(f.column) {
                let self_ref =
                    f.references == t.name && row.get("id") == Some(&SqlValue::Uuid(*target));
                if !self_ref && !self.exists(f.references, *target) {
                    return Err(StoreError::ForeignKeyViolation {
                        table: t.name.into(),
                        constraint: f.name.into(),
                    });
                }
            }
        }
        Ok(())
    }

    fn check_not_null(t: &TableDef, row: &Row) -> StoreResult<()> {
        for c in t.columns {
            if !c.nullable && row.get(c.name).is_none_or(SqlValue::is_null) {
                return Err(StoreError::NotNullViolation {
                    table: t.name.into(),
                    column: c.name.into(),
                });
            }
        }
        Ok(())
    }

    fn index_add(&mut self, t: &TableDef, id: Uuid, row: &Row) {
        for u in t.uniques {
            if let Some(k) = unique_key(row, u.columns) {
                self.unique.entry(u.name).or_default().insert(k, id);
            }
        }
    }

    fn index_remove(&mut self, t: &TableDef, row: &Row) {
        for u in t.uniques {
            if let Some(k) = unique_key(row, u.columns) {
                if let Some(m) = self.unique.get_mut(u.name) {
                    m.remove(&k);
                }
            }
        }
    }

    /// Store a row (no checks): used by forward operations after checking, and by undo.
    fn put(&mut self, t: &'static TableDef, mr: MemRow) {
        let id = match mr.row.get("id") {
            Some(SqlValue::Uuid(u)) => *u,
            _ => return,
        };
        if let Some(old) = self.table_mut(t).rows.remove(&id) {
            self.index_remove(t, &old.row);
        }
        self.index_add(t, id, &mr.row);
        self.table_mut(t).rows.insert(id, mr);
    }

    fn remove(&mut self, t: &'static TableDef, id: Uuid) -> Option<MemRow> {
        let old = self.table_mut(t).rows.remove(&id)?;
        self.index_remove(t, &old.row);
        Some(old)
    }

    fn apply_undo(&mut self, u: Undo) {
        match u {
            Undo::Inserted { table, id } => {
                self.remove(table, id);
            }
            Undo::Updated { table, before } | Undo::Deleted { table, before } => {
                self.put(table, before);
            }
        }
    }
}

fn matches(row: &Row, filters: &[Filter]) -> bool {
    filters.iter().all(|f| match f {
        Filter::Eq(c, SqlValue::Null) => row.get(c).is_none_or(SqlValue::is_null),
        Filter::Ne(c, SqlValue::Null) => row.get(c).is_some_and(|v| !v.is_null()),
        Filter::Eq(c, v) => row.get(c) == Some(v),
        // SQL three-valued logic: a NULL column never satisfies `<>`, `>` or `<`.
        Filter::Ne(c, v) => row.get(c).is_some_and(|x| !x.is_null() && x != v),
        Filter::Gt(c, v) => row
            .get(c)
            .is_some_and(|x| cmp_values(x, v) == Some(std::cmp::Ordering::Greater)),
        Filter::Lt(c, v) => row
            .get(c)
            .is_some_and(|x| cmp_values(x, v) == Some(std::cmp::Ordering::Less)),
    })
}

fn cmp_values(a: &SqlValue, b: &SqlValue) -> Option<std::cmp::Ordering> {
    use SqlValue::*;
    match (a, b) {
        (Uuid(x), Uuid(y)) => Some(x.cmp(y)),
        (Text(x), Text(y)) => Some(x.as_bytes().cmp(y.as_bytes())),
        (Int(x), Int(y)) => Some(x.cmp(y)),
        (Bool(x), Bool(y)) => Some(x.cmp(y)),
        (Float(x), Float(y)) => x.partial_cmp(y),
        (Timestamp(x), Timestamp(y)) => Some(x.cmp(y)),
        _ => None,
    }
}

/// ORDER BY with PostgreSQL's NULL placement (last for ASC, first for DESC), then insertion order.
fn sort_rows(rows: &mut [MemRow], order: &[(&'static str, Order)]) {
    rows.sort_by(|a, b| {
        for (col, dir) in order {
            let x = a.row.get(col).unwrap_or(&SqlValue::Null);
            let y = b.row.get(col).unwrap_or(&SqlValue::Null);
            let o = match (x.is_null(), y.is_null()) {
                (true, true) => std::cmp::Ordering::Equal,
                (true, false) => std::cmp::Ordering::Greater,
                (false, true) => std::cmp::Ordering::Less,
                (false, false) => cmp_values(x, y).unwrap_or(std::cmp::Ordering::Equal),
            };
            let o = match dir {
                Order::Asc => o,
                Order::Desc => o.reverse(),
            };
            if o != std::cmp::Ordering::Equal {
                return o;
            }
        }
        a.ins.cmp(&b.ins)
    });
}

/// The in-memory store.
#[derive(Clone, Default)]
pub struct MemoryStore {
    state: Arc<Mutex<MemState>>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl Store for MemoryStore {
    fn backend(&self) -> Backend {
        Backend::Memory
    }

    async fn begin(&self) -> StoreResult<Box<dyn Tx>> {
        let guard = self.state.clone().lock_owned().await;
        Ok(Box::new(MemTx {
            guard: Some(guard),
            undo: Vec::new(),
            now: micros(Utc::now()),
            next_sp: 0,
            savepoints: Vec::new(),
            aborted: false,
        }))
    }

    async fn ping(&self) -> StoreResult<()> {
        Ok(())
    }
}

pub struct MemTx {
    guard: Option<OwnedMutexGuard<MemState>>,
    undo: Vec<Undo>,
    /// Transaction time (PostgreSQL `now()`).
    now: DateTime<Utc>,
    next_sp: u32,
    /// Open savepoint ids, innermost last.
    savepoints: Vec<u32>,
    /// An operation failed: like PostgreSQL, refuse further work until a savepoint rollback.
    aborted: bool,
}

const ABORTED: &str =
    "current transaction is aborted, commands ignored until end of transaction block";

impl MemTx {
    fn check_open(&mut self) -> StoreResult<()> {
        if self.aborted {
            return Err(StoreError::Database(ABORTED.into()));
        }
        self.state().map(|_| ())
    }

    fn track<T>(&mut self, r: StoreResult<T>) -> StoreResult<T> {
        if r.is_err() {
            self.aborted = true;
        }
        r
    }

    fn state(&mut self) -> StoreResult<&mut MemState> {
        self.guard
            .as_deref_mut()
            .ok_or_else(|| StoreError::Database("transaction already finished".into()))
    }

    fn undo_to(&mut self, mark: usize) {
        if let Some(state) = self.guard.as_deref_mut() {
            while self.undo.len() > mark {
                if let Some(u) = self.undo.pop() {
                    state.apply_undo(u);
                }
            }
        }
    }

    fn fill_defaults(&mut self, t: &'static TableDef, mut row: Row) -> StoreResult<Row> {
        for c in t.columns {
            if row.contains(c.name)
                && !(c.default == ColDefault::Identity && row.get(c.name) == Some(&SqlValue::Null))
            {
                continue;
            }
            let v = match c.default {
                ColDefault::None => SqlValue::Null,
                ColDefault::Now => SqlValue::Timestamp(self.now),
                ColDefault::Identity => {
                    let state = self.state()?;
                    let n = state.identity.entry((t.name, c.name)).or_insert(0);
                    *n += 1;
                    SqlValue::Int(*n)
                }
                ColDefault::Text(s) => SqlValue::Text(s.into()),
                ColDefault::Int(i) => SqlValue::Int(i),
                ColDefault::Bool(b) => SqlValue::Bool(b),
            };
            row.set(c.name, v);
        }
        Ok(row)
    }

    fn normalize(row: Row) -> Row {
        let mut out = Row::new();
        for (k, v) in row.iter() {
            let v = match v {
                SqlValue::Timestamp(t) => SqlValue::Timestamp(micros(*t)),
                other => other.clone(),
            };
            out.set(k, v);
        }
        out
    }

    fn id_of(t: &TableDef, row: &Row) -> StoreResult<Uuid> {
        match row.get("id") {
            Some(SqlValue::Uuid(u)) => Ok(*u),
            _ => Err(StoreError::NotNullViolation {
                table: t.name.into(),
                column: "id".into(),
            }),
        }
    }

    /// Delete one row applying ON DELETE rules of referencing tables (recursively).
    fn delete_row(&mut self, t: &'static TableDef, id: Uuid) -> StoreResult<()> {
        // Referencing rows first (alembic tables and the Rust-owned ones).
        for child in schema::ALL_TABLES.iter().chain(schema::RUST_OWNED_TABLES) {
            for f in child.foreign_keys.iter().filter(|f| f.references == t.name) {
                let refs: Vec<Uuid> = {
                    let state = self.state()?;
                    state
                        .tables
                        .get(child.name)
                        .map(|tb| {
                            tb.rows
                                .iter()
                                .filter(|(cid, r)| {
                                    r.row.get(f.column) == Some(&SqlValue::Uuid(id))
                                        && !(child.name == t.name && **cid == id)
                                })
                                .map(|(cid, _)| *cid)
                                .collect()
                        })
                        .unwrap_or_default()
                };
                if refs.is_empty() {
                    continue;
                }
                match f.on_delete {
                    OnDelete::Restrict => {
                        return Err(StoreError::ForeignKeyViolation {
                            table: child.name.into(),
                            constraint: f.name.into(),
                        })
                    }
                    OnDelete::Cascade => {
                        for cid in refs {
                            self.delete_row(child, cid)?;
                        }
                    }
                    OnDelete::SetNull => {
                        for cid in refs {
                            let now = self.now;
                            let state = self.state()?;
                            if let Some(before) = state.row(child, cid).cloned() {
                                let mut after = before.clone();
                                after.row.set(f.column, SqlValue::Null);
                                if child.has_column("updated_at") {
                                    after.row.set("updated_at", SqlValue::Timestamp(now));
                                }
                                state.put(child, after);
                                self.undo.push(Undo::Updated {
                                    table: child,
                                    before,
                                });
                            }
                        }
                    }
                }
            }
        }
        let state = self.state()?;
        if let Some(before) = state.remove(t, id) {
            self.undo.push(Undo::Deleted { table: t, before });
        }
        Ok(())
    }
}

impl Drop for MemTx {
    fn drop(&mut self) {
        // Not committed: roll back.
        self.undo_to(0);
    }
}

impl MemTx {
    async fn insert_inner(&mut self, t: &'static TableDef, row: Row) -> StoreResult<Row> {
        check_row_types(t, &row)?;
        let row = Self::normalize(self.fill_defaults(t, row)?);
        check_row_types(t, &row)?;
        MemState::check_not_null(t, &row)?;
        let id = Self::id_of(t, &row)?;
        let state = self.state()?;
        if state.exists(t.name, id) {
            return Err(StoreError::UniqueViolation {
                table: t.name.into(),
                constraint: t.primary_key_name(),
            });
        }
        state.check_unique(t, id, &row)?;
        state.check_foreign_keys(t, &row)?;
        state.next_ins += 1;
        let ins = state.next_ins;
        state.put(
            t,
            MemRow {
                ins,
                row: row.clone(),
            },
        );
        self.undo.push(Undo::Inserted { table: t, id });
        Ok(row)
    }

    async fn get_inner(&mut self, t: &'static TableDef, id: Uuid) -> StoreResult<Option<Row>> {
        let state = self.state()?;
        Ok(state.row(t, id).map(|r| r.row.clone()))
    }

    async fn get_for_update_inner(
        &mut self,
        t: &'static TableDef,
        id: Uuid,
    ) -> StoreResult<Option<Row>> {
        self.get_inner(t, id).await
    }

    async fn select_inner(&mut self, t: &'static TableDef, q: &Query) -> StoreResult<Vec<Row>> {
        for f in &q.filters {
            if !t.has_column(f.column()) {
                return Err(StoreError::UnknownColumn {
                    table: t.name.into(),
                    column: f.column().into(),
                });
            }
        }
        for (c, _) in &q.order_by {
            if !t.has_column(c) {
                return Err(StoreError::UnknownColumn {
                    table: t.name.into(),
                    column: (*c).into(),
                });
            }
        }
        let state = self.state()?;
        let mut rows: Vec<MemRow> = state
            .tables
            .get(t.name)
            .map(|tb| {
                tb.rows
                    .values()
                    .filter(|r| matches(&r.row, &q.filters))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        sort_rows(&mut rows, &q.order_by);
        let mut out: Vec<Row> = rows.into_iter().map(|r| r.row).collect();
        if let Some(n) = q.limit {
            out.truncate(usize::try_from(n.max(0)).unwrap_or(usize::MAX));
        }
        Ok(out)
    }

    async fn update_inner(
        &mut self,
        t: &'static TableDef,
        id: Uuid,
        set: Row,
        expected_version: Option<i64>,
    ) -> StoreResult<UpdateOutcome> {
        check_row_types(t, &set)?;
        let now = self.now;
        let state = self.state()?;
        let Some(before) = state.row(t, id).cloned() else {
            return Ok(UpdateOutcome::NotFound);
        };
        let current_version = before.row.get("record_version").and_then(|v| match v {
            SqlValue::Int(i) => Some(*i),
            _ => None,
        });
        if let Some(expected) = expected_version {
            let actual = current_version.unwrap_or(0);
            if actual != expected {
                return Ok(UpdateOutcome::VersionMismatch { actual });
            }
        }
        let mut after = before.clone();
        after.row.merge(Self::normalize(set));
        if t.document {
            after.row.set(
                "record_version",
                SqlValue::Int(current_version.unwrap_or(1) + 1),
            );
        }
        if t.has_column("updated_at") {
            after.row.set("updated_at", SqlValue::Timestamp(now));
        }
        check_row_types(t, &after.row)?;
        MemState::check_not_null(t, &after.row)?;
        if after.row.get("id") != Some(&SqlValue::Uuid(id)) {
            return Err(StoreError::InvalidValue {
                table: t.name.into(),
                column: "id".into(),
                reason: "the primary key cannot be changed".into(),
            });
        }
        state.check_unique(t, id, &after.row)?;
        state.check_foreign_keys(t, &after.row)?;
        state.put(t, after.clone());
        self.undo.push(Undo::Updated { table: t, before });
        Ok(UpdateOutcome::Updated(after.row))
    }

    async fn update_where_inner(
        &mut self,
        t: &'static TableDef,
        filters: &[Filter],
        set: Row,
    ) -> StoreResult<u64> {
        check_row_types(t, &set)?;
        if set.contains("id") {
            return Err(StoreError::InvalidValue {
                table: t.name.into(),
                column: "id".into(),
                reason: "the primary key cannot be changed".into(),
            });
        }
        let now = self.now;
        let targets: Vec<MemRow> = {
            let state = self.state()?;
            state
                .tables
                .get(t.name)
                .map(|tb| {
                    tb.rows
                        .values()
                        .filter(|r| matches(&r.row, filters))
                        .cloned()
                        .collect()
                })
                .unwrap_or_default()
        };
        let set = Self::normalize(set);
        // Validate all first so a failure leaves nothing half-applied.
        let mut updated = Vec::with_capacity(targets.len());
        {
            let state = self.state()?;
            for before in &targets {
                let mut after = before.clone();
                after.row.merge(set.clone());
                if t.has_column("updated_at") {
                    after.row.set("updated_at", SqlValue::Timestamp(now));
                }
                MemState::check_not_null(t, &after.row)?;
                let id = Self::id_of(t, &after.row)?;
                state.check_unique(t, id, &after.row)?;
                state.check_foreign_keys(t, &after.row)?;
                updated.push(after);
            }
        }
        let n = updated.len() as u64;
        for (before, after) in targets.into_iter().zip(updated) {
            let state = self.state()?;
            state.put(t, after);
            self.undo.push(Undo::Updated { table: t, before });
        }
        Ok(n)
    }

    async fn delete_where_inner(
        &mut self,
        t: &'static TableDef,
        filters: &[Filter],
    ) -> StoreResult<u64> {
        let ids: Vec<Uuid> = {
            let state = self.state()?;
            state
                .tables
                .get(t.name)
                .map(|tb| {
                    tb.rows
                        .iter()
                        .filter(|(_, r)| matches(&r.row, filters))
                        .map(|(id, _)| *id)
                        .collect()
                })
                .unwrap_or_default()
        };
        let mark = self.undo.len();
        for id in &ids {
            if let Err(e) = self.delete_row(t, *id) {
                // A statement is atomic: undo this statement's partial work.
                self.undo_to(mark);
                return Err(e);
            }
        }
        Ok(ids.len() as u64)
    }
}

#[async_trait]
impl Tx for MemTx {
    fn backend(&self) -> Backend {
        Backend::Memory
    }

    async fn insert(&mut self, t: &'static TableDef, row: Row) -> StoreResult<Row> {
        self.check_open()?;
        let r = self.insert_inner(t, row).await;
        self.track(r)
    }

    async fn get(&mut self, t: &'static TableDef, id: Uuid) -> StoreResult<Option<Row>> {
        self.check_open()?;
        let r = self.get_inner(t, id).await;
        self.track(r)
    }

    async fn get_for_update(&mut self, t: &'static TableDef, id: Uuid) -> StoreResult<Option<Row>> {
        self.check_open()?;
        let r = self.get_for_update_inner(t, id).await;
        self.track(r)
    }

    async fn select(&mut self, t: &'static TableDef, q: &Query) -> StoreResult<Vec<Row>> {
        self.check_open()?;
        let r = self.select_inner(t, q).await;
        self.track(r)
    }

    async fn update(
        &mut self,
        t: &'static TableDef,
        id: Uuid,
        set: Row,
        expected_version: Option<i64>,
    ) -> StoreResult<UpdateOutcome> {
        self.check_open()?;
        let r = self.update_inner(t, id, set, expected_version).await;
        self.track(r)
    }

    async fn update_where(
        &mut self,
        t: &'static TableDef,
        filters: &[Filter],
        set: Row,
    ) -> StoreResult<u64> {
        self.check_open()?;
        let r = self.update_where_inner(t, filters, set).await;
        self.track(r)
    }

    async fn delete_where(&mut self, t: &'static TableDef, filters: &[Filter]) -> StoreResult<u64> {
        self.check_open()?;
        let r = self.delete_where_inner(t, filters).await;
        self.track(r)
    }

    async fn advisory_xact_lock(&mut self, _key: i64) -> StoreResult<()> {
        // Transactions are fully serialized by the store lock this one already holds.
        self.check_open()
    }

    async fn savepoint(&mut self) -> StoreResult<Savepoint> {
        self.check_open()?;
        self.next_sp += 1;
        self.savepoints.push(self.next_sp);
        Ok(Savepoint {
            id: self.next_sp,
            mark: self.undo.len(),
        })
    }

    async fn release_savepoint(&mut self, sp: Savepoint) -> StoreResult<()> {
        self.check_open()?;
        let pos = self
            .savepoints
            .iter()
            .rposition(|id| *id == sp.id)
            .ok_or_else(|| StoreError::Database(format!("savepoint {} does not exist", sp.id)))?;
        // Releasing a savepoint also releases the savepoints created after it.
        self.savepoints.truncate(pos);
        Ok(())
    }

    async fn rollback_to_savepoint(&mut self, sp: Savepoint) -> StoreResult<()> {
        let pos = self
            .savepoints
            .iter()
            .rposition(|id| *id == sp.id)
            .ok_or_else(|| StoreError::Database(format!("savepoint {} does not exist", sp.id)))?;
        self.undo_to(sp.mark);
        self.savepoints.truncate(pos);
        self.aborted = false;
        Ok(())
    }

    async fn commit(mut self: Box<Self>) -> StoreResult<()> {
        self.state()?;
        if self.aborted {
            // Same outcome as PostgreSQL (COMMIT of an aborted transaction rolls back), reported.
            self.undo_to(0);
            self.guard = None;
            return Err(StoreError::Database(
                "transaction was aborted by an earlier error and has been rolled back".into(),
            ));
        }
        self.undo.clear();
        self.guard = None;
        Ok(())
    }

    async fn rollback(mut self: Box<Self>) -> StoreResult<()> {
        self.undo_to(0);
        self.guard = None;
        Ok(())
    }
}
