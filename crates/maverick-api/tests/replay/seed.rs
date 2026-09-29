//! Replays the SQL the Go test code ran (README §2.2) on one connection, so `begin` and
//! `commit` pair up. Each statement is prepared first and its arguments are bound by the
//! parameter types Postgres infers, from the Go values in the recording.
//!
//! - River statements are skipped: the Rust engine queues with graphile_worker.
//! - `CreateAPIKey` stores the hash of a key minted here, since the raw Go key is not
//!   recorded; requests that carry that Go key's hash send ours (README §2.1).
//! - Server-minted ids the test copied into a seed map to ours; timestamps shift from the
//!   recording's clock to ours.

use std::collections::HashMap;
use std::str::FromStr;

use chrono::{DateTime, Duration, NaiveDate, Utc};
use maverick_domain::keys;
use maverick_store::{Decimal, JsonRawValue};
use serde_json::Value;
use sqlx::postgres::{PgArguments, PgRow};
use sqlx::query::Query;
use sqlx::types::Json;
use sqlx::{
    AssertSqlSafe, Column, Either, Executor, PgConnection, Postgres, Row, SqlSafeStr,
    Statement as _, TypeInfo,
};

use super::script::Statement;
use super::tokens::Tokens;

/// River's statements: the Rust engine has no River tables.
const SKIPPED: [&str; 3] = [
    "JobInsertFastMany :many",
    "PGNotifyMany :exec",
    "QueueCreateOrSetUpdatedAt :one",
];

/// Raw keys minted for the Go keys, by the Go key's SHA-256 hex.
#[derive(Debug, Default)]
pub struct Keys(HashMap<String, String>);

impl Keys {
    pub fn raw(&self, hash_hex: &str) -> Option<&str> {
        self.0.get(hash_hex).map(String::as_str)
    }

    pub fn insert(&mut self, hash_hex: &str, raw: &str) {
        self.0.insert(hash_hex.to_owned(), raw.to_owned());
    }

    /// Our raw key for a Go key hash: deterministic, well formed, and never the Go key.
    fn mint(&mut self, hash_hex: &str) -> [u8; 32] {
        let bytes: [u8; 32] = hex(hash_hex).try_into().expect("32-byte key hash");
        let minted = keys::mint_key_from_bytes(&bytes);
        self.0.insert(hash_hex.to_owned(), minted.raw_key);
        minted.key_hash
    }
}

pub fn hex(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).expect("hex"))
        .collect()
}

pub fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// What a statement returned, for the lookups a later request needs.
#[derive(Debug, Default)]
pub struct Returned {
    /// Every text value of every returned row, in order.
    pub texts: Vec<String>,
    /// Every timestamptz value of every returned row, in order.
    pub timestamps: Vec<DateTime<Utc>>,
}

fn is_skipped(statement: &Statement) -> bool {
    SKIPPED.contains(&statement.name.as_str()) || statement.sql.trim_start().starts_with("LISTEN")
}

type PgQuery<'q> = Query<'q, Postgres, PgArguments>;

fn opt<T>(value: &Value, parse: impl FnOnce(&Value) -> Option<T>) -> Result<Option<T>, String> {
    if value.is_null() {
        return Ok(None);
    }
    parse(value)
        .map(Some)
        .ok_or_else(|| format!("cannot read {value}"))
}

/// The argument's bytes (`[]uint8`) or text, with mapped ids replaced when it is text.
fn text_of(arg: &Value, tokens: &Tokens) -> Result<Option<String>, String> {
    // A Go nil slice is `null` (its `hex` is empty): SQL NULL.
    if arg["json"].is_null() {
        return Ok(None);
    }
    if let Some(hex_text) = arg.get("hex").and_then(Value::as_str) {
        let bytes = hex(hex_text);
        let text =
            String::from_utf8(bytes).map_err(|_| "binary where text is expected".to_owned())?;
        return Ok(Some(tokens.apply(&text)));
    }
    opt(&arg["json"], |v| v.as_str().map(|s| tokens.apply(s)))
}

