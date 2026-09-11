//! DOMO connector implementation via the REST query API.
//!
//! DOMO exposes a single-endpoint `POST /query/v1/execute/{datasetId}`
//! that takes a SQL body and returns a JSON response with explicit
//! per-column type metadata:
//!
//! ```json
//! {
//!   "columns":  ["id", "name", "amount", "ts"],
//!   "metadata": [{"type":"LONG"}, {"type":"STRING"},
//!                {"type":"DECIMAL"}, {"type":"DATETIME"}],
//!   "rows":     [[1, "alpha", 10.0, "2026-04-22 12:34:56"], ...]
//! }
//! ```
//!
//! Schema introspection uses `GET /v1/datasets/{datasetId}` fetched once
//! at construction time and cached.  Every other method is a single HTTP
//! round-trip authenticated with the `X-DOMO-Developer-Token` header.

#![cfg(feature = "domo")]

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use agentic_core::result::{
    CellValue, ColumnSpec, QueryResult, QueryRow, TypedDataType, TypedRowError, TypedRowStream,
    TypedValue,
};

use crate::connector::{
    ColumnStats, ConnectorError, DatabaseConnector, ExecutionResult, ResultSummary,
    SchemaColumnInfo, SchemaInfo, SchemaTableInfo, SqlDialect,
};

// ── Wire shapes ─────────────────────────────────────────────────────────────

#[derive(Serialize, Debug)]
struct ExecuteQueryRequest<'a> {
    sql: &'a str,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct ColumnMetadata {
    r#type: String,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct ExecuteQueryResponse {
    #[serde(default)]
    columns: Vec<String>,
    #[serde(default)]
    metadata: Vec<ColumnMetadata>,
    #[serde(default)]
    rows: Vec<Vec<Value>>,
}

// ── Type mapping ────────────────────────────────────────────────────────────

/// Map a DOMO `metadata.type` tag (`"LONG"`, `"STRING"`, `"DECIMAL"`,
/// `"BOOLEAN"`, `"DATE"`, `"DATETIME"`) to a [`TypedDataType`]. DOMO
/// documents the canonical set inline; anything else is rendered as
/// [`TypedDataType::Unknown`] and stringified per-cell.
pub(crate) fn domo_type_to_typed(t: &str) -> TypedDataType {
    match t.to_ascii_uppercase().as_str() {
        "LONG" => TypedDataType::Int64,
        "DOUBLE" => TypedDataType::Float64,
        // DOMO doesn't expose decimal precision/scale in its type metadata, so
        // we use Text to avoid misleading consumers with a hardcoded scale: 0.
        "DECIMAL" => TypedDataType::Text,
        "BOOLEAN" | "BOOL" => TypedDataType::Bool,
        "STRING" | "VARCHAR" | "TEXT" => TypedDataType::Text,
        "DATE" => TypedDataType::Date,
        "DATETIME" | "TIMESTAMP" => TypedDataType::Timestamp,
        _ => TypedDataType::Unknown,
    }
}

// ── Value helpers ───────────────────────────────────────────────────────────

fn json_to_cell(v: &Value) -> CellValue {
    match v {
        Value::Null => CellValue::Null,
        Value::Number(n) => CellValue::Number(n.as_f64().unwrap_or(0.0)),
        Value::Bool(b) => CellValue::Number(if *b { 1.0 } else { 0.0 }),
        Value::String(s) => {
            if let Ok(n) = s.parse::<f64>() {
                CellValue::Number(n)
            } else {
                CellValue::Text(s.clone())
            }
        }
        other => CellValue::Text(other.to_string()),
    }
}

