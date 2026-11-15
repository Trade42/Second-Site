//! PostgreSQL `Store` (sqlx, runtime-checked queries only) and the alembic-compatible migration
//! runner.
//!
//! SQL is generated from the static table metadata; table and column names come only from
//! [`TableDef`]s, every value is a bind parameter.
//!
//! Migrations: `migrations/0001_*.sql` and `0002_*.sql` are alembic's own offline output for
//! revisions `0001` and `0002`. The runner keeps alembic's bookkeeping table (`alembic_version`),
//! so a database alembic already migrated is left untouched, and a database this runner migrated
//! is at head for alembic too.

use std::str::FromStr;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures_util::future::BoxFuture;
use serde_json::Value;
use sqlx::postgres::{PgArguments, PgConnectOptions, PgPool, PgPoolOptions, PgRow};
use sqlx::types::Json;
use sqlx::{Arguments as _, PgConnection, Postgres, Row as _, Transaction};
use uuid::Uuid;

use super::schema::{ColType, Column, TableDef};
use super::{
    check_row_types, Backend, Filter, Order, Query, Row, Savepoint, SqlValue, Store, StoreError,
    StoreResult, Tx, UpdateOutcome,
};

/// Alembic revisions, in order.
pub const MIGRATIONS: &[(&str, &str)] = &[
    (
        "0001",
        include_str!("../../migrations/0001_foundation_schema.sql"),
    ),
    (
        "0002",
        include_str!("../../migrations/0002_owner_channels.sql"),
    ),
];

/// Rust-owned tables (`migrations/rust-owned/`), NOT alembic revisions: [`migrate`] creates them
/// idempotently (`CREATE TABLE IF NOT EXISTS`) after the alembic revisions, in the same
/// transaction and under the same migration lock, and never records them in `alembic_version`.
/// Alembic does not know them, so the Python app's `alembic upgrade head` is unaffected.
pub const RUST_OWNED_MIGRATIONS: &[(&str, &str)] = &[(
    "nessie_simulation_records",
    include_str!("../../migrations/rust-owned/nessie_simulation_records.sql"),
)];

async fn ensure_rust_owned(conn: &mut PgConnection) -> Result<(), sqlx::Error> {
    for (_, sql) in RUST_OWNED_MIGRATIONS {
        sqlx::raw_sql(sql).execute(&mut *conn).await?;
    }
    Ok(())
}

/// Result of [`migrate`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MigrationOutcome {
    /// These revisions were applied (in order).
    Applied(Vec<&'static str>),
    /// Already at head; nothing ran.
    UpToDate,
    /// The database is at a revision this binary does not know (e.g. a newer alembic head);
    /// nothing ran.
    UnknownRevision(String),
}

fn map_sqlx(table: &str, e: sqlx::Error) -> StoreError {
    if let sqlx::Error::Database(db) = &e {
        let code = db.code().map(|c| c.into_owned()).unwrap_or_default();
        let t = db.table().unwrap_or(table).to_owned();
        let constraint = db.constraint().unwrap_or_default().to_owned();
        return match code.as_str() {
            "23505" => StoreError::UniqueViolation {
                table: t,
                constraint,
            },
            "23503" => StoreError::ForeignKeyViolation {
                table: t,
                constraint,
            },
            "23502" => StoreError::NotNullViolation {
                table: t,
                column: db.message().to_owned(),
            },
            "22001" => StoreError::ValueTooLong {
                table: t,
                column: db.message().to_owned(),
            },
            "22003" | "22P02" | "22P05" | "22021" | "22023" => StoreError::InvalidValue {
                table: t,
                column: String::new(),
                reason: db.message().to_owned(),
            },
            _ => StoreError::Database(format!("{code}: {}", db.message())),
        };
    }
    StoreError::Database(e.to_string())
}

/// Owned SQL text plus its bind arguments: executed inside a boxed future that owns both, so no
/// borrowed query crosses an await (that trips rustc's `Send` inference with sqlx executors).
struct Stmt {
    sql: String,
    args: PgArguments,
}

impl Stmt {
    fn new(sql: String) -> Self {
        Stmt {
            sql,
            args: PgArguments::default(),
        }
    }

