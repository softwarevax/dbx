use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use reqwest::{Certificate, Client as HttpClient, Response};
use serde::{Deserialize, Serialize};
use std::fs;
use std::time::{Duration, Instant};

use super::with_connection_timeout;
use crate::models::connection::ConnectionConfig;
use crate::sql::starts_with_executable_sql_keyword;
use crate::types::{ColumnInfo, DatabaseInfo, QueryMessage, QueryResult, TableInfo};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InfluxdbApiVersion {
    V1,
    V2,
    V3,
}

pub struct InfluxdbClient {
    http: HttpClient,
    base_url: String,
    username: Option<String>,
    password: Option<String>,
    url_params: Option<String>,
    version: InfluxdbApiVersion,
    org: Option<String>,
}

impl InfluxdbClient {
    pub fn new(
        url: &str,
        username: Option<String>,
        password: Option<String>,
        url_params: Option<String>,
        timeout: Duration,
    ) -> Self {
        let http = HttpClient::builder().connect_timeout(timeout).build().unwrap_or_else(|_| HttpClient::new());
        Self {
            http,
            base_url: url.trim_end_matches('/').to_string(),
            username,
            password,
            url_params,
            version: InfluxdbApiVersion::V1,
            org: None,
        }
    }

    pub fn new_with_ca_cert(
        url: &str,
        username: Option<String>,
        password: Option<String>,
        url_params: Option<String>,
        ca_cert_path: Option<&str>,
        timeout: Duration,
    ) -> Result<Self, String> {
        let http = build_http_client(ca_cert_path, timeout)?;
        Ok(Self {
            http,
            base_url: url.trim_end_matches('/').to_string(),
            username,
            password,
            url_params,
            version: InfluxdbApiVersion::V1,
            org: None,
        })
    }

    pub fn new_for_config(url: &str, config: &ConnectionConfig, timeout: Duration) -> Result<Self, String> {
        let version = influxdb_api_version(config.external_config.as_ref());
        let org = influxdb_org(config.external_config.as_ref());
        // InfluxDB 2.x authenticates with a token and scopes requests by org/bucket.
        // InfluxDB 3.x authenticates with a bearer token and scopes requests by database.
        let (username, password) = match version {
            InfluxdbApiVersion::V1 => (
                (!config.username.is_empty()).then_some(config.username.clone()),
                (!config.password.is_empty()).then_some(config.password.clone()),
            ),
            InfluxdbApiVersion::V2 | InfluxdbApiVersion::V3 => {
                (None, (!config.password.is_empty()).then_some(config.password.clone()))
            }
        };
        let http = build_http_client(Some(&config.ca_cert_path), timeout)?;
        Ok(Self {
            http,
            base_url: url.trim_end_matches('/').to_string(),
            username,
            password,
            url_params: config.url_params.clone(),
            version,
            org,
        })
    }
}

fn build_http_client(ca_cert_path: Option<&str>, timeout: Duration) -> Result<HttpClient, String> {
    let mut builder = HttpClient::builder().connect_timeout(timeout);
    if let Some(path) = ca_cert_path.map(str::trim).filter(|path| !path.is_empty()) {
        let path = expand_cert_path(path);
        let cert_bytes =
            fs::read(&path).map_err(|e| format!("Failed to read InfluxDB CA certificate at {path}: {e}"))?;
        let cert = Certificate::from_pem(&cert_bytes)
            .or_else(|_| Certificate::from_der(&cert_bytes))
            .map_err(|e| format!("Failed to parse InfluxDB CA certificate at {path}: {e}"))?;
        builder = builder.add_root_certificate(cert);
    }
    builder.build().map_err(|e| format!("Failed to configure InfluxDB HTTP client: {e}"))
}

fn influxdb_api_version(external_config: Option<&serde_json::Value>) -> InfluxdbApiVersion {
    match external_config.and_then(|value| value.get("version")).and_then(serde_json::Value::as_str).map(str::trim) {
        Some("2" | "v2" | "V2") => InfluxdbApiVersion::V2,
        Some("3" | "v3" | "V3") => InfluxdbApiVersion::V3,
        _ => InfluxdbApiVersion::V1,
    }
}

fn influxdb_org(external_config: Option<&serde_json::Value>) -> Option<String> {
    external_config
        .and_then(|value| value.get("org").or_else(|| value.get("organization")))
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn expand_cert_path(path: &str) -> String {
    let home = || std::env::var(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).ok();
    if path == "~" || path.starts_with("~/") || path.starts_with("~\\") {
        if let Some(home) = home() {
            return format!("{}{}", home, &path[1..]);
        }
    }
    if let Some(rest) = path.strip_prefix("$HOME") {
        if let Some(home) = home() {
            return format!("{home}{rest}");
        }
    }
    if let Some(rest) = path.strip_prefix("${HOME}") {
        if let Some(home) = home() {
            return format!("{home}{rest}");
        }
    }
    if let Some(rest) = path.strip_prefix("%USERPROFILE%") {
        if let Ok(home) = std::env::var("USERPROFILE") {
            return format!("{home}{rest}");
        }
    }
    path.to_string()
}

impl Clone for InfluxdbClient {
    fn clone(&self) -> Self {
        Self {
            http: self.http.clone(),
            base_url: self.base_url.clone(),
            username: self.username.clone(),
            password: self.password.clone(),
            url_params: self.url_params.clone(),
            version: self.version,
            org: self.org.clone(),
        }
    }
}

#[derive(Deserialize, Default)]
struct InfluxErrorResult {
    #[serde(default)]
    #[serde(rename = "error", alias = "err")]
    error: Option<String>,
    #[serde(default)]
    message: Option<String>,
}

#[derive(Deserialize)]
struct InfluxJsonResult {
    results: Vec<InfluxQueryResult>,
}

#[derive(Deserialize)]
#[allow(dead_code)]
struct InfluxQueryResult {
    /// The `statement_id` field may not be included in older versions (such as 1.1)
    #[serde(default)]
    #[allow(dead_code)]
    statement_id: usize,
    #[serde(default)]
    #[allow(dead_code)]
    series: Vec<InfluxSeries>,
}

#[derive(Deserialize)]
#[allow(dead_code)]
struct InfluxSeries {
    #[serde(default)]
    #[allow(dead_code)]
    name: String,
    columns: Vec<String>,
    values: Vec<Vec<serde_json::Value>>,
}

#[derive(Deserialize)]
struct InfluxBucketsResult {
    #[serde(default)]
    buckets: Vec<InfluxBucket>,
}

#[derive(Deserialize)]
struct InfluxBucket {
    name: String,
}

#[derive(Serialize)]
struct FluxQueryRequest<'a> {
    query: &'a str,
    #[serde(rename = "type")]
    query_type: &'static str,
}