fn bind<'q>(
    query: PgQuery<'q>,
    pg_type: &str,
    arg: &Value,
    tokens: &Tokens,
    shift: Duration,
) -> Result<PgQuery<'q>, String> {
    let json = &arg["json"];
    Ok(match pg_type {
        "TEXT" | "VARCHAR" | "NAME" => query.bind(text_of(arg, tokens)?),
        "JSONB" | "JSON" => {
            let text = text_of(arg, tokens)?;
            let raw = text
                .map(|t| JsonRawValue::from_string(t).map(Json))
                .transpose()
                .map_err(|e| format!("jsonb argument: {e}"))?;
            query.bind(raw)
        }
        "BYTEA" => {
            let bytes = arg
                .get("hex")
                .and_then(Value::as_str)
                .filter(|_| !json.is_null())
                .map(hex);
            query.bind(bytes)
        }
        "TEXT[]" => query.bind(opt(json, |v| {
            v.as_array().map(|items| {
                items
                    .iter()
                    .map(|s| tokens.apply(s.as_str().unwrap_or_default()))
                    .collect::<Vec<String>>()
            })
        })?),
        "BOOL" => query.bind(opt(json, Value::as_bool)?),
        "INT2" => query.bind(opt(json, |v| {
            v.as_i64().and_then(|n| i16::try_from(n).ok())
        })?),
        "INT4" => query.bind(opt(json, |v| {
            v.as_i64().and_then(|n| i32::try_from(n).ok())
        })?),
        "INT8" => query.bind(opt(json, Value::as_i64)?),
        #[allow(clippy::cast_possible_truncation)]
        "FLOAT4" => query.bind(opt(json, |v| v.as_f64().map(|n| n as f32))?),
        "FLOAT8" => query.bind(opt(json, Value::as_f64)?),
        "NUMERIC" => query.bind(opt(json, |v| Decimal::from_str(&v.to_string()).ok())?),
        "DATE" => query.bind(opt(json, |v| {
            v.as_str()
                .and_then(|s| NaiveDate::parse_from_str(s, "%Y-%m-%d").ok())
        })?),
        "TIMESTAMPTZ" => query.bind(opt(json, |v| {
            v.as_str()
                .and_then(super::tokens::instant)
                .map(|at| at + shift)
        })?),
        other => return Err(format!("no binding for parameter type {other}")),
    })
}

fn returned(rows: &[PgRow]) -> Returned {
    let mut out = Returned::default();
    for row in rows {
        for column in row.columns() {
            match column.type_info().name() {
                "TEXT" | "VARCHAR" => {
                    if let Ok(Some(text)) = row.try_get::<Option<String>, _>(column.ordinal()) {
                        out.texts.push(text);
                    }
                }
                "TIMESTAMPTZ" => {
                    if let Ok(Some(at)) = row.try_get::<Option<DateTime<Utc>>, _>(column.ordinal())
                    {
                        out.timestamps.push(at);
                    }
                }
                _ => {}
            }
        }
    }
    out
}

/// The statement as it runs on the Rust schema. D18: `debug_captures.capture_key_version`
/// is required, and a Go capture was sealed with the one master key, version 1.
fn adapt(statement: &Statement) -> String {
    if statement.name == "CreateDebugCapture :one" {
        let sql = statement
            .sql
            .replacen(
                "nonce, expires_at)",
                "nonce, expires_at, capture_key_version)",
                1,
            )
            .replacen("$6, $7)", "$6, $7, 1)", 1);
        assert_ne!(sql, statement.sql, "CreateDebugCapture adapts");
        return sql;
    }
    statement.sql.clone()
}

/// Runs one recorded statement. `shift` moves a recorded timestamp onto our clock.
pub async fn apply(
    conn: &mut PgConnection,
    statement: &Statement,
    tokens: &Tokens,
    keys: &mut Keys,
    shift: Duration,
) -> Result<Returned, String> {
    if is_skipped(statement) {
        return Ok(Returned::default());
    }
    let mut args = statement.args.clone();
    if statement.name == "CreateAPIKey :one" {
        let hash_hex = args[2]["hex"].as_str().expect("key_hash hex").to_owned();
        let ours = keys.mint(&hash_hex);
        args[2] = serde_json::json!({"go_type": "[]uint8", "hex": to_hex(&ours), "json": "minted"});
    }
    let sql = adapt(statement);
    let prepared = conn
        .prepare(AssertSqlSafe(sql).into_sql_str())
        .await
        .map_err(|e| format!("prepare: {e}"))?;
    let types: Vec<String> = match prepared.parameters() {
        Some(Either::Left(types)) => types.iter().map(|t| t.name().to_owned()).collect(),
        Some(Either::Right(n)) => vec!["TEXT".to_owned(); n],
        None => Vec::new(),
    };
    if types.len() != args.len() {
        return Err(format!(
            "{} parameters, {} arguments",
            types.len(),
            args.len()
        ));
    }
    let mut query = prepared.query();
    for (pg_type, arg) in types.iter().zip(&args) {
        query = bind(query, pg_type, arg, tokens, shift)?;
    }
    let rows = query
        .fetch_all(&mut *conn)
        .await
        .map_err(|e| format!("execute: {e}"))?;
    Ok(returned(&rows))
}