    fn add<T>(&mut self, table: &TableDef, col: &str, v: T) -> StoreResult<()>
    where
        T: for<'q> sqlx::Encode<'q, Postgres> + sqlx::Type<Postgres> + Send,
    {
        self.args.add(v).map_err(|e| StoreError::InvalidValue {
            table: table.name.into(),
            column: col.into(),
            reason: e.to_string(),
        })
    }

    /// Bind a column value with the column's SQL type.
    fn bind(&mut self, table: &TableDef, col: &Column, v: &SqlValue) -> StoreResult<()> {
        super::check_value(table, col, v)?;
        let name = col.name;
        match (col.ty, v) {
            (ColType::Uuid, SqlValue::Uuid(u)) => self.add(table, name, *u),
            (ColType::Uuid, SqlValue::Null) => self.add(table, name, None::<Uuid>),
            (ColType::Varchar(_) | ColType::Text, SqlValue::Text(s)) => {
                self.add(table, name, s.clone())
            }
            (ColType::Varchar(_) | ColType::Text, SqlValue::Null) => {
                self.add(table, name, None::<String>)
            }
            // Bound as INT8; PostgreSQL applies the assignment cast to INTEGER columns and
            // compares across integer widths.
            (ColType::Int | ColType::BigInt, SqlValue::Int(i)) => self.add(table, name, *i),
            (ColType::Int | ColType::BigInt, SqlValue::Null) => self.add(table, name, None::<i64>),
            (ColType::Bool, SqlValue::Bool(b)) => self.add(table, name, *b),
            (ColType::Bool, SqlValue::Null) => self.add(table, name, None::<bool>),
            (ColType::Float, SqlValue::Float(f)) => self.add(table, name, *f),
            (ColType::Float, SqlValue::Null) => self.add(table, name, None::<f64>),
            (ColType::Json, SqlValue::Json(j)) => self.add(table, name, Json(j.clone())),
            (ColType::Json, SqlValue::Null) => self.add(table, name, None::<Json<Value>>),
            (ColType::Timestamp, SqlValue::Timestamp(t)) => self.add(table, name, *t),
            (ColType::Timestamp, SqlValue::Null) => self.add(table, name, None::<DateTime<Utc>>),
            _ => Err(StoreError::InvalidValue {
                table: table.name.into(),
                column: col.name.into(),
                reason: "value does not match the column type".into(),
            }),
        }
    }

    /// Filter values bind like column values, except that integer comparisons accept any i64
    /// (a cursor beyond the 32-bit range simply matches nothing, as in Python).
    fn bind_filter(&mut self, table: &TableDef, col: &Column, v: &SqlValue) -> StoreResult<()> {
        match (col.ty, v) {
            (ColType::Int, SqlValue::Int(i)) => self.add(table, col.name, *i),
            _ => self.bind(table, col, v),
        }
    }

    fn bind_filters(&mut self, table: &TableDef, filters: &[Filter]) -> StoreResult<()> {
        for f in filters {
            let c = column(table, f.column())?;
            match f {
                Filter::Eq(_, SqlValue::Null) | Filter::Ne(_, SqlValue::Null) => {}
                Filter::Eq(_, v) | Filter::Ne(_, v) | Filter::Gt(_, v) | Filter::Lt(_, v) => {
                    self.bind_filter(table, c, v)?
                }
            }
        }
        Ok(())
    }
}

fn run_fetch<'c>(
    conn: &'c mut PgConnection,
    table: &'static TableDef,
    stmt: Stmt,
) -> BoxFuture<'c, StoreResult<Vec<Row>>> {
    Box::pin(async move {
        let Stmt { sql, args } = stmt;
        let rows = sqlx::query_with(&sql, args)
            .fetch_all(&mut *conn)
            .await
            .map_err(|e| map_sqlx(table.name, e))?;
        rows.iter().map(|r| decode(table, r)).collect()
    })
}

fn run_exec<'c>(
    conn: &'c mut PgConnection,
    table: &'static TableDef,
    stmt: Stmt,
) -> BoxFuture<'c, StoreResult<u64>> {
    Box::pin(async move {
        let Stmt { sql, args } = stmt;
        sqlx::query_with(&sql, args)
            .execute(&mut *conn)
            .await
            .map(|d| d.rows_affected())
            .map_err(|e| map_sqlx(table.name, e))
    })
}

