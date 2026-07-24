use crate::legacy::LegacyRoasTrie;
use crate::{RoasTrie, RpkiValidation};
use axum::extract::{Query, State};
use axum::http::{Method, StatusCode};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use chrono::{DateTime, NaiveDate};
use clap::Args;
use ipnet::IpNet;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::Arc;
use tokio::sync::RwLock;
use tower_http::cors::{Any, CorsLayer};
use tracing::warn;

/// The trie backend — either v2 (rkyv mmap) or v1 (legacy in-memory bincode).
/// Both implement the same query surface (search/validate/counts/latest_date).
pub enum TrieBackend {
    V2(RoasTrie),
    V1(LegacyRoasTrie),
}

impl TrieBackend {
    pub fn search(
        &self,
        prefix: Option<IpNet>,
        origin: Option<u32>,
        max_len: Option<u8>,
        date: Option<NaiveDate>,
        current: Option<bool>,
        exact: bool,
    ) -> Vec<crate::RoasLookupEntry> {
        match self {
            TrieBackend::V2(t) => t.search(prefix, origin, max_len, date, current, exact),
            TrieBackend::V1(t) => t.search(prefix, origin, max_len, date, current, exact),
        }
    }

    pub fn validate(&self, prefix: &IpNet, origin: u32, date_ts: i64) -> RpkiValidation {
        match self {
            TrieBackend::V2(t) => t.validate(prefix, origin, date_ts),
            TrieBackend::V1(t) => t.validate(prefix, origin, date_ts),
        }
    }

    pub fn counts(&self) -> (u64, u64) {
        match self {
            TrieBackend::V2(t) => t.counts(),
            TrieBackend::V1(t) => {
                let mut v4 = 0u64;
                let mut v6 = 0u64;
                for (prefix, entries) in t.trie.iter() {
                    let count = entries.len() as u64;
                    match prefix {
                        IpNet::V4(_) => v4 += count,
                        IpNet::V6(_) => v6 += count,
                    }
                }
                (v4, v6)
            }
        }
    }

    pub fn latest_date_ts(&self) -> i64 {
        match self {
            TrieBackend::V2(t) => t.latest_date_ts(),
            TrieBackend::V1(t) => t.latest_date,
        }
    }

    pub fn format_version(&self) -> u32 {
        match self {
            TrieBackend::V2(_) => crate::FORMAT_VERSION,
            TrieBackend::V1(_) => 1,
        }
    }
}

pub type SharedTrie = Arc<RwLock<TrieBackend>>;

#[derive(Args, Debug, Serialize, Deserialize)]
pub struct RoasSearchQuery {
    /// filter results by ASN exact match
    asn: Option<u32>,

    /// IP prefix to search ROAs for, e.g. `?prefix=1.1.1.0/24`.
    prefix: Option<String>,

    /// filter results by the max_len value
    max_len: Option<u8>,

    /// limit the date of the ROAs, format: YYYY-MM-DD, e.g. `?date=2022-01-01`
    date: Option<String>,

    /// filter results to whether ROA is still current
    current: Option<bool>,

    /// page number, starting from 0
    page: Option<usize>,

    /// number of items per page, maximum 1000
    page_size: Option<usize>,

    /// if true (default), only return exact prefix matches; if false, include supernets and subnets
    exact: Option<bool>,
}

#[derive(Serialize, Deserialize)]
pub struct RoasSearchResult {
    /// total number of matching entries (before pagination)
    pub total: usize,
    /// error message if any
    pub error: Option<String>,
    pub data: Vec<RoasSearchResultEntry>,
    pub meta: Option<Meta>,
    pub page: usize,
    pub page_size: usize,
}

#[derive(Serialize, Deserialize)]
pub struct Meta {
    pub latest_date: String,
    pub format_version: u32,
}

#[derive(Serialize, Deserialize)]
pub struct RoasSearchResultEntry {
    pub prefix: String,
    pub max_len: u8,
    pub asn: u32,
    pub date_ranges: Vec<(String, String)>,
    pub current: bool,
}

#[derive(Args, Debug, Serialize, Deserialize)]
pub struct ValidateQuery {
    /// IP prefix to validate, e.g. `?prefix=1.1.1.0/24` (required)
    prefix: String,

    /// origin ASN to validate (required)
    asn: u32,

    /// date for historical validation, format: YYYY-MM-DD (default: latest)
    date: Option<String>,
}

#[derive(Serialize)]
pub struct ValidateResult {
    pub prefix: String,
    pub asn: u32,
    pub date: String,
    pub result: String,
}

fn bad_request(msg: &str) -> axum::response::Response {
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({"error": msg})),
    )
        .into_response()
}

