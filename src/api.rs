use crate::RoasTrie;
use axum::extract::{Query, State};
use axum::http::Method;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use chrono::DateTime;
use clap::Args;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::Arc;
use tokio::sync::RwLock;
use tower_http::cors::{Any, CorsLayer};
use tracing::warn;

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
}

#[derive(Serialize, Deserialize)]
pub struct RoasSearchResult {
    pub count: usize,
    pub error: Option<String>,
    pub data: Vec<RoasSearchResultEntry>,
    pub meta: Option<Meta>,
    pub page: usize,
    pub page_size: usize,
}

#[derive(Serialize, Deserialize)]
pub struct Meta {
    pub latest_date: String,
}

#[derive(Serialize, Deserialize)]
pub struct RoasSearchResultEntry {
    pub prefix: String,
    pub max_len: u8,
    pub asn: u32,
    pub date_ranges: Vec<(String, String)>,
    pub current: bool,
}

async fn health(State(state): State<Arc<RwLock<RoasTrie>>>) -> impl IntoResponse {
    let trie = state.read().await;
    let (ipv4_count, ipv6_count) = trie.trie.len();
    let latest_ts = trie.latest_date;
    Json(json! ({
        "ipv4_roas_count": ipv4_count,
        "ipv6_roas_count": ipv6_count,
        "latest_date": DateTime::from_timestamp(latest_ts, 0).unwrap().naive_utc().date().to_string(),
    }))
    .into_response()
}

async fn search(
    query: Query<RoasSearchQuery>,
    State(state): State<Arc<RwLock<RoasTrie>>>,
) -> impl IntoResponse {
    let page = query.page.unwrap_or(0);
    let mut page_size = query.page_size.unwrap_or(100);
    if page_size > 1000 {
        warn!("page_size is too large, setting to 1000");
        page_size = 1000;
    }

    let trie = state.read().await;
    let latest_ts = trie.latest_date;
    let latest_date = DateTime::from_timestamp(latest_ts, 0)
        .unwrap()
        .naive_utc()
        .date();
    let mut results = trie.search(
        query.prefix.clone().map(|p| p.parse().unwrap()),
        query.asn,
        query.max_len,
        query.date.clone().map(|d| d.parse().unwrap()),
        query.current,
    );
    results.sort_by(|a, b| a.prefix.cmp(&b.prefix));
    let result_entries = results
        .iter()
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

    // filter result entries by page and page_size
    let result_entries = result_entries
        .into_iter()
        .skip(page * page_size)
        .take(page_size)
        .collect::<Vec<_>>();

    Json(RoasSearchResult {
        count: result_entries.len(),
        error: None,
        data: result_entries,
        meta: Some(Meta {
            latest_date: latest_date.to_string(),
        }),
        page,
        page_size,
    })
    .into_response()
}

pub async fn start_api_service(
    trie_lock: Arc<RwLock<RoasTrie>>,
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