fn build_query_url(client: &InfluxdbClient, database: Option<&str>, sql: &str) -> String {
    let mut params: Vec<String> = vec![];
    if let Some(url_params) = &client.url_params {
        params.push(url_params.to_string())
    }
    if let Some(db) = database {
        params.push(format!("db={db}"));
    }
    let encoded_sql = utf8_percent_encode(sql, NON_ALPHANUMERIC);
    params.push(format!("q={encoded_sql}"));
    format!("{}/query?{}", client.base_url, params.join("&").as_str())
}

fn encode_url_param(value: &str) -> String {
    utf8_percent_encode(value, NON_ALPHANUMERIC).to_string()
}

fn build_v2_buckets_url(client: &InfluxdbClient, offset: usize) -> Result<String, String> {
    let org = client.org.as_deref().ok_or_else(|| "InfluxDB 2.x organization is required".to_string())?;
    Ok(format!("{}/api/v2/buckets?org={}&limit=100&offset={offset}", client.base_url, encode_url_param(org)))
}

fn build_v2_query_url(client: &InfluxdbClient) -> Result<String, String> {
    let org = client.org.as_deref().ok_or_else(|| "InfluxDB 2.x organization is required".to_string())?;
    Ok(format!("{}/api/v2/query?org={}", client.base_url, encode_url_param(org)))
}

/// InfluxDB 3 exposes SQL through `/api/v3/query_sql` and InfluxQL through
/// `/api/v3/query_influxql`. Both accept the same JSON payload.
fn build_v3_query_url(client: &InfluxdbClient, language: V3QueryLanguage) -> String {
    format!("{}/api/v3/{}", client.base_url, language.endpoint())
}

fn build_v3_databases_url(client: &InfluxdbClient) -> String {
    format!("{}/api/v3/configure/database?format=json", client.base_url)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum V3QueryLanguage {
    Sql,
    InfluxQl,
}

impl V3QueryLanguage {
    fn endpoint(self) -> &'static str {
        match self {
            V3QueryLanguage::Sql => "query_sql",
            V3QueryLanguage::InfluxQl => "query_influxql",
        }
    }
}

#[derive(Serialize)]
struct V3QueryRequest<'a> {
    db: &'a str,
    q: &'a str,
    format: &'static str,
}

fn build_request(client: &InfluxdbClient, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    if matches!(client.version, InfluxdbApiVersion::V2 | InfluxdbApiVersion::V3) {
        let Some(token) = client.password.as_deref().map(str::trim).filter(|token| !token.is_empty()) else {
            // InfluxDB 3 Core can be started with `--without-auth`, so a missing token is not fatal.
            return req;
        };
        // v2 expects the legacy `Token` scheme; v3 standardises on `Bearer`.
        let scheme = if client.version == InfluxdbApiVersion::V3 { "Bearer" } else { "Token" };
        return req.header("Authorization", format!("{scheme} {token}"));
    }
    match (&client.username, &client.password) {
        (Some(u), Some(p)) if !u.is_empty() => req.basic_auth(u, Some(p)),
        (Some(u), None) if !u.is_empty() => req.basic_auth(u, None::<&str>),
        _ => req,
    }
}

fn require_v2_token(client: &InfluxdbClient) -> Result<(), String> {
    if client.password.as_deref().map(str::trim).filter(|token| !token.is_empty()).is_none() {
        return Err("InfluxDB 2.x token is required".to_string());
    }
    Ok(())
}

async fn influx_query(client: &InfluxdbClient, sql: &str, database: Option<&str>) -> Result<InfluxJsonResult, String> {
    let url = build_query_url(client, database, sql);
    log::info!("[influxdb] query url={url} username={:?} password={}", client.username, client.password.is_some());

    let req = if starts_with_executable_sql_keyword(sql, &["SELECT", "SHOW"]) {
        build_request(client, client.http.get(&url))
    } else {
        build_request(client, client.http.post(&url))
    };

    let resp = req.send().await.map_err(|e| format!("InfluxDB request failed: {e}"))?;
    log::info!("[influxdb] response status={}", resp.status());
    if !resp.status().is_success() {
        return handle_influx_error(resp).await;
    }
    let response_text = resp.text().await.unwrap_or_default();
    serde_json::from_str::<InfluxJsonResult>(&response_text)
        .map_err(|e| format!("InfluxDB parse error: {e}; response: {response_text}"))
}

async fn influx_v2_buckets(client: &InfluxdbClient, timeout: Duration) -> Result<Vec<InfluxBucket>, String> {
    require_v2_token(client)?;
    let mut buckets = Vec::new();
    let mut offset = 0;
    loop {
        let url = build_v2_buckets_url(client, offset)?;
        let req = build_request(client, client.http.get(&url));
        let resp = with_connection_timeout("InfluxDB", timeout, async {
            req.send().await.map_err(|e| format!("InfluxDB connection failed: {e}"))
        })
        .await?;
        if !resp.status().is_success() {
            return handle_influx_error(resp).await;
        }
        let body = resp.text().await.unwrap_or_default();
        let mut page = serde_json::from_str::<InfluxBucketsResult>(&body)
            .map(|result| result.buckets)
            .map_err(|e| format!("InfluxDB parse error: {e}; response: {body}"))?;
        let page_len = page.len();
        buckets.append(&mut page);
        if page_len < 100 {
            break;
        }
        offset += page_len;
    }
    Ok(buckets)
}