/// Decode one JSON cell into a [`TypedValue`] according to the column spec.
pub(crate) fn parse_domo_cell(v: &Value, col: &ColumnSpec) -> Result<TypedValue, TypedRowError> {
    if v.is_null() {
        return Ok(TypedValue::Null);
    }

    fn mapping_err(col: &ColumnSpec, v: &Value, detail: impl std::fmt::Display) -> TypedRowError {
        TypedRowError::TypeMappingError {
            column: col.name.clone(),
            native_type: format!("{:?}", col.data_type),
            message: format!("could not decode '{v}': {detail}"),
        }
    }

    match &col.data_type {
        TypedDataType::Bool => match v {
            Value::Bool(b) => Ok(TypedValue::Bool(*b)),
            Value::Number(n) => Ok(TypedValue::Bool(n.as_i64().unwrap_or(0) != 0)),
            Value::String(s) => match s.to_ascii_lowercase().as_str() {
                "true" | "1" => Ok(TypedValue::Bool(true)),
                "false" | "0" => Ok(TypedValue::Bool(false)),
                _ => Err(mapping_err(col, v, "unrecognised bool literal")),
            },
            _ => Err(mapping_err(col, v, "expected bool")),
        },
        TypedDataType::Int32 => number_as_i64(v)
            .and_then(|n| i32::try_from(n).ok())
            .map(TypedValue::Int32)
            .ok_or_else(|| mapping_err(col, v, "not a 32-bit integer")),
        TypedDataType::Int64 => number_as_i64(v)
            .map(TypedValue::Int64)
            .ok_or_else(|| mapping_err(col, v, "not a 64-bit integer")),
        TypedDataType::Float64 => number_as_f64(v)
            .map(TypedValue::Float64)
            .ok_or_else(|| mapping_err(col, v, "not a number")),
        TypedDataType::Text => match v {
            Value::String(s) => Ok(TypedValue::Text(s.clone())),
            other => Ok(TypedValue::Text(other.to_string())),
        },
        TypedDataType::Bytes => match v {
            Value::String(s) => Ok(TypedValue::Bytes(s.as_bytes().to_vec())),
            other => Ok(TypedValue::Bytes(other.to_string().into_bytes())),
        },
        TypedDataType::Date => v
            .as_str()
            .and_then(parse_date)
            .map(TypedValue::Date)
            .ok_or_else(|| mapping_err(col, v, "expected YYYY-MM-DD")),
        TypedDataType::Timestamp => v
            .as_str()
            .and_then(parse_timestamp_micros)
            .map(TypedValue::Timestamp)
            .ok_or_else(|| mapping_err(col, v, "expected YYYY-MM-DD HH:MM:SS[.fff]")),
        TypedDataType::Decimal { .. } => {
            let s = match v {
                Value::String(s) => s.clone(),
                Value::Number(n) => n.to_string(),
                other => other.to_string(),
            };
            Ok(TypedValue::Decimal(s))
        }
        TypedDataType::Json => Ok(TypedValue::Json(v.clone())),
        TypedDataType::Unknown => match v {
            Value::String(s) => Ok(TypedValue::Text(s.clone())),
            other => Ok(TypedValue::Text(other.to_string())),
        },
    }
}