fn run_raw<'c>(conn: &'c mut PgConnection, sql: String) -> BoxFuture<'c, StoreResult<()>> {
    Box::pin(async move {
        // A plain `&str` without arguments runs over the simple-query protocol (several
        // statements allowed), like `raw_sql`, through the boxed `Executor` method.
        sqlx::Executor::execute(&mut *conn, sql.as_str())
            .await
            .map(|_| ())
            .map_err(|e| map_sqlx("", e))
    })
}

fn run_advisory_lock<'c>(conn: &'c mut PgConnection, key: i64) -> BoxFuture<'c, StoreResult<()>> {
    Box::pin(async move {
        let mut args = PgArguments::default();
        args.add(key)
            .map_err(|e| StoreError::Database(format!("bind advisory lock key: {e}")))?;
        sqlx::query_with("SELECT pg_advisory_xact_lock($1)", args)
            .execute(&mut *conn)
            .await
            .map(|_| ())
            .map_err(|e| map_sqlx("", e))
    })
}

fn decode(table: &TableDef, r: &PgRow) -> StoreResult<Row> {
    let mut row = Row::new();
    for (i, c) in table.columns.iter().enumerate() {
        let err = |e: sqlx::Error| StoreError::Decode(format!("{}.{}: {e}", table.name, c.name));
        let v: SqlValue = match c.ty {
            ColType::Uuid => r.try_get::<Option<Uuid>, _>(i).map_err(err)?.into(),
            ColType::Varchar(_) | ColType::Text => {
                r.try_get::<Option<String>, _>(i).map_err(err)?.into()
            }
            ColType::Int => r.try_get::<Option<i32>, _>(i).map_err(err)?.into(),
            ColType::BigInt => r.try_get::<Option<i64>, _>(i).map_err(err)?.into(),
            ColType::Bool => r.try_get::<Option<bool>, _>(i).map_err(err)?.into(),
            ColType::Float => r.try_get::<Option<f64>, _>(i).map_err(err)?.into(),
            ColType::Json => r
                .try_get::<Option<Json<Value>>, _>(i)
                .map_err(err)?
                .map(|j| j.0)
                .into(),
            ColType::Timestamp => r
                .try_get::<Option<DateTime<Utc>>, _>(i)
                .map_err(err)?
                .into(),
        };
        row.set(c.name, v);
    }
    Ok(row)
}

fn column<'t>(table: &'t TableDef, name: &str) -> StoreResult<&'t Column> {
    table.column(name).ok_or_else(|| StoreError::UnknownColumn {
        table: table.name.into(),
        column: name.into(),
    })
}

/// Append `WHERE ...` for `filters`, numbering parameters from `*n + 1`.
fn where_clause(table: &TableDef, filters: &[Filter], n: &mut usize) -> StoreResult<String> {
    let mut parts = Vec::with_capacity(filters.len());
    for f in filters {
        let c = column(table, f.column())?;
        let part = match f {
            Filter::Eq(_, SqlValue::Null) => format!("{} IS NULL", c.name),
            Filter::Ne(_, SqlValue::Null) => format!("{} IS NOT NULL", c.name),
            Filter::Eq(..) => {
                *n += 1;
                format!("{} = ${n}", c.name)
            }
            Filter::Ne(..) => {
                *n += 1;
                format!("{} <> ${n}", c.name)
            }
            Filter::Gt(..) => {
                *n += 1;
                format!("{} > ${n}", c.name)
            }
            Filter::Lt(..) => {
                *n += 1;
                format!("{} < ${n}", c.name)
            }
        };
        parts.push(part);
    }
    Ok(if parts.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", parts.join(" AND "))
    })
}

/// The PostgreSQL store.
#[derive(Clone)]
pub struct PgStore {
    pool: PgPool,
}

impl PgStore {
    /// Connect with a URL (SQLAlchemy `+driver` suffixes are accepted and ignored).
    pub async fn connect(url: &str) -> StoreResult<Self> {
        let opts = PgConnectOptions::from_str(&crate::config::normalize_database_url(url))
            .map_err(|e| StoreError::Database(format!("invalid DATABASE_URL: {e}")))?;
        Self::connect_with(opts, 10).await
    }