async fn influx_flux_query(client: &InfluxdbClient, sql: &str) -> Result<QueryResult, String> {
    require_v2_token(client)?;
    let start = Instant::now();
    let url = build_v2_query_url(client)?;
    let body = FluxQueryRequest { query: sql, query_type: "flux" };
    let resp = build_request(client, client.http.post(&url).json(&body))
        .send()
        .await
        .map_err(|e| format!("InfluxDB request failed: {e}"))?;
    if !resp.status().is_success() {
        return handle_influx_error(resp).await;
    }
    let response_text = resp.text().await.unwrap_or_default();
    parse_flux_csv(&response_text, start)
}

async fn influx_v3_query(
    client: &InfluxdbClient,
    database: &str,
    sql: &str,
    language: V3QueryLanguage,
) -> Result<QueryResult, String> {
    let start = Instant::now();
    let url = build_v3_query_url(client, language);
    let body = V3QueryRequest { db: database, q: sql, format: "json" };
    log::info!("[influxdb] v3 query url={url} db={database} language={language:?}");
    let resp = build_request(client, client.http.post(&url).json(&body))
        .send()
        .await
        .map_err(|e| format!("InfluxDB request failed: {e}"))?;
    if !resp.status().is_success() {
        return handle_influx_error(resp).await;
    }
    let response_text = resp.text().await.unwrap_or_default();
    parse_v3_json_rows(&response_text, start)
}

/// InfluxDB 3 has no SQL `DROP TABLE` or InfluxQL `DROP MEASUREMENT`. Dropping a table is a
/// management operation performed through `DELETE /api/v3/configure/table`. `hard_delete_at=now`
/// removes the table immediately instead of leaving a soft-deleted (renamed) placeholder behind.
async fn delete_table_v3(client: &InfluxdbClient, database: &str, table: &str) -> Result<QueryResult, String> {
    let db = encode_url_param(database);
    let table = encode_url_param(table);
    let url = format!(
        "{}/api/v3/configure/table?db={}&table={}&hard_delete_at=now",
        client.base_url, db, table
    );
    log::info!("[influxdb] v3 delete table url={url} database={database} table={table}");
    let start = Instant::now();
    let resp = build_request(client, client.http.delete(&url))
        .send()
        .await
        .map_err(|e| format!("InfluxDB 3 delete table request failed: {e}"))?;
    if !resp.status().is_success() {
        return handle_influx_error(resp).await;
    }
    Ok(QueryResult {
        columns: vec![],
        column_types: vec![],
        column_sortables: vec![],
        spatial_columns: vec![],
        spatial_values: vec![],
        rows: vec![],
        affected_rows: 1,
        execution_time_ms: start.elapsed().as_millis(),
        truncated: false,
        session_id: None,
        has_more: false,
        elasticsearch_raw_body: None,
        messages: vec![QueryMessage {
            severity: "INFO".to_string(),
            message: format!("Dropped table {table} from database {database}"),
            code: None,
            detail: None,
            hint: None,
        }],
    })
}

async fn influx_v3_databases(client: &InfluxdbClient, timeout: Duration) -> Result<Vec<String>, String> {
    let url = build_v3_databases_url(client);
    let req = build_request(client, client.http.get(&url));
    let resp = with_connection_timeout("InfluxDB", timeout, async {
        req.send().await.map_err(|e| format!("InfluxDB connection failed: {e}"))
    })
    .await?;
    if !resp.status().is_success() {
        return handle_influx_error(resp).await;
    }
    let body = resp.text().await.unwrap_or_default();
    let value = serde_json::from_str::<serde_json::Value>(&body)
        .map_err(|e| format!("InfluxDB parse error: {e}; response: {body}"))?;
    Ok(extract_v3_database_names(&value))
}

/// InfluxDB 3 has shipped a few shapes for the database listing across releases:
/// `{"databases": ["a"]}`, `[{"iox::database": "a"}]` and `[{"db_name": "a"}]`.
/// Accept all of them so the driver keeps working across Core/Enterprise builds.
fn extract_v3_database_names(value: &serde_json::Value) -> Vec<String> {
    const NAME_KEYS: [&str; 4] = ["iox::database", "db_name", "database", "name"];
    let entries = match value {
        serde_json::Value::Array(items) => items.as_slice(),
        serde_json::Value::Object(map) => match map.get("databases") {
            Some(serde_json::Value::Array(items)) => items.as_slice(),
            _ => &[],
        },
        _ => &[],
    };
    entries
        .iter()
        .filter_map(|entry| match entry {
            serde_json::Value::String(name) => Some(name.clone()),
            serde_json::Value::Object(map) => NAME_KEYS
                .iter()
                .find_map(|key| map.get(*key).and_then(serde_json::Value::as_str))
                .map(str::to_string),
            _ => None,
        })
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
        .collect()
}

/// `format=json` responses are a JSON array of row objects. `serde_json` is built with
/// `preserve_order`, so the key order of each object still reflects the projection order.
fn parse_v3_json_rows(text: &str, start: Instant) -> Result<QueryResult, String> {
    let trimmed = text.trim();
    let parsed = if trimmed.is_empty() {
        serde_json::Value::Array(Vec::new())
    } else {
        serde_json::from_str::<serde_json::Value>(trimmed)
            .map_err(|e| format!("InfluxDB parse error: {e}; response: {text}"))?
    };
    let records = match parsed {
        serde_json::Value::Array(items) => items,
        object @ serde_json::Value::Object(_) => vec![object],
        serde_json::Value::Null => Vec::new(),
        other => return Err(format!("InfluxDB returned an unexpected payload: {other}")),
    };

    let mut columns: Vec<String> = Vec::new();
    for record in &records {
        let Some(map) = record.as_object() else { continue };
        for key in map.keys() {
            if !columns.iter().any(|existing| existing == key) {
                columns.push(key.clone());
            }
        }
    }

    let rows = records
        .iter()
        .map(|record| {
            let map = record.as_object();
            columns
                .iter()
                .map(|column| {
                    map.and_then(|map| map.get(column)).cloned().unwrap_or(serde_json::Value::Null)
                })
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();

    Ok(QueryResult {
        column_sortables: columns.iter().map(|_| false).collect(),
        column_types: vec![],
        spatial_columns: vec![],
        spatial_values: vec![],
        columns,
        affected_rows: rows.len() as u64,
        rows,
        execution_time_ms: start.elapsed().as_millis(),
        truncated: false,
        session_id: None,
        has_more: false,
        elasticsearch_raw_body: None,
        messages: Vec::new(),
    })
}

pub async fn test_connection(client: &InfluxdbClient, timeout: Duration) -> Result<(), String> {
    if client.version == InfluxdbApiVersion::V3 {
        influx_v3_databases(client, timeout).await?;
        return Ok(());
    }
    if client.version == InfluxdbApiVersion::V2 {
        influx_v2_buckets(client, timeout).await?;
        return Ok(());
    }
    let url = build_query_url(client, None, "SHOW DATABASES");
    let req = build_request(client, client.http.get(&url));
    let resp = with_connection_timeout("InfluxDB", timeout, async {
        req.send().await.map_err(|e| format!("InfluxDB connection failed: {e}"))
    })
    .await?;
    if !resp.status().is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("InfluxDB error: {body}"));
    }
    Ok(())
}