fn number_as_i64(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

fn number_as_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

// ── Date / timestamp parsing (dep-free, mirrors airhouse_typed) ─────────────

fn parse_date(s: &str) -> Option<i32> {
    let (y, m, d) = split_ymd(s)?;
    Some(days_from_civil(y, m, d))
}

fn parse_timestamp_micros(s: &str) -> Option<i64> {
    let (date_part, time_part) = match s.split_once(' ') {
        Some((d, t)) => (d, Some(t)),
        None => match s.split_once('T') {
            Some((d, t)) => (d, Some(t)),
            None => (s, None),
        },
    };
    let (y, m, d) = split_ymd(date_part)?;
    let days = days_from_civil(y, m, d) as i64;
    let sod_micros = match time_part {
        None => 0i64,
        Some(t) => parse_time_micros(t)?,
    };
    Some(days * 86_400 * 1_000_000 + sod_micros)
}

fn parse_time_micros(s: &str) -> Option<i64> {
    let s = s
        .split('+')
        .next()
        .unwrap_or(s)
        .trim_end_matches('Z')
        .trim();

    let mut parts = s.splitn(3, ':');
    let h: i64 = parts.next()?.parse().ok()?;
    let m: i64 = parts.next()?.parse().ok()?;
    let sec_raw = parts.next()?;

    let (sec_i, frac_us) = match sec_raw.split_once('.') {
        Some((whole, frac)) => {
            let sec_i: i64 = whole.parse().ok()?;
            let frac_truncated: String = frac.chars().take(6).collect();
            let frac_padded = format!("{frac_truncated:0<6}");
            let frac_us: i64 = frac_padded.parse().ok()?;
            (sec_i, frac_us)
        }
        None => (sec_raw.parse().ok()?, 0i64),
    };

    Some(h * 3_600_000_000 + m * 60_000_000 + sec_i * 1_000_000 + frac_us)
}

fn split_ymd(s: &str) -> Option<(i64, u32, u32)> {
    let mut parts = s.splitn(3, '-');
    let y: i64 = parts.next()?.parse().ok()?;
    let m: u32 = parts.next()?.parse().ok()?;
    let d: u32 = parts.next()?.parse().ok()?;
    Some((y, m, d))
}

fn days_from_civil(y: i64, m: u32, d: u32) -> i32 {
    let (y, m) = if m <= 2 { (y - 1, m + 12) } else { (y, m) };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u32;
    let doy = (153 * (m - 3) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    (era * 146_097 + doe as i64 - 719_468) as i32
}

// ── Dataset metadata (schema introspection) ──────────────────────────────────

#[derive(Deserialize, Debug)]
struct DatasetColumn {
    name: String,
    #[serde(rename = "type")]
    col_type: String,
}

#[derive(Deserialize, Debug)]
struct DatasetSchema {
    #[serde(default)]
    columns: Vec<DatasetColumn>,
}

#[derive(Deserialize, Debug)]
struct DatasetInfo {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    schema: Option<DatasetSchema>,
}

/// Fetch the dataset schema from `GET {base_url}/v1/datasets/{dataset_id}`.
///
/// Failures are non-fatal — the caller falls back to an empty [`SchemaInfo`].
async fn fetch_schema(
    client: &reqwest::Client,
    base_url: &str,
    dataset_id: &str,
) -> Result<SchemaInfo, ConnectorError> {
    let url = format!(
        "{}/v1/datasets/{dataset_id}",
        base_url.trim_end_matches('/')
    );
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| ConnectorError::ConnectionError(e.to_string()))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(ConnectorError::ConnectionError(format!(
            "dataset metadata fetch failed: HTTP {status}: {text}"
        )));
    }

    let info: DatasetInfo = resp
        .json()
        .await
        .map_err(|e| ConnectorError::ConnectionError(format!("dataset metadata parse: {e}")))?;

    let table_name = info.name.unwrap_or_else(|| dataset_id.to_string());
    let columns: Vec<SchemaColumnInfo> = info
        .schema
        .map(|s| {
            s.columns
                .into_iter()
                .map(|c| SchemaColumnInfo {
                    name: c.name,
                    data_type: c.col_type,
                    min: None,
                    max: None,
                    sample_values: vec![],
                })
                .collect()
        })
        .unwrap_or_default();

    Ok(SchemaInfo {
        tables: vec![SchemaTableInfo {
            name: table_name,
            columns,
        }],
        join_keys: vec![],
    })
}

// ── Connector ───────────────────────────────────────────────────────────────

/// DOMO connector backed by the REST query API.
pub struct DomoConnector {
    client: reqwest::Client,
    base_url: String,
    dataset_id: String,
    cached_schema: SchemaInfo,
}