    pub async fn connect_with(opts: PgConnectOptions, max_connections: u32) -> StoreResult<Self> {
        let pool = PgPoolOptions::new()
            .max_connections(max_connections)
            .acquire_timeout(Duration::from_secs(10))
            .connect_with(opts)
            .await
            .map_err(|e| map_sqlx("", e))?;
        Ok(Self { pool })
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Apply pending alembic revisions (see [`migrate`]).
    pub async fn migrate(&self) -> StoreResult<MigrationOutcome> {
        migrate(&self.pool).await
    }
}

/// Bring the schema to alembic head, alembic-compatibly. Safe to run concurrently (advisory
/// lock per schema) and a no-op when already at head. Then (on every outcome, under the same
/// lock) create the [`RUST_OWNED_MIGRATIONS`] tables if they are missing, without touching
/// `alembic_version`.
pub async fn migrate(pool: &PgPool) -> StoreResult<MigrationOutcome> {
    let m = |e: sqlx::Error| StoreError::Migration(e.to_string());
    let mut tx = pool.begin().await.map_err(m)?;
    sqlx::query(
        "SELECT pg_advisory_xact_lock(hashtext('second-site-migrations:' || current_schema()))",
    )
    .execute(&mut *tx)
    .await
    .map_err(m)?;
    let exists: Option<String> = sqlx::query_scalar("SELECT to_regclass('alembic_version')::text")
        .fetch_one(&mut *tx)
        .await
        .map_err(m)?;
    if exists.is_none() {
        sqlx::raw_sql(
            "CREATE TABLE alembic_version (\n    version_num VARCHAR(32) NOT NULL, \n    \
             CONSTRAINT alembic_version_pkc PRIMARY KEY (version_num)\n)",
        )
        .execute(&mut *tx)
        .await
        .map_err(m)?;
    }
    let current: Option<String> = sqlx::query_scalar("SELECT version_num FROM alembic_version")
        .fetch_optional(&mut *tx)
        .await
        .map_err(m)?;
    let start = match &current {
        None => 0,
        Some(v) => match MIGRATIONS.iter().position(|(rev, _)| rev == v) {
            Some(i) => i + 1,
            None => {
                // The alembic tables are left alone; the Rust-owned ones only need `projects`.
                ensure_rust_owned(&mut tx).await.map_err(m)?;
                tx.commit().await.map_err(m)?;
                return Ok(MigrationOutcome::UnknownRevision(v.clone()));
            }
        },
    };
    if start == MIGRATIONS.len() {
        ensure_rust_owned(&mut tx).await.map_err(m)?;
        tx.commit().await.map_err(m)?;
        return Ok(MigrationOutcome::UpToDate);
    }
    let mut applied = Vec::new();
    let mut previous = current;
    for (rev, sql) in &MIGRATIONS[start..] {
        sqlx::raw_sql(sql).execute(&mut *tx).await.map_err(m)?;
        match &previous {
            None => sqlx::query("INSERT INTO alembic_version (version_num) VALUES ($1)")
                .bind(*rev)
                .execute(&mut *tx)
                .await
                .map_err(m)?,
            Some(prev) => {
                sqlx::query("UPDATE alembic_version SET version_num = $1 WHERE alembic_version.version_num = $2")
                    .bind(*rev)
                    .bind(prev.clone())
                    .execute(&mut *tx)
                    .await
                    .map_err(m)?
            }
        };
        previous = Some((*rev).to_owned());
        applied.push(*rev);
    }
    ensure_rust_owned(&mut tx).await.map_err(m)?;
    tx.commit().await.map_err(m)?;
    Ok(MigrationOutcome::Applied(applied))
}

#[async_trait]
impl Store for PgStore {
    fn backend(&self) -> Backend {
        Backend::Postgres
    }

    async fn begin(&self) -> StoreResult<Box<dyn Tx>> {
        let tx = self.pool.begin().await.map_err(|e| map_sqlx("", e))?;
        Ok(Box::new(PgTx {
            tx: Some(tx),
            next_sp: 0,
            savepoints: Vec::new(),
            aborted: false,
        }))
    }