pub async fn list_databases(client: &InfluxdbClient) -> Result<Vec<DatabaseInfo>, String> {
    if client.version == InfluxdbApiVersion::V3 {
        return Ok(influx_v3_databases(client, Duration::from_secs(30))
            .await?
            .into_iter()
            .map(|name| DatabaseInfo { name })
            .collect());
    }
    if client.version == InfluxdbApiVersion::V2 {
        return Ok(influx_v2_buckets(client, Duration::from_secs(30))
            .await?
            .into_iter()
            .map(|bucket| DatabaseInfo { name: bucket.name })
            .collect());
    }
    let result = influx_query(client, "SHOW DATABASES", None).await?;
    Ok(result
        .results
        .iter()
        .flat_map(|r| &r.series)
        .flat_map(|s| &s.values)
        .map(|row| DatabaseInfo { name: row[0].as_str().unwrap_or("").to_string() })
        .collect())
}

pub async fn list_tables(client: &InfluxdbClient, database: &str) -> Result<Vec<TableInfo>, String> {
    if client.version == InfluxdbApiVersion::V3 {
        // `iox` is the schema holding user-created tables; `system` and `information_schema`
        // are server internals and should stay out of the object browser.
        let query = "SELECT table_name FROM information_schema.tables \
                     WHERE table_schema = 'iox' ORDER BY table_name";
        let result = influx_v3_query(client, database, query, V3QueryLanguage::Sql).await?;
        return Ok(v3_column_values(&result, "table_name")
            .into_iter()
            .filter(|name| !is_v3_soft_deleted_table(name))
            .map(|name| TableInfo {
                name,
                table_type: "TABLE".to_string(),
                comment: None,
                parent_schema: None,
                parent_name: None,
            })
            .collect());
    }
    if client.version == InfluxdbApiVersion::V2 {
        let query = format!(
            "import \"influxdata/influxdb/schema\"\nschema.measurements(bucket: \"{}\", start: 0)",
            escape_flux_string(database)
        );
        let result = influx_flux_query(client, &query).await?;
        return Ok(flux_column_values(&result, "_value")
            .into_iter()
            .map(|name| TableInfo {
                name,
                table_type: "TABLE".to_string(),
                comment: None,
                parent_schema: None,
                parent_name: None,
            })
            .collect());
    }
    let result = influx_query(client, "SHOW MEASUREMENTS", Some(database)).await?;
    let empty = vec![];
    let series = result.results.first().map(|r| &r.series).unwrap_or(&empty);
    if series.is_empty() {
        return Ok(vec![]);
    }
    let first_series = &series[0];
    Ok(first_series
        .values
        .iter()
        .map(|row| TableInfo {
            name: row[0].as_str().unwrap_or("").to_string(),
            table_type: "TABLE".to_string(),
            comment: None,
            parent_schema: None,
            parent_name: None,
        })
        .collect())
}

pub async fn get_columns(client: &InfluxdbClient, database: &str, table: &str) -> Result<Vec<ColumnInfo>, String> {
    if client.version == InfluxdbApiVersion::V3 {
        return get_columns_v3(client, database, table).await;
    }
    if client.version == InfluxdbApiVersion::V2 {
        return get_columns_v2(client, database, table).await;
    }
    let empty = vec![];

    let tag_sql = format!("SHOW TAG KEYS FROM \"{}\"", table);
    let tag_result = influx_query(client, &tag_sql, Some(database)).await?;
    let tag_series = tag_result.results.first().map(|r| &r.series).unwrap_or(&empty);

    let field_sql = format!("SHOW FIELD KEYS FROM \"{}\"", table);
    let field_result = influx_query(client, &field_sql, Some(database)).await?;
    let field_series = field_result.results.first().map(|r| &r.series).unwrap_or(&empty);

    let time_col = ColumnInfo {
        name: "time".to_string(),
        data_type: "timestamp".to_string(),
        is_nullable: false,
        column_default: None,
        is_primary_key: true,
        extra: None,
        comment: None,
        numeric_precision: None,
        numeric_scale: None,
        character_maximum_length: None,
        enum_values: None,
        ..Default::default()
    };

    let cols: Vec<ColumnInfo> = std::iter::once(time_col)
        .chain(tag_series.first().into_iter().flat_map(|s| s.values.iter()).map(|row| ColumnInfo {
            name: row[0].as_str().unwrap_or("").to_string(),
            data_type: "string".to_string(),
            is_nullable: true,
            column_default: None,
            is_primary_key: true,
            extra: None,
            comment: None,
            numeric_precision: None,
            numeric_scale: None,
            character_maximum_length: None,
            enum_values: None,
            ..Default::default()
        }))
        .chain(field_series.first().into_iter().flat_map(|s| s.values.iter()).map(|row| {
            let data_type = row.get(1).and_then(|v| v.as_str()).unwrap_or("unknown").to_string();
            ColumnInfo {
                name: row[0].as_str().unwrap_or("").to_string(),
                data_type,
                is_nullable: true,
                column_default: None,
                is_primary_key: false,
                extra: None,
                comment: None,
                numeric_precision: None,
                numeric_scale: None,
                character_maximum_length: None,
                enum_values: None,
                ..Default::default()
            }
        }))
        .collect();

    Ok(cols)
}