impl DomoConnector {
    /// Connect: build an HTTP client with the developer-token header baked in.
    /// Returns on the first HTTP error if the client builder fails — no probe
    /// query is run during construction.
    pub async fn new(
        base_url: String,
        developer_token: String,
        dataset_id: String,
    ) -> Result<Self, ConnectorError> {
        let mut headers = reqwest::header::HeaderMap::new();
        let mut token_value =
            reqwest::header::HeaderValue::from_str(&developer_token).map_err(|e| {
                ConnectorError::ConnectionError(format!("invalid developer-token header: {e}"))
            })?;
        token_value.set_sensitive(true);
        headers.insert("X-DOMO-Developer-Token", token_value);

        let client = reqwest::Client::builder()
            .default_headers(headers)
            .build()
            .map_err(|e| ConnectorError::ConnectionError(format!("http client build: {e}")))?;

        let cached_schema = fetch_schema(&client, &base_url, &dataset_id)
            .await
            .unwrap_or_else(|e| {
                tracing::warn!("domo: schema prefetch failed ({e}); schema browsing unavailable");
                SchemaInfo::default()
            });

        Ok(Self {
            client,
            base_url,
            dataset_id,
            cached_schema,
        })
    }

    async fn http_query(&self, sql: &str) -> Result<ExecuteQueryResponse, ConnectorError> {
        let url = format!(
            "{}/query/v1/execute/{}",
            self.base_url.trim_end_matches('/'),
            self.dataset_id
        );

        let response = self
            .client
            .post(&url)
            .json(&ExecuteQueryRequest { sql })
            .send()
            .await
            .map_err(|e| ConnectorError::query_failed(sql.to_string(), e.to_string()))?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            return Err(ConnectorError::query_failed(
                sql.to_string(),
                format!("HTTP {status}: {text}"),
            ));
        }

        response.json::<ExecuteQueryResponse>().await.map_err(|e| {
            ConnectorError::query_failed(sql.to_string(), format!("JSON parse error: {e}"))
        })
    }
}

// ── DatabaseConnector impl ──────────────────────────────────────────────────

#[async_trait]
impl DatabaseConnector for DomoConnector {
    fn dialect(&self) -> SqlDialect {
        SqlDialect::Other("DOMO")
    }