    async fn ping(&self) -> StoreResult<()> {
        sqlx::query("select 1")
            .execute(&self.pool)
            .await
            .map(|_| ())
            .map_err(|e| map_sqlx("", e))
    }
}

pub struct PgTx {
    tx: Option<Transaction<'static, Postgres>>,
    next_sp: u32,
    savepoints: Vec<u32>,
    /// A statement failed: PostgreSQL ignores further commands until a savepoint rollback.
    aborted: bool,
}

const ABORTED: &str =
    "current transaction is aborted, commands ignored until end of transaction block";

impl PgTx {
    fn conn(&mut self) -> StoreResult<&mut PgConnection> {
        if self.aborted {
            return Err(StoreError::Database(ABORTED.into()));
        }
        self.tx
            .as_deref_mut()
            .ok_or_else(|| StoreError::Database("transaction already finished".into()))
    }

    fn track<T>(&mut self, r: StoreResult<T>) -> StoreResult<T> {
        if r.is_err() {
            self.aborted = true;
        }
        r
    }

    async fn fetch(&mut self, table: &'static TableDef, stmt: Stmt) -> StoreResult<Vec<Row>> {
        let conn = self.conn()?;
        let r = run_fetch(conn, table, stmt).await;
        self.track(r)
    }

    async fn exec(&mut self, table: &'static TableDef, stmt: Stmt) -> StoreResult<u64> {
        let conn = self.conn()?;
        let r = run_exec(conn, table, stmt).await;
        self.track(r)
    }

    async fn raw(&mut self, sql: &str) -> StoreResult<()> {
        let conn = self
            .tx
            .as_deref_mut()
            .ok_or_else(|| StoreError::Database("transaction already finished".into()))?;
        run_raw(conn, sql.to_owned()).await
    }