pub async fn execute_query(client: &InfluxdbClient, database: &str, sql: &str) -> Result<QueryResult, String> {
    if client.version == InfluxdbApiVersion::V3 {
        // v3 has no DROP statement; route `DROP MEASUREMENT`/`DROP TABLE` to the management API.
        if let Some(table) = extract_drop_table_name(sql) {
            return delete_table_v3(client, database, &table).await;
        }
        let language =
            if looks_like_influxql_query(sql) { V3QueryLanguage::InfluxQl } else { V3QueryLanguage::Sql };
        return influx_v3_query(client, database, sql, language).await;
    }
    if client.version == InfluxdbApiVersion::V2 && looks_like_flux_query(sql) {
        return influx_flux_query(client, sql).await;
    }
    let start = Instant::now();
    let json = influx_query(client, sql, Some(database)).await?;
    let series = json.results.iter().flat_map(|r| &r.series).next();
    match series {
        Some(s) => Ok(QueryResult {
            columns: s.columns.clone(),
            column_types: vec![],
            column_sortables: s.columns.iter().map(|_| false).collect(),
            spatial_columns: vec![],
            spatial_values: vec![],
            rows: s.values.clone(),
            affected_rows: s.values.len() as u64,
            execution_time_ms: start.elapsed().as_millis(),
            truncated: false,
            session_id: None,
            has_more: false,
            elasticsearch_raw_body: None,
            messages: Vec::new(),
        }),
        None => Ok(QueryResult {
            columns: vec![],
            column_types: vec![],
            column_sortables: vec![],
            spatial_columns: vec![],
            spatial_values: vec![],
            rows: vec![],
            affected_rows: 0,
            execution_time_ms: start.elapsed().as_millis(),
            truncated: false,
            session_id: None,
            has_more: false,
            elasticsearch_raw_body: None,
            messages: Vec::new(),
        }),
    }
}

async fn get_columns_v2(client: &InfluxdbClient, bucket: &str, measurement: &str) -> Result<Vec<ColumnInfo>, String> {
    let tag_query = format!(
        "import \"influxdata/influxdb/schema\"\nschema.measurementTagKeys(bucket: \"{}\", measurement: \"{}\", start: 0)",
        escape_flux_string(bucket),
        escape_flux_string(measurement)
    );
    let field_query = format!(
        "import \"influxdata/influxdb/schema\"\nschema.measurementFieldKeys(bucket: \"{}\", measurement: \"{}\", start: 0)",
        escape_flux_string(bucket),
        escape_flux_string(measurement)
    );
    let tag_result = influx_flux_query(client, &tag_query).await?;
    let field_result = influx_flux_query(client, &field_query).await?;
    let time_col = ColumnInfo {
        name: "time".to_string(),
        data_type: "timestamp".to_string(),
        is_nullable: false,
        column_default: None,
        is_primary_key: true,
        extra: None,
        comment: None,
        numeric_precision: None,
        numeric_scale: None,
        character_maximum_length: None,
        enum_values: None,
        ..Default::default()
    };
    let tag_cols = flux_column_values(&tag_result, "_value")
        .into_iter()
        .filter(|name| !is_influx_system_column(name))
        .map(|name| ColumnInfo {
            name,
            data_type: "string".to_string(),
            is_nullable: true,
            column_default: None,
            is_primary_key: true,
            extra: None,
            comment: None,
            numeric_precision: None,
            numeric_scale: None,
            character_maximum_length: None,
            enum_values: None,
            ..Default::default()
        });
    let field_cols = flux_column_values(&field_result, "_value").into_iter().map(|name| ColumnInfo {
        name,
        data_type: "field".to_string(),
        is_nullable: true,
        column_default: None,
        is_primary_key: false,
        extra: None,
        comment: None,
        numeric_precision: None,
        numeric_scale: None,
        character_maximum_length: None,
        enum_values: None,
        ..Default::default()
    });
    Ok(std::iter::once(time_col).chain(tag_cols).chain(field_cols).collect())
}

async fn get_columns_v3(client: &InfluxdbClient, database: &str, table: &str) -> Result<Vec<ColumnInfo>, String> {
    let query = format!(
        "SELECT column_name, data_type, is_nullable FROM information_schema.columns \
         WHERE table_schema = 'iox' AND table_name = '{}' ORDER BY ordinal_position",
        escape_sql_string(table)
    );
    let result = influx_v3_query(client, database, &query, V3QueryLanguage::Sql).await?;
    let name_idx = result.columns.iter().position(|name| name == "column_name");
    let type_idx = result.columns.iter().position(|name| name == "data_type");
    let nullable_idx = result.columns.iter().position(|name| name == "is_nullable");
    let Some(name_idx) = name_idx else { return Ok(Vec::new()) };

    Ok(result
        .rows
        .iter()
        .filter_map(|row| {
            let name = row.get(name_idx).and_then(serde_json::Value::as_str)?.trim().to_string();
            if name.is_empty() {
                return None;
            }
            let data_type = type_idx
                .and_then(|idx| row.get(idx))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown")
                .to_string();
            let is_nullable = nullable_idx
                .and_then(|idx| row.get(idx))
                .and_then(serde_json::Value::as_str)
                .map(|value| !value.eq_ignore_ascii_case("no"))
                .unwrap_or(true);
            // In IOx the series key is made up of the timestamp plus the dictionary-encoded tags.
            let is_primary_key = is_v3_time_column(&name, &data_type) || is_v3_tag_column(&data_type);
            Some(ColumnInfo {
                name,
                data_type: normalize_v3_data_type(&data_type),
                is_nullable,
                column_default: None,
                is_primary_key,
                extra: None,
                comment: None,
                numeric_precision: None,
                numeric_scale: None,
                character_maximum_length: None,
                enum_values: None,
                ..Default::default()
            })
        })
        .collect())
}

