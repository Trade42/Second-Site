//! The `Store` abstraction: one trait, two implementations (PostgreSQL via sqlx, and in-memory),
//! table-driven by static metadata in [`schema`].
//!
//! - A request opens one transaction with [`Store::begin`], works through `&mut dyn Tx`, and calls
//!   [`Tx::commit`] on success. Dropping a transaction without committing rolls it back (both
//!   implementations), so `?` on an error path is a rollback.
//! - Nested savepoints: [`Tx::savepoint`], [`Tx::release_savepoint`], [`Tx::rollback_to_savepoint`].
//! - Unique, primary-key, not-null and foreign-key constraints are enforced by both
//!   implementations and reported as typed [`StoreError`]s.
//! - Identity columns (`activity_events.seq`) increase monotonically and are never reused, even
//!   after a rollback (as PostgreSQL sequences).
//! - Rows keep the `registry.py` shape: the validated contract document in `data` plus indexed
//!   columns. Adding a table means adding a [`schema::TableDef`] (and its migration); the store core
//!   does not change.

pub mod comms;
pub mod memory;
pub mod postgres;
pub mod repo;
pub mod schema;

use std::collections::BTreeMap;
use std::fmt;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::Value;
use uuid::Uuid;

pub use schema::{ColType, Column, TableDef};

/// Which implementation a store is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Postgres,
    Memory,
}

impl Backend {
    pub fn as_str(&self) -> &'static str {
        match self {
            Backend::Postgres => "postgres",
            Backend::Memory => "memory",
        }
    }
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum StoreError {
    #[error("unique constraint {constraint} violated on {table}")]
    UniqueViolation { table: String, constraint: String },
    #[error("foreign key constraint {constraint} violated on {table}")]
    ForeignKeyViolation { table: String, constraint: String },
    #[error("null value in column {column} of {table} violates not-null constraint")]
    NotNullViolation { table: String, column: String },
    #[error("value too long for column {column} of {table}")]
    ValueTooLong { table: String, column: String },
    #[error("invalid value for column {column} of {table}: {reason}")]
    InvalidValue {
        table: String,
        column: String,
        reason: String,
    },
    #[error("unknown column {column} on {table}")]
    UnknownColumn { table: String, column: String },
    #[error("could not decode row: {0}")]
    Decode(String),
    #[error("database error: {0}")]
    Database(String),
    #[error("migration error: {0}")]
    Migration(String),
}

impl StoreError {
    /// A short class name (`/ready` reports `error: <kind>`; failed tasks record it).
    pub fn kind(&self) -> &'static str {
        match self {
            StoreError::UniqueViolation { .. }
            | StoreError::ForeignKeyViolation { .. }
            | StoreError::NotNullViolation { .. } => "IntegrityError",
            StoreError::ValueTooLong { .. } | StoreError::InvalidValue { .. } => "DataError",
            StoreError::UnknownColumn { .. } | StoreError::Decode(_) => "ProgrammingError",
            StoreError::Database(_) => "OperationalError",
            StoreError::Migration(_) => "MigrationError",
        }
    }

    pub fn is_unique_violation(&self, constraint: &str) -> bool {
        matches!(self, StoreError::UniqueViolation { constraint: c, .. } if c == constraint)
    }
}

pub type StoreResult<T> = Result<T, StoreError>;

/// A column value.
#[derive(Debug, Clone, PartialEq)]
pub enum SqlValue {
    Null,
    Uuid(Uuid),
    Text(String),
    Int(i64),
    Bool(bool),
    Float(f64),
    Json(Value),
    Timestamp(DateTime<Utc>),
}

impl SqlValue {
    pub fn is_null(&self) -> bool {
        matches!(self, SqlValue::Null)
    }

    fn type_name(&self) -> &'static str {
        match self {
            SqlValue::Null => "null",
            SqlValue::Uuid(_) => "uuid",
            SqlValue::Text(_) => "text",
            SqlValue::Int(_) => "integer",
            SqlValue::Bool(_) => "boolean",
            SqlValue::Float(_) => "float",
            SqlValue::Json(_) => "json",
            SqlValue::Timestamp(_) => "timestamp",
        }
    }
}

impl fmt::Display for SqlValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SqlValue::Null => f.write_str("NULL"),
            SqlValue::Uuid(u) => write!(f, "{u}"),
            SqlValue::Text(s) => write!(f, "{s:?}"),
            SqlValue::Int(i) => write!(f, "{i}"),
            SqlValue::Bool(b) => write!(f, "{b}"),
            SqlValue::Float(x) => write!(f, "{x}"),
            SqlValue::Json(v) => write!(f, "{v}"),
            SqlValue::Timestamp(t) => write!(f, "{}", t.to_rfc3339()),
        }
    }
}