async fn health(State(state): State<SharedTrie>) -> impl IntoResponse {
    let trie = state.read().await;
    let (ipv4_count, ipv6_count) = trie.counts();
    Json(json!({
        "ipv4_roas_count": ipv4_count,
        "ipv6_roas_count": ipv6_count,
        "latest_date": DateTime::from_timestamp(trie.latest_date_ts(), 0)
            .unwrap()
            .naive_utc()
            .date()
            .to_string(),
        "format_version": trie.format_version(),
    }))
    .into_response()
}

async fn search(
    query: Query<RoasSearchQuery>,
    State(state): State<SharedTrie>,
) -> impl IntoResponse {
    let page = query.page.unwrap_or(0);
    let mut page_size = query.page_size.unwrap_or(100);
    if page_size > 1000 {
        warn!("page_size is too large, setting to 1000");
        page_size = 1000;
    }

    // Parse prefix and date early so we can return a clean 400 instead of
    // panicking (→ 500) on malformed input.
    let prefix: Option<IpNet> = match query.prefix.as_ref().map(|p| p.parse()) {
        Some(Ok(p)) => Some(p),
        Some(Err(_)) => return bad_request("invalid prefix"),
        None => None,
    };
    let date: Option<NaiveDate> = match query.date.as_ref().map(|d| d.parse()) {
        Some(Ok(d)) => Some(d),
        Some(Err(_)) => return bad_request("invalid date"),
        None => None,
    };

    let trie = state.read().await;
    let latest_ts = trie.latest_date_ts();
    let format_version = trie.format_version();
    let latest_date = DateTime::from_timestamp(latest_ts, 0)
        .unwrap()
        .naive_utc()
        .date();

    let results = trie.search(
        prefix,
        query.asn,
        query.max_len,
        date,
        query.current,
        query.exact.unwrap_or(true),
    );

    let total = results.len();

    // Results come back in deterministic trie (lexicographic prefix) order;
    // paginate without an additional full sort.
    let result_entries = results
        .iter()
        .skip(page * page_size)
        .take(page_size)
        .map(|entry| RoasSearchResultEntry {
            prefix: entry.prefix.to_string(),
            max_len: entry.max_len,
            asn: entry.origin,
            date_ranges: entry
                .dates_ranges
                .iter()
                .map(|(from, to)| (from.to_string(), to.to_string()))
                .collect(),
            current: entry.dates_ranges.iter().any(|(_from, to)| {
                to.and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp() >= latest_ts
            }),
        })
        .collect::<Vec<_>>();

    Json(RoasSearchResult {
        total,
        error: None,
        data: result_entries,
        meta: Some(Meta {
            latest_date: latest_date.to_string(),
            format_version,
        }),
        page,
        page_size,
    })
    .into_response()
}

async fn validate(
    query: Query<ValidateQuery>,
    State(state): State<SharedTrie>,
) -> impl IntoResponse {
    let prefix: IpNet = match query.prefix.parse() {
        Ok(p) => p,
        Err(_) => return bad_request("invalid prefix"),
    };
    let trie = state.read().await;
    let latest_ts = trie.latest_date_ts();
    let latest_date = DateTime::from_timestamp(latest_ts, 0)
        .unwrap()
        .naive_utc()
        .date();
    let date: NaiveDate = match query.date.as_ref().map(|d| d.parse()) {
        Some(Ok(d)) => d,
        Some(Err(_)) => return bad_request("invalid date"),
        None => latest_date,
    };
    let date_ts = date.and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp();

    let result = trie.validate(&prefix, query.asn, date_ts);
    let result_str = match result {
        RpkiValidation::Valid => "valid",
        RpkiValidation::Invalid => "invalid",
        RpkiValidation::Unknown => "unknown",
    };

    Json(ValidateResult {
        prefix: prefix.to_string(),
        asn: query.asn,
        date: date.to_string(),
        result: result_str.to_string(),
    })
    .into_response()
}

pub async fn start_api_service(
    trie_lock: SharedTrie,
    host: String,
    port: u16,
    root: String,
) -> std::io::Result<()> {
    let cors_layer = CorsLayer::new()
        // allow `GET` and `POST` when accessing the resource
        .allow_methods([Method::GET, Method::POST])
        // allow requests from any origin
        .allow_origin(Any);

    let app = Router::new()
        .route("/search", get(search))
        .route("/validate", get(validate))
        .route("/health", get(health))
        .with_state(trie_lock)
        .layer(cors_layer);
    let root_app = if root == "/" {
        // If root is "/", just use the app router directly
        app
    } else {
        // Otherwise, nest under the specified path
        Router::new().nest(root.as_str(), app)
    };

    let socket_str = format!("{}:{}", host, port);
    let listener = tokio::net::TcpListener::bind(socket_str).await?;
    tracing::info!("listening on {}", listener.local_addr()?);
    axum::serve(listener, root_app).await?;

    Ok(())
}