fn is_v3_time_column(name: &str, data_type: &str) -> bool {
    name == "time" || data_type.starts_with("Timestamp")
}

fn is_v3_tag_column(data_type: &str) -> bool {
    data_type.starts_with("Dictionary")
}

/// Arrow type names are accurate but noisy in the UI, so map the common ones to
/// the friendlier labels the v1/v2 code paths already produce.
fn normalize_v3_data_type(data_type: &str) -> String {
    if data_type.starts_with("Timestamp") {
        return "timestamp".to_string();
    }
    if data_type.starts_with("Dictionary") {
        return "string".to_string();
    }
    match data_type {
        "Utf8" | "LargeUtf8" => "string".to_string(),
        "Boolean" => "boolean".to_string(),
        "Float64" | "Float32" => "float".to_string(),
        "Int64" | "Int32" => "integer".to_string(),
        "UInt64" | "UInt32" => "unsigned".to_string(),
        other => other.to_string(),
    }
}

fn v3_column_values(result: &QueryResult, column: &str) -> Vec<String> {
    let Some(index) = result.columns.iter().position(|name| name == column) else {
        return Vec::new();
    };
    result
        .rows
        .iter()
        .filter_map(|row| row.get(index).and_then(serde_json::Value::as_str))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect()
}

fn escape_sql_string(value: &str) -> String {
    value.replace('\'', "''")
}

/// InfluxDB 3 defaults to SQL. Only the InfluxQL-specific `SHOW` statements need to be
/// routed to the InfluxQL endpoint — `SHOW TABLES` is valid DataFusion SQL and stays on
/// the SQL endpoint.
fn looks_like_influxql_query(sql: &str) -> bool {
    let trimmed = sql.trim_start().to_ascii_uppercase();
    const INFLUXQL_PREFIXES: [&str; 7] = [
        "SHOW MEASUREMENTS",
        "SHOW TAG KEYS",
        "SHOW TAG VALUES",
        "SHOW FIELD KEYS",
        "SHOW SERIES",
        "SHOW DATABASES",
        "SHOW RETENTION POLICIES",
    ];
    INFLUXQL_PREFIXES.iter().any(|prefix| trimmed.starts_with(prefix))
}

fn escape_flux_string(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn looks_like_flux_query(sql: &str) -> bool {
    let trimmed = sql.trim_start().to_ascii_lowercase();
    trimmed.starts_with("from(")
        || trimmed.starts_with("import ")
        || trimmed.starts_with("option ")
        || trimmed.contains("|>")
}

fn is_influx_system_column(name: &str) -> bool {
    matches!(name, "_start" | "_stop" | "_time" | "_value" | "_field" | "_measurement")
}

/// Extracts the table name from a `DROP MEASUREMENT`/`DROP TABLE` statement so it can be
/// routed to the v3 management API. Returns `None` for anything else.
///
/// Case-insensitive prefix matching is used for the keywords, but the original-cased table
/// name is preserved — InfluxDB identifiers are case-sensitive, so uppercasing would target
/// the wrong table.
fn extract_drop_table_name(sql: &str) -> Option<String> {
    let trimmed = sql.trim().trim_end_matches(';').trim();
    let upper = trimmed.to_ascii_uppercase();
    let rest = upper
        .strip_prefix("DROP TABLE")
        .or_else(|| upper.strip_prefix("DROP MEASUREMENT"))?;
    // Map the matched suffix back onto the original-cased string by byte offset.
    let offset = upper.len() - rest.len();
    let after_keyword = &trimmed[offset..];
    let after_keyword = after_keyword.trim_start();
    let after_keyword = after_keyword.strip_prefix("IF EXISTS").map(str::trim).unwrap_or(after_keyword);
    let name = after_keyword.trim();
    if name.is_empty() {
        return None;
    }
    // Measurement names are usually unquoted, but strip surrounding quotes if present.
    let name = name.strip_prefix('"').and_then(|n| n.strip_suffix('"')).unwrap_or(name);
    Some(name.to_string())
}

/// InfluxDB 3 soft-deletes a table by renaming it to `<table>-<deleted_at_timestamp>`,
/// which lingers in `information_schema.tables` until hard deletion. Flag those so they can
/// be hidden from the object browser as a defense-in-depth measure.
fn is_v3_soft_deleted_table(name: &str) -> bool {
    let Some((_, suffix)) = name.rsplit_once('-') else {
        return false;
    };
    // The suffix is an ISO-8601-ish timestamp, e.g. `2025-12-31T23:59:59Z` (len >= 19).
    suffix.len() >= 19
        && suffix.starts_with(|c: char| c.is_ascii_digit())
        && suffix.contains(|c: char| c == 'T' || c == 'Z' || c == '-')
}

fn flux_column_values(result: &QueryResult, column: &str) -> Vec<String> {
    let Some(index) = result.columns.iter().position(|name| name == column) else {
        return Vec::new();
    };
    result
        .rows
        .iter()
        .filter_map(|row| row.get(index).and_then(serde_json::Value::as_str))
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string)
        .collect()
}

fn parse_flux_csv(text: &str, start: Instant) -> Result<QueryResult, String> {
    let mut headers: Option<Vec<String>> = None;
    let mut rows = Vec::new();
    let mut reader = csv::ReaderBuilder::new().has_headers(false).from_reader(text.as_bytes());
    for record in reader.records() {
        let record = record.map_err(|e| format!("InfluxDB Flux CSV parse error: {e}"))?;
        if record.is_empty() || record.iter().all(|value| value.trim().is_empty()) {
            continue;
        }
        if record.get(0).is_some_and(|value| value.starts_with('#')) {
            continue;
        }
        if headers.is_none() {
            let mut parsed = record.iter().map(str::to_string).collect::<Vec<_>>();
            if parsed.first().is_some_and(|value| value.is_empty()) {
                parsed.remove(0);
            }
            headers = Some(parsed);
            continue;
        }
        let mut values =
            record
                .iter()
                .map(|value| {
                    if value.is_empty() {
                        serde_json::Value::Null
                    } else {
                        serde_json::Value::String(value.to_string())
                    }
                })
                .collect::<Vec<_>>();
        if values.len() == headers.as_ref().map(Vec::len).unwrap_or_default() + 1
            && record.get(0).is_some_and(|value| value.is_empty())
        {
            values.remove(0);
        }
        rows.push(values);
    }
    let columns = headers.unwrap_or_default();
    Ok(QueryResult {
        column_sortables: columns.iter().map(|_| false).collect(),
        spatial_columns: vec![],
        spatial_values: vec![],
        columns,
        column_types: vec![],
        affected_rows: rows.len() as u64,
        rows,
        execution_time_ms: start.elapsed().as_millis(),
        truncated: false,
        session_id: None,
        has_more: false,
        elasticsearch_raw_body: None,
        messages: Vec::new(),
    })
}