    #[tracing::instrument(
        target = "agentic_connector::query",
        name = "db.query",
        skip_all,
        err(level = "info", Display),
        fields(
            otel.name = %crate::telemetry::span_name(sql, "domo"),
            otel.kind = "client",
            db.system.name = "domo",
            db.operation.name = %crate::telemetry::operation_name(sql),
            db.query.text = %crate::telemetry::query_text(sql),
            oxy.db.method = "execute_query",
            oxy.db.sample_limit = sample_limit,
        )
    )]
    async fn execute_query(
        &self,
        sql: &str,
        sample_limit: u64,
    ) -> Result<ExecutionResult, ConnectorError> {
        // DOMO's REST API has no server-side aggregation / LIMIT injection —
        // the whole result comes back as JSON. Fetch once, then slice and
        // aggregate client-side.
        let resp = self.http_query(sql).await?;

        let column_names = resp.columns.clone();
        let column_types: Vec<Option<String>> = resp
            .metadata
            .iter()
            .map(|m| Some(m.r#type.clone()))
            .collect();
        let col_count = column_names.len();

        let total_row_count = resp.rows.len() as u64;

        let limit = (sample_limit as usize).min(resp.rows.len());
        let sample_rows: Vec<QueryRow> = resp
            .rows
            .iter()
            .take(limit)
            .map(|row| {
                let cells = (0..col_count)
                    .map(|i| row.get(i).map(json_to_cell).unwrap_or(CellValue::Null))
                    .collect();
                QueryRow(cells)
            })
            .collect();

        // Per-column stats. Computed client-side since DOMO's REST API has
        // no aggregate query sugar.
        let col_stats: Vec<ColumnStats> = (0..col_count)
            .map(|idx| {
                let col = &column_names[idx];
                let mut null_count = 0u64;
                let mut distinct_vals: std::collections::HashSet<String> =
                    std::collections::HashSet::new();
                let mut numeric_sum = 0f64;
                let mut numeric_sqsum = 0f64;
                let mut numeric_count = 0u64;
                let mut min_cell = CellValue::Null;
                let mut max_cell = CellValue::Null;

                for row in &resp.rows {
                    let v = row.get(idx).unwrap_or(&Value::Null);
                    if v.is_null() {
                        null_count += 1;
                        continue;
                    }
                    let repr = v.to_string();
                    distinct_vals.insert(repr.clone());

                    let cell = json_to_cell(v);
                    if matches!(min_cell, CellValue::Null) {
                        min_cell = cell.clone();
                    } else if cell_lt(&cell, &min_cell) {
                        min_cell = cell.clone();
                    }
                    if matches!(max_cell, CellValue::Null) {
                        max_cell = cell.clone();
                    } else if cell_gt(&cell, &max_cell) {
                        max_cell = cell.clone();
                    }

                    if let CellValue::Number(n) = cell {
                        numeric_sum += n;
                        numeric_sqsum += n * n;
                        numeric_count += 1;
                    }
                }

                let (mean, std_dev) = if numeric_count > 0 {
                    let n = numeric_count as f64;
                    let mean = numeric_sum / n;
                    let variance = (numeric_sqsum / n) - mean * mean;
                    (Some(mean), Some(variance.max(0.0).sqrt()))
                } else {
                    (None, None)
                };

                ColumnStats {
                    name: col.clone(),
                    data_type: column_types.get(idx).cloned().flatten(),
                    null_count,
                    distinct_count: Some(distinct_vals.len() as u64),
                    min: Some(min_cell),
                    max: Some(max_cell),
                    mean,
                    std_dev,
                }
            })
            .collect();

        let truncated = (sample_rows.len() as u64) < total_row_count;
        Ok(ExecutionResult {
            result: QueryResult {
                columns: column_names,
                rows: sample_rows,
                total_row_count,
                truncated,
            },
            summary: ResultSummary {
                row_count: total_row_count,
                columns: col_stats,
            },
        })
    }

    #[tracing::instrument(
        target = "agentic_connector::query",
        name = "db.query",
        skip_all,
        err(level = "info", Display),
        fields(
            otel.name = %crate::telemetry::span_name(sql, "domo"),
            otel.kind = "client",
            db.system.name = "domo",
            db.operation.name = %crate::telemetry::operation_name(sql),
            db.query.text = %crate::telemetry::query_text(sql),
            oxy.db.method = "execute_query_full",
        )
    )]
    async fn execute_query_full(&self, sql: &str) -> Result<TypedRowStream, ConnectorError> {
        let resp = self.http_query(sql).await?;

        let columns: Vec<ColumnSpec> = resp
            .columns
            .iter()
            .zip(resp.metadata.iter())
            .map(|(name, meta)| ColumnSpec {
                name: name.clone(),
                data_type: domo_type_to_typed(&meta.r#type),
            })
            .collect();
        let col_count = columns.len();

        let typed_rows: Vec<Result<Vec<TypedValue>, TypedRowError>> = resp
            .rows
            .iter()
            .map(|row| {
                let mut cells = Vec::with_capacity(col_count);
                for (i, col) in columns.iter().enumerate() {
                    let v = row.get(i).unwrap_or(&Value::Null);
                    cells.push(parse_domo_cell(v, col)?);
                }
                Ok(cells)
            })
            .collect();

        Ok(TypedRowStream::from_rows(columns, typed_rows))
    }

    fn introspect_schema(&self) -> Result<SchemaInfo, ConnectorError> {
        Ok(self.cached_schema.clone())
    }
}

// ── CellValue ordering (crude lexical-or-numeric compare for stats) ────────