    async fn insert_inner(&mut self, t: &'static TableDef, row: Row) -> StoreResult<Row> {
        check_row_types(t, &row)?;
        let mut cols = Vec::new();
        let mut vals: Vec<(&'static Column, SqlValue)> = Vec::new();
        for c in t.columns {
            match row.get(c.name) {
                // An explicit NULL for an identity column means "generate it".
                Some(SqlValue::Null) if c.default == super::schema::ColDefault::Identity => {}
                Some(v) => {
                    cols.push(c.name);
                    vals.push((c, v.clone()));
                }
                None => match c.default {
                    super::schema::ColDefault::Text(s) => {
                        cols.push(c.name);
                        vals.push((c, SqlValue::Text(s.into())));
                    }
                    super::schema::ColDefault::Int(i) => {
                        cols.push(c.name);
                        vals.push((c, SqlValue::Int(i)));
                    }
                    super::schema::ColDefault::Bool(b) => {
                        cols.push(c.name);
                        vals.push((c, SqlValue::Bool(b)));
                    }
                    // Server defaults (now(), identity) and plain NULLs.
                    _ => {}
                },
            }
        }
        let placeholders: Vec<String> = (1..=cols.len()).map(|i| format!("${i}")).collect();
        let sql = format!(
            "INSERT INTO {} ({}) VALUES ({}) RETURNING {}",
            t.name,
            cols.join(", "),
            placeholders.join(", "),
            t.column_list()
        );
        let mut stmt = Stmt::new(sql);
        for (c, v) in &vals {
            stmt.bind(t, c, v)?;
        }
        let mut rows = self.fetch(t, stmt).await?;
        rows.pop()
            .ok_or_else(|| StoreError::Decode("INSERT returned no row".into()))
    }

    /// `lock`: `FOR NO KEY UPDATE`, the lock an `UPDATE` that changes no key column takes anyway
    /// (rust-server-7). It excludes every other locker and updater of the row, exactly as
    /// `FOR UPDATE` did, but not the `FOR KEY SHARE` lock that inserting a child row takes on its
    /// parent through the foreign key. With `FOR UPDATE`, a transaction holding the project row
    /// and waiting for the project's event lock (or an approval row) deadlocked against one that
    /// held that lock and then inserted an event (or any other child row) of the project.
    async fn get_inner(
        &mut self,
        t: &'static TableDef,
        id: Uuid,
        lock: bool,
    ) -> StoreResult<Option<Row>> {
        let sql = format!(
            "SELECT {} FROM {} WHERE id = $1{}",
            t.column_list(),
            t.name,
            if lock { " FOR NO KEY UPDATE" } else { "" }
        );
        let mut stmt = Stmt::new(sql);
        stmt.add(t, "id", id)?;
        Ok(self.fetch(t, stmt).await?.pop())
    }

    async fn select_inner(&mut self, t: &'static TableDef, query: &Query) -> StoreResult<Vec<Row>> {
        let mut n = 0;
        let mut sql = format!("SELECT {} FROM {}", t.column_list(), t.name);
        sql.push_str(&where_clause(t, &query.filters, &mut n)?);
        if !query.order_by.is_empty() {
            let mut parts = Vec::new();
            for (c, dir) in &query.order_by {
                let c = column(t, c)?;
                parts.push(format!(
                    "{} {}",
                    c.name,
                    match dir {
                        Order::Asc => "ASC",
                        Order::Desc => "DESC",
                    }
                ));
            }
            sql.push_str(&format!(" ORDER BY {}", parts.join(", ")));
        }
        if let Some(limit) = query.limit {
            sql.push_str(&format!(" LIMIT {}", limit.max(0)));
        }
        let mut stmt = Stmt::new(sql);
        stmt.bind_filters(t, &query.filters)?;
        self.fetch(t, stmt).await
    }

    async fn update_inner(
        &mut self,
        t: &'static TableDef,
        id: Uuid,
        set: Row,
        expected_version: Option<i64>,
    ) -> StoreResult<UpdateOutcome> {
        check_row_types(t, &set)?;
        if set.get("id").is_some_and(|v| v != &SqlValue::Uuid(id)) {
            return Err(StoreError::InvalidValue {
                table: t.name.into(),
                column: "id".into(),
                reason: "the primary key cannot be changed".into(),
            });
        }
        let mut assignments = Vec::new();
        let mut vals: Vec<(&'static Column, SqlValue)> = Vec::new();
        let mut n = 0;
        for (name, v) in set.iter() {
            if name == "id" || (t.document && name == "record_version") || name == "updated_at" {
                continue;
            }
            let c = column(t, name)?;
            n += 1;
            assignments.push(format!("{} = ${n}", c.name));
            vals.push((c, v.clone()));
        }
        if t.document {
            assignments.push("record_version = record_version + 1".into());
        }
        if t.has_column("updated_at") {
            assignments.push("updated_at = now()".into());
        }
        n += 1;
        let mut sql = format!(
            "UPDATE {} SET {} WHERE id = ${n}",
            t.name,
            assignments.join(", ")
        );
        if expected_version.is_some() {
            n += 1;
            sql.push_str(&format!(" AND record_version = ${n}"));
        }
        sql.push_str(&format!(" RETURNING {}", t.column_list()));
        let mut stmt = Stmt::new(sql);
        for (c, v) in &vals {
            stmt.bind(t, c, v)?;
        }
        stmt.add(t, "id", id)?;
        if let Some(v) = expected_version {
            stmt.add(t, "record_version", v)?;
        }
        if let Some(row) = self.fetch(t, stmt).await?.pop() {
            return Ok(UpdateOutcome::Updated(row));
        }
        match self.get_inner(t, id, false).await? {
            None => Ok(UpdateOutcome::NotFound),
            Some(row) => Ok(UpdateOutcome::VersionMismatch {
                actual: row.int("record_version").unwrap_or(0),
            }),
        }
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
        let mut assignments = Vec::new();
        let mut vals: Vec<(&'static Column, SqlValue)> = Vec::new();
        let mut n = 0;
        for (name, v) in set.iter() {
            if name == "updated_at" {
                continue;
            }
            let c = column(t, name)?;
            n += 1;
            assignments.push(format!("{} = ${n}", c.name));
            vals.push((c, v.clone()));
        }
        if t.has_column("updated_at") {
            assignments.push("updated_at = now()".into());
        }
        if assignments.is_empty() {
            return Ok(0);
        }
        let mut sql = format!("UPDATE {} SET {}", t.name, assignments.join(", "));
        sql.push_str(&where_clause(t, filters, &mut n)?);
        let mut stmt = Stmt::new(sql);
        for (c, v) in &vals {
            stmt.bind(t, c, v)?;
        }
        stmt.bind_filters(t, filters)?;
        self.exec(t, stmt).await
    }

    async fn delete_where_inner(
        &mut self,
        t: &'static TableDef,
        filters: &[Filter],
    ) -> StoreResult<u64> {
        let mut n = 0;
        let mut sql = format!("DELETE FROM {}", t.name);
        sql.push_str(&where_clause(t, filters, &mut n)?);
        let mut stmt = Stmt::new(sql);
        stmt.bind_filters(t, filters)?;
        self.exec(t, stmt).await
    }
}

#[async_trait]
impl Tx for PgTx {
    fn backend(&self) -> Backend {
        Backend::Postgres
    }

    async fn insert(&mut self, t: &'static TableDef, row: Row) -> StoreResult<Row> {
        let r = self.insert_inner(t, row).await;
        self.track(r)
    }

    async fn get(&mut self, t: &'static TableDef, id: Uuid) -> StoreResult<Option<Row>> {
        let r = self.get_inner(t, id, false).await;
        self.track(r)
    }

    async fn get_for_update(&mut self, t: &'static TableDef, id: Uuid) -> StoreResult<Option<Row>> {
        let r = self.get_inner(t, id, true).await;
        self.track(r)
    }

    async fn select(&mut self, t: &'static TableDef, q: &Query) -> StoreResult<Vec<Row>> {
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
        let r = self.update_inner(t, id, set, expected_version).await;
        self.track(r)
    }

    async fn update_where(
        &mut self,
        t: &'static TableDef,
        filters: &[Filter],
        set: Row,
    ) -> StoreResult<u64> {
        let r = self.update_where_inner(t, filters, set).await;
        self.track(r)
    }

    async fn delete_where(&mut self, t: &'static TableDef, filters: &[Filter]) -> StoreResult<u64> {
        let r = self.delete_where_inner(t, filters).await;
        self.track(r)
    }

    async fn advisory_xact_lock(&mut self, key: i64) -> StoreResult<()> {
        let conn = self.conn()?;
        let r = run_advisory_lock(conn, key).await;
        self.track(r)
    }

    async fn savepoint(&mut self) -> StoreResult<Savepoint> {
        self.conn()?;
        self.next_sp += 1;
        let id = self.next_sp;
        let r = self.raw(&format!("SAVEPOINT ss_sp_{id}")).await;
        self.track(r)?;
        self.savepoints.push(id);
        Ok(Savepoint { id, mark: 0 })
    }

    async fn release_savepoint(&mut self, sp: Savepoint) -> StoreResult<()> {
        self.conn()?;
        let pos = self
            .savepoints
            .iter()
            .rposition(|id| *id == sp.id)
            .ok_or_else(|| StoreError::Database(format!("savepoint {} does not exist", sp.id)))?;
        let r = self
            .raw(&format!("RELEASE SAVEPOINT ss_sp_{}", sp.id))
            .await;
        self.track(r)?;
        self.savepoints.truncate(pos);
        Ok(())
    }

    async fn rollback_to_savepoint(&mut self, sp: Savepoint) -> StoreResult<()> {
        let pos = self
            .savepoints
            .iter()
            .rposition(|id| *id == sp.id)
            .ok_or_else(|| StoreError::Database(format!("savepoint {} does not exist", sp.id)))?;
        // Allowed (and required) while the transaction is aborted.
        self.raw(&format!(
            "ROLLBACK TO SAVEPOINT ss_sp_{id}; RELEASE SAVEPOINT ss_sp_{id}",
            id = sp.id
        ))
        .await?;
        self.savepoints.truncate(pos);
        self.aborted = false;
        Ok(())
    }

    async fn commit(mut self: Box<Self>) -> StoreResult<()> {
        let tx = self
            .tx
            .take()
            .ok_or_else(|| StoreError::Database("transaction already finished".into()))?;
        if self.aborted {
            // PostgreSQL would silently turn this COMMIT into a ROLLBACK; say so instead.
            tx.rollback().await.map_err(|e| map_sqlx("", e))?;
            return Err(StoreError::Database(
                "transaction was aborted by an earlier error and has been rolled back".into(),
            ));
        }
        tx.commit().await.map_err(|e| map_sqlx("", e))
    }

    async fn rollback(mut self: Box<Self>) -> StoreResult<()> {
        match self.tx.take() {
            Some(tx) => tx.rollback().await.map_err(|e| map_sqlx("", e)),
            None => Ok(()),
        }
    }
}