async fn handle_influx_error<T>(resp: Response) -> Result<T, String> {
    let status = resp.status();

    let status_message = match status {
        reqwest::StatusCode::UNAUTHORIZED => Some("Unauthorized access."),
        reqwest::StatusCode::FORBIDDEN => Some("Access denied."),
        reqwest::StatusCode::NOT_FOUND => Some("Database not found."),
        reqwest::StatusCode::UNPROCESSABLE_ENTITY => Some("Unprocessable entity."),
        _ => None,
    };

    let error_msg = if let Some(msg) = status_message {
        msg.to_string()
    } else {
        // 尝试从响应体中解析错误信息
        let error_body = resp.text().await.unwrap_or_default();
        extract_error_message(&error_body).unwrap_or_else(|| {
            if error_body.trim().is_empty() {
                "Unknown error.".to_string()
            } else {
                error_body
            }
        })
    };

    log::warn!("[influxdb] error response: status = {}, message = {}", status, error_msg);
    Err(format!("InfluxDB error: status = {}, message = {}", status.as_str(), error_msg))
}

/// 从 InfluxDB 错误响应中提取错误消息
fn extract_error_message(body: &str) -> Option<String> {
    if body.trim().is_empty() {
        return None;
    }

    serde_json::from_str::<InfluxErrorResult>(body).ok().and_then(|error_json| {
        // 优先使用 error 字段，其次使用 message 字段
        error_json
            .error
            .filter(|msg| !msg.trim().is_empty())
            .or_else(|| error_json.message.filter(|msg| !msg.trim().is_empty()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn v1_client() -> InfluxdbClient {
        InfluxdbClient {
            http: reqwest::Client::new(),
            base_url: "http://localhost:8086".to_string(),
            username: None,
            password: None,
            url_params: None,
            version: InfluxdbApiVersion::V1,
            org: None,
        }
    }

    #[test]
    fn query_url() {
        let client = v1_client();
        let url = build_query_url(&client, Some("sample"), "SHOW DATABASES");

        assert_eq!(url, "http://localhost:8086/query?db=sample&q=SHOW%20DATABASES");
    }

    #[test]
    fn v2_config_uses_token_auth_and_org_urls() {
        let config: ConnectionConfig = serde_json::from_value(json!({
            "id": "influx-v2",
            "name": "InfluxDB 2",
            "db_type": "influxdb",
            "host": "127.0.0.1",
            "port": 8086,
            "username": "ignored-user",
            "password": "token-value",
            "database": "metrics",
            "external_config": {
                "version": "2",
                "org": "DBX Org"
            }
        }))
        .unwrap();

        let client = InfluxdbClient::new_for_config("http://localhost:8086/", &config, Duration::from_secs(1)).unwrap();

        assert_eq!(client.version, InfluxdbApiVersion::V2);
        assert_eq!(client.username, None);
        assert_eq!(client.password.as_deref(), Some("token-value"));
        assert_eq!(client.org.as_deref(), Some("DBX Org"));
        assert_eq!(
            build_v2_buckets_url(&client, 0).unwrap(),
            "http://localhost:8086/api/v2/buckets?org=DBX%20Org&limit=100&offset=0"
        );
        assert_eq!(build_v2_query_url(&client).unwrap(), "http://localhost:8086/api/v2/query?org=DBX%20Org");
    }

    #[test]
    fn v2_config_accepts_organization_alias() {
        let config: ConnectionConfig = serde_json::from_value(json!({
            "id": "influx-v2",
            "name": "InfluxDB 2",
            "db_type": "influxdb",
            "host": "127.0.0.1",
            "port": 8086,
            "username": "",
            "password": "token-value",
            "external_config": {
                "version": "v2",
                "organization": "alias-org"
            }
        }))
        .unwrap();

        let client = InfluxdbClient::new_for_config("http://localhost:8086", &config, Duration::from_secs(1)).unwrap();

        assert_eq!(client.version, InfluxdbApiVersion::V2);
        assert_eq!(client.org.as_deref(), Some("alias-org"));
    }

    #[test]
    fn parses_flux_csv_annotations() {
        let csv = "\
#datatype,string,long,dateTime:RFC3339,string,double
#group,false,false,false,true,false
#default,_result,,,,
,result,table,_time,_measurement,_value
,,0,2026-07-06T00:00:00Z,cpu,42.5
";

        let result = parse_flux_csv(csv, Instant::now()).unwrap();

        assert_eq!(result.columns, vec!["result", "table", "_time", "_measurement", "_value"]);
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0][3], serde_json::Value::String("cpu".to_string()));
        assert_eq!(result.rows[0][4], serde_json::Value::String("42.5".to_string()));
    }

    fn v3_config(external: serde_json::Value) -> ConnectionConfig {
        serde_json::from_value(json!({
            "id": "influx-v3",
            "name": "InfluxDB 3",
            "db_type": "influxdb",
            "host": "127.0.0.1",
            "port": 8181,
            "username": "ignored-user",
            "password": "apiv3-token",
            "database": "sensors",
            "external_config": external
        }))
        .unwrap()
    }

    #[test]
    fn v3_config_uses_bearer_token_and_v3_urls() {
        let config = v3_config(json!({ "version": "3" }));

        let client = InfluxdbClient::new_for_config("http://localhost:8181/", &config, Duration::from_secs(1)).unwrap();

        assert_eq!(client.version, InfluxdbApiVersion::V3);
        assert_eq!(client.username, None);
        assert_eq!(client.password.as_deref(), Some("apiv3-token"));
        assert_eq!(build_v3_query_url(&client, V3QueryLanguage::Sql), "http://localhost:8181/api/v3/query_sql");
        assert_eq!(
            build_v3_query_url(&client, V3QueryLanguage::InfluxQl),
            "http://localhost:8181/api/v3/query_influxql"
        );
        assert_eq!(
            build_v3_databases_url(&client),
            "http://localhost:8181/api/v3/configure/database?format=json"
        );
    }

    #[test]
    fn v3_version_accepts_prefixed_alias() {
        let client =
            InfluxdbClient::new_for_config("http://localhost:8181", &v3_config(json!({ "version": "v3" })), Duration::from_secs(1))
                .unwrap();

        assert_eq!(client.version, InfluxdbApiVersion::V3);
    }

    #[test]
    fn v3_databases_accepts_all_known_shapes() {
        let wrapped = json!({ "databases": ["alpha", "beta"] });
        assert_eq!(extract_v3_database_names(&wrapped), vec!["alpha", "beta"]);

        let iox = json!([{ "iox::database": "sensors" }, { "iox::database": "  " }]);
        assert_eq!(extract_v3_database_names(&iox), vec!["sensors"]);

        let db_name = json!([{ "db_name": "metrics", "retention_period": "7d" }]);
        assert_eq!(extract_v3_database_names(&db_name), vec!["metrics"]);

        let plain = json!(["one", "two"]);
        assert_eq!(extract_v3_database_names(&plain), vec!["one", "two"]);

        assert!(extract_v3_database_names(&json!({ "error": "nope" })).is_empty());
    }

    #[test]
    fn parses_v3_json_rows_preserving_column_order() {
        let body = r#"[
            {"time":"2026-02-25T20:19:34Z","room":"Kitchen","temp":72.0},
            {"time":"2026-02-25T20:20:34Z","room":"Living room","temp":71.5,"co":9}
        ]"#;

        let result = parse_v3_json_rows(body, Instant::now()).unwrap();

        assert_eq!(result.columns, vec!["time", "room", "temp", "co"]);
        assert_eq!(result.rows.len(), 2);
        assert_eq!(result.affected_rows, 2);
        assert_eq!(result.rows[0][1], serde_json::Value::String("Kitchen".to_string()));
        // Missing keys are padded so every row matches the column count.
        assert_eq!(result.rows[0].len(), 4);
        assert_eq!(result.rows[0][3], serde_json::Value::Null);
        assert_eq!(result.rows[1][3], json!(9));
    }

    #[test]
    fn parses_empty_v3_response() {
        let result = parse_v3_json_rows("", Instant::now()).unwrap();

        assert!(result.columns.is_empty());
        assert!(result.rows.is_empty());
        assert_eq!(result.affected_rows, 0);
    }

    #[test]
    fn routes_only_influxql_specific_statements_to_influxql() {
        assert!(looks_like_influxql_query("SHOW MEASUREMENTS"));
        assert!(looks_like_influxql_query("  show tag keys from cpu"));
        assert!(looks_like_influxql_query("SHOW FIELD KEYS"));
        // `SHOW TABLES` is valid DataFusion SQL, so it must stay on the SQL endpoint.
        assert!(!looks_like_influxql_query("SHOW TABLES"));
        assert!(!looks_like_influxql_query("SELECT * FROM cpu"));
    }

    #[test]
    fn normalizes_arrow_types_for_v3_columns() {
        assert_eq!(normalize_v3_data_type("Timestamp(Nanosecond, None)"), "timestamp");
        assert_eq!(normalize_v3_data_type("Dictionary(Int32, Utf8)"), "string");
        assert_eq!(normalize_v3_data_type("Float64"), "float");
        assert_eq!(normalize_v3_data_type("Boolean"), "boolean");
        assert_eq!(normalize_v3_data_type("SomethingElse"), "SomethingElse");

        assert!(is_v3_time_column("time", "Timestamp(Nanosecond, None)"));
        assert!(is_v3_tag_column("Dictionary(Int32, Utf8)"));
        assert!(!is_v3_tag_column("Float64"));
    }

    #[test]
    fn escapes_single_quotes_in_v3_identifiers() {
        assert_eq!(escape_sql_string("it's"), "it''s");
    }

    #[test]
    fn recognizes_influx_system_columns() {
        assert!(is_influx_system_column("_start"));
        assert!(is_influx_system_column("_measurement"));
        assert!(!is_influx_system_column("host"));
        assert!(!is_influx_system_column("usage"));
    }

    #[test]
    fn extracts_v3_drop_table_name() {
        assert_eq!(extract_drop_table_name("DROP MEASUREMENT cpu;"), Some("cpu".to_string()));
        assert_eq!(extract_drop_table_name("drop measurement cpu"), Some("cpu".to_string()));
        assert_eq!(extract_drop_table_name("DROP TABLE \"my-table\";"), Some("my-table".to_string()));
        assert_eq!(
            extract_drop_table_name("DROP MEASUREMENT IF EXISTS sensors;"),
            Some("sensors".to_string())
        );
        assert_eq!(extract_drop_table_name("SELECT * FROM cpu"), None);
        assert_eq!(extract_drop_table_name("SHOW MEASUREMENTS"), None);
        assert_eq!(extract_drop_table_name("DROP MEASUREMENT"), None);
    }

    #[test]
    fn flags_v3_soft_deleted_tables() {
        assert!(is_v3_soft_deleted_table("cpu-2025-12-31T23:59:59Z"));
        assert!(is_v3_soft_deleted_table("sensors-2025-12-31T00:00:00Z"));
        assert!(!is_v3_soft_deleted_table("cpu"));
        assert!(!is_v3_soft_deleted_table("my-table"));
        assert!(!is_v3_soft_deleted_table("cpu-01"));
        assert!(!is_v3_soft_deleted_table("my-table-2024"));
    }
}