macro_rules! sql_from {
    ($t:ty, $v:ident, $e:expr) => {
        impl From<$t> for SqlValue {
            fn from($v: $t) -> Self {
                $e
            }
        }
        impl From<Option<$t>> for SqlValue {
            fn from(o: Option<$t>) -> Self {
                match o {
                    Some($v) => $e,
                    None => SqlValue::Null,
                }
            }
        }
    };
}

sql_from!(Uuid, v, SqlValue::Uuid(v));
sql_from!(String, v, SqlValue::Text(v));
sql_from!(&str, v, SqlValue::Text(v.to_owned()));
sql_from!(i64, v, SqlValue::Int(v));
sql_from!(i32, v, SqlValue::Int(i64::from(v)));
sql_from!(bool, v, SqlValue::Bool(v));
sql_from!(f64, v, SqlValue::Float(v));
sql_from!(Value, v, SqlValue::Json(v));
sql_from!(DateTime<Utc>, v, SqlValue::Timestamp(v));

/// A row: column name to value. Store results always carry every column of the table.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Row {
    values: BTreeMap<&'static str, SqlValue>,
}

impl Row {
    pub fn new() -> Self {
        Self::default()
    }

    /// Builder: set a column.
    pub fn with(mut self, col: &'static str, v: impl Into<SqlValue>) -> Self {
        self.values.insert(col, v.into());
        self
    }

    pub fn set(&mut self, col: &'static str, v: impl Into<SqlValue>) {
        self.values.insert(col, v.into());
    }

    pub fn get(&self, col: &str) -> Option<&SqlValue> {
        self.values.get(col)
    }

    pub fn contains(&self, col: &str) -> bool {
        self.values.contains_key(col)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&'static str, &SqlValue)> {
        self.values.iter().map(|(k, v)| (*k, v))
    }

    pub fn merge(&mut self, other: Row) {
        for (k, v) in other.values {
            self.values.insert(k, v);
        }
    }

    fn missing(col: &str) -> StoreError {
        StoreError::Decode(format!("column {col} missing from row"))
    }

    fn mismatch(col: &str, want: &str, got: &SqlValue) -> StoreError {
        StoreError::Decode(format!(
            "column {col}: expected {want}, found {}",
            got.type_name()
        ))
    }

    pub fn uuid(&self, col: &str) -> StoreResult<Uuid> {
        match self.get(col) {
            Some(SqlValue::Uuid(u)) => Ok(*u),
            Some(v) => Err(Self::mismatch(col, "uuid", v)),
            None => Err(Self::missing(col)),
        }
    }

    pub fn opt_uuid(&self, col: &str) -> StoreResult<Option<Uuid>> {
        match self.get(col) {
            Some(SqlValue::Uuid(u)) => Ok(Some(*u)),
            Some(SqlValue::Null) => Ok(None),
            Some(v) => Err(Self::mismatch(col, "uuid", v)),
            None => Err(Self::missing(col)),
        }
    }

    pub fn text(&self, col: &str) -> StoreResult<String> {
        match self.get(col) {
            Some(SqlValue::Text(s)) => Ok(s.clone()),
            Some(v) => Err(Self::mismatch(col, "text", v)),
            None => Err(Self::missing(col)),
        }
    }

    pub fn opt_text(&self, col: &str) -> StoreResult<Option<String>> {
        match self.get(col) {
            Some(SqlValue::Text(s)) => Ok(Some(s.clone())),
            Some(SqlValue::Null) => Ok(None),
            Some(v) => Err(Self::mismatch(col, "text", v)),
            None => Err(Self::missing(col)),
        }
    }

    pub fn int(&self, col: &str) -> StoreResult<i64> {
        match self.get(col) {
            Some(SqlValue::Int(i)) => Ok(*i),
            Some(v) => Err(Self::mismatch(col, "integer", v)),
            None => Err(Self::missing(col)),
        }
    }

    pub fn boolean(&self, col: &str) -> StoreResult<bool> {
        match self.get(col) {
            Some(SqlValue::Bool(b)) => Ok(*b),
            Some(v) => Err(Self::mismatch(col, "boolean", v)),
            None => Err(Self::missing(col)),
        }
    }

    pub fn json(&self, col: &str) -> StoreResult<Value> {
        match self.get(col) {
            Some(SqlValue::Json(v)) => Ok(v.clone()),
            Some(v) => Err(Self::mismatch(col, "json", v)),
            None => Err(Self::missing(col)),
        }
    }

    pub fn timestamp(&self, col: &str) -> StoreResult<DateTime<Utc>> {
        match self.get(col) {
            Some(SqlValue::Timestamp(t)) => Ok(*t),
            Some(v) => Err(Self::mismatch(col, "timestamp", v)),
            None => Err(Self::missing(col)),
        }
    }
}