fn cell_lt(a: &CellValue, b: &CellValue) -> bool {
    match (a, b) {
        (CellValue::Number(x), CellValue::Number(y)) => x < y,
        (CellValue::Text(x), CellValue::Text(y)) => x < y,
        (CellValue::Number(x), CellValue::Text(y)) => x.to_string().as_str() < y.as_str(),
        (CellValue::Text(x), CellValue::Number(y)) => x.as_str() < y.to_string().as_str(),
        _ => false,
    }
}

fn cell_gt(a: &CellValue, b: &CellValue) -> bool {
    cell_lt(b, a)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(data_type: TypedDataType) -> ColumnSpec {
        ColumnSpec {
            name: "c".into(),
            data_type,
        }
    }

    #[test]
    fn type_mapping_basic() {
        assert_eq!(domo_type_to_typed("LONG"), TypedDataType::Int64);
        assert_eq!(domo_type_to_typed("DOUBLE"), TypedDataType::Float64);
        // DOMO doesn't expose precision/scale, so DECIMAL maps to Text.
        assert_eq!(domo_type_to_typed("DECIMAL"), TypedDataType::Text);
        assert_eq!(domo_type_to_typed("STRING"), TypedDataType::Text);
        assert_eq!(domo_type_to_typed("BOOLEAN"), TypedDataType::Bool);
        assert_eq!(domo_type_to_typed("DATE"), TypedDataType::Date);
        assert_eq!(domo_type_to_typed("DATETIME"), TypedDataType::Timestamp);
        assert_eq!(domo_type_to_typed("NOPE"), TypedDataType::Unknown);
    }

    #[test]
    fn parse_cell_handles_nulls_and_scalars() {
        assert_eq!(
            parse_domo_cell(&Value::Null, &col(TypedDataType::Int64)).unwrap(),
            TypedValue::Null
        );
        assert_eq!(
            parse_domo_cell(&serde_json::json!(42), &col(TypedDataType::Int64)).unwrap(),
            TypedValue::Int64(42)
        );
        assert_eq!(
            parse_domo_cell(&Value::String("hello".into()), &col(TypedDataType::Text)).unwrap(),
            TypedValue::Text("hello".into())
        );
    }

    #[test]
    fn parse_cell_bool_accepts_multiple_encodings() {
        for truthy in ["true", "1"] {
            assert_eq!(
                parse_domo_cell(&Value::String(truthy.into()), &col(TypedDataType::Bool)).unwrap(),
                TypedValue::Bool(true)
            );
        }
        assert_eq!(
            parse_domo_cell(&serde_json::json!(true), &col(TypedDataType::Bool)).unwrap(),
            TypedValue::Bool(true)
        );
        assert!(parse_domo_cell(&Value::String("nope".into()), &col(TypedDataType::Bool)).is_err());
    }

    #[test]
    fn parse_cell_date_and_timestamp() {
        assert_eq!(
            parse_domo_cell(
                &Value::String("1970-01-01".into()),
                &col(TypedDataType::Date)
            )
            .unwrap(),
            TypedValue::Date(0)
        );
        assert_eq!(
            parse_domo_cell(
                &Value::String("1970-01-01 00:00:00".into()),
                &col(TypedDataType::Timestamp)
            )
            .unwrap(),
            TypedValue::Timestamp(0)
        );
        assert_eq!(
            parse_domo_cell(
                &Value::String("2026-04-22".into()),
                &col(TypedDataType::Date)
            )
            .unwrap(),
            TypedValue::Date(20_565)
        );
    }

    #[test]
    fn parse_cell_decimal_preserves_string() {
        let v = Value::String("123.4500".into());
        assert_eq!(
            parse_domo_cell(
                &v,
                &col(TypedDataType::Decimal {
                    precision: 10,
                    scale: 4
                })
            )
            .unwrap(),
            TypedValue::Decimal("123.4500".into())
        );
    }
}