/// A `WHERE` condition. `Eq(col, Null)` means `IS NULL` (as SQLAlchemy's `col == None`).
#[derive(Debug, Clone, PartialEq)]
pub enum Filter {
    Eq(&'static str, SqlValue),
    Ne(&'static str, SqlValue),
    Gt(&'static str, SqlValue),
    Lt(&'static str, SqlValue),
}

impl Filter {
    pub fn column(&self) -> &'static str {
        match self {
            Filter::Eq(c, _) | Filter::Ne(c, _) | Filter::Gt(c, _) | Filter::Lt(c, _) => c,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Order {
    Asc,
    Desc,
}

/// A `SELECT` over one table.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Query {
    pub filters: Vec<Filter>,
    pub order_by: Vec<(&'static str, Order)>,
    pub limit: Option<i64>,
}

impl Query {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn eq(mut self, col: &'static str, v: impl Into<SqlValue>) -> Self {
        self.filters.push(Filter::Eq(col, v.into()));
        self
    }

    pub fn ne(mut self, col: &'static str, v: impl Into<SqlValue>) -> Self {
        self.filters.push(Filter::Ne(col, v.into()));
        self
    }

    pub fn gt(mut self, col: &'static str, v: impl Into<SqlValue>) -> Self {
        self.filters.push(Filter::Gt(col, v.into()));
        self
    }

    pub fn lt(mut self, col: &'static str, v: impl Into<SqlValue>) -> Self {
        self.filters.push(Filter::Lt(col, v.into()));
        self
    }

    pub fn order_asc(mut self, col: &'static str) -> Self {
        self.order_by.push((col, Order::Asc));
        self
    }

    pub fn order_desc(mut self, col: &'static str) -> Self {
        self.order_by.push((col, Order::Desc));
        self
    }

    pub fn limit(mut self, n: i64) -> Self {
        self.limit = Some(n);
        self
    }
}

/// Outcome of [`Tx::update`].
#[derive(Debug, Clone, PartialEq)]
pub enum UpdateOutcome {
    Updated(Row),
    NotFound,
    /// `expected_version` did not match; carries the stored version.
    VersionMismatch {
        actual: i64,
    },
}

/// A savepoint inside a transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Savepoint {
    pub(crate) id: u32,
    pub(crate) mark: usize,
}

/// One database transaction.
#[async_trait]
pub trait Tx: Send {
    fn backend(&self) -> Backend;

    /// Insert a row; missing columns take their defaults. Returns the stored row (with generated
    /// identity values and timestamps).
    async fn insert(&mut self, table: &'static TableDef, row: Row) -> StoreResult<Row>;

    async fn get(&mut self, table: &'static TableDef, id: Uuid) -> StoreResult<Option<Row>>;

    /// `SELECT ... FOR NO KEY UPDATE`: other transactions updating, deleting or locking the row
    /// wait until this one ends; inserting a row that references it (whose foreign-key check
    /// takes `FOR KEY SHARE` on it) does not. Nothing may rely on this lock blocking such inserts:
    /// that was the deadlock of rust-server-7 (see the lock order in `services::events`). (The
    /// in-memory store runs one transaction at a time, so every read is locked.)
    async fn get_for_update(
        &mut self,
        table: &'static TableDef,
        id: Uuid,
    ) -> StoreResult<Option<Row>>;

    async fn select(&mut self, table: &'static TableDef, query: &Query) -> StoreResult<Vec<Row>>;

    /// Update one row by id. On document tables `record_version` is incremented; when
    /// `expected_version` is given the update only applies if the stored version equals it.
    /// `updated_at` is set to the transaction time when the table has it.
    async fn update(
        &mut self,
        table: &'static TableDef,
        id: Uuid,
        set: Row,
        expected_version: Option<i64>,
    ) -> StoreResult<UpdateOutcome>;

    /// Bulk `UPDATE ... WHERE` (no version bump; `updated_at` refreshed). Returns affected rows.
    async fn update_where(
        &mut self,
        table: &'static TableDef,
        filters: &[Filter],
        set: Row,
    ) -> StoreResult<u64>;

    /// `DELETE ... WHERE`, applying the foreign keys' `ON DELETE` rules. Returns deleted rows.
    async fn delete_where(
        &mut self,
        table: &'static TableDef,
        filters: &[Filter],
    ) -> StoreResult<u64>;

    /// `pg_advisory_xact_lock(key)`: wait until no other transaction holds the advisory lock
    /// `key`, then hold it until this transaction ends (re-entrant; a rollback to a savepoint
    /// taken before the lock releases it). The in-memory store serializes whole transactions
    /// already, so there it is a no-op.
    async fn advisory_xact_lock(&mut self, key: i64) -> StoreResult<()>;

    async fn savepoint(&mut self) -> StoreResult<Savepoint>;
    async fn release_savepoint(&mut self, sp: Savepoint) -> StoreResult<()>;
    async fn rollback_to_savepoint(&mut self, sp: Savepoint) -> StoreResult<()>;

    async fn commit(self: Box<Self>) -> StoreResult<()>;
    async fn rollback(self: Box<Self>) -> StoreResult<()>;
}

/// A database.
#[async_trait]
pub trait Store: Send + Sync {
    fn backend(&self) -> Backend;
    async fn begin(&self) -> StoreResult<Box<dyn Tx>>;
    /// `select 1` (used by `/ready`).
    async fn ping(&self) -> StoreResult<()>;
}

/// Validate a row against the table metadata (column names and value types).
pub(crate) fn check_row_types(table: &TableDef, row: &Row) -> StoreResult<()> {
    for (name, v) in row.iter() {
        let col = table
            .column(name)
            .ok_or_else(|| StoreError::UnknownColumn {
                table: table.name.into(),
                column: name.into(),
            })?;
        check_value(table, col, v)?;
    }
    Ok(())
}

pub(crate) fn check_value(table: &TableDef, col: &Column, v: &SqlValue) -> StoreResult<()> {
    let ok = matches!(
        (col.ty, v),
        (_, SqlValue::Null)
            | (ColType::Uuid, SqlValue::Uuid(_))
            | (ColType::Varchar(_) | ColType::Text, SqlValue::Text(_))
            | (ColType::Int | ColType::BigInt, SqlValue::Int(_))
            | (ColType::Bool, SqlValue::Bool(_))
            | (ColType::Float, SqlValue::Float(_))
            | (ColType::Json, SqlValue::Json(_))
            | (ColType::Timestamp, SqlValue::Timestamp(_))
    );
    if !ok {
        return Err(StoreError::InvalidValue {
            table: table.name.into(),
            column: col.name.into(),
            reason: format!("expected {:?}, got {}", col.ty, v.type_name()),
        });
    }
    match (col.ty, v) {
        (ColType::Varchar(n), SqlValue::Text(s)) if s.chars().count() > n as usize => {
            Err(StoreError::ValueTooLong {
                table: table.name.into(),
                column: col.name.into(),
            })
        }
        (ColType::Int, SqlValue::Int(i)) if i32::try_from(*i).is_err() => {
            Err(StoreError::InvalidValue {
                table: table.name.into(),
                column: col.name.into(),
                reason: "integer out of range".into(),
            })
        }
        (ColType::Text | ColType::Varchar(_), SqlValue::Text(s)) if s.contains('\0') => {
            Err(StoreError::InvalidValue {
                table: table.name.into(),
                column: col.name.into(),
                reason: "text may not contain NUL characters".into(),
            })
        }
        // PostgreSQL rejects `\u0000` inside jsonb strings; refuse it in memory too.
        (ColType::Json, SqlValue::Json(j)) if json_has_nul(j) => Err(StoreError::InvalidValue {
            table: table.name.into(),
            column: col.name.into(),
            reason: "unsupported Unicode escape sequence \\u0000 in jsonb".into(),
        }),
        _ => Ok(()),
    }
}

fn json_has_nul(v: &Value) -> bool {
    match v {
        Value::String(s) => s.contains('\0'),
        Value::Array(a) => a.iter().any(json_has_nul),
        Value::Object(m) => m.iter().any(|(k, v)| k.contains('\0') || json_has_nul(v)),
        _ => false,
    }
}

/// Truncate to microseconds (PostgreSQL `timestamptz` precision).
pub(crate) fn micros(t: DateTime<Utc>) -> DateTime<Utc> {
    use chrono::Timelike;
    let n = t.nanosecond() / 1_000 * 1_000;
    t.with_nanosecond(n).unwrap_or(t)
}

/// Run `f` inside a savepoint: released on `Ok`, rolled back on `Err`.
///
/// ```text
/// let out = with_savepoint(tx, |tx| Box::pin(async move { do_work(tx).await })).await?;
/// ```
pub async fn with_savepoint<T, E, F>(tx: &mut dyn Tx, f: F) -> Result<T, E>
where
    E: From<StoreError>,
    F: for<'a> FnOnce(&'a mut dyn Tx) -> futures_util::future::BoxFuture<'a, Result<T, E>>,
{
    let sp = tx.savepoint().await?;
    match f(tx).await {
        Ok(v) => {
            tx.release_savepoint(sp).await?;
            Ok(v)
        }
        Err(e) => {
            tx.rollback_to_savepoint(sp).await?;
            Err(e)
        }
    }
}
