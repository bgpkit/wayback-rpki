use crate::RoasTrie;
use axum::extract::{Query, State};
use axum::http::Method;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use clap::Args;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use tower_http::cors::{Any, CorsLayer};

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
}

#[derive(Serialize, Deserialize)]
struct RoasSearchResult {
    prefix: String,
    max_len: u8,
    asn: u32,
    date_ranges: Vec<(String, String)>,
    current: bool,
}

async fn search(
    query: Query<RoasSearchQuery>,
    State(state): State<Arc<Mutex<RoasTrie>>>,
) -> impl IntoResponse {
    let trie = state.lock().unwrap();
    let latest_ts = trie.latest_date;
    let mut results = trie.search(
        query.prefix.clone().map(|p| p.parse().unwrap()),
        query.asn,
        query.max_len,
        query.date.clone().map(|d| d.parse().unwrap()),
        query.current,
    );
    results.sort_by(|a, b| a.prefix.cmp(&b.prefix));
    let results = results
        .iter()
        .map(|entry| RoasSearchResult {
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
    Json(results).into_response()
}

pub async fn start_api_service(
    trie: RoasTrie,
    host: String,
    port: u16,
    root: String,
) -> std::io::Result<()> {
    let cors_layer = CorsLayer::new()
        // allow `GET` and `POST` when accessing the resource
        .allow_methods([Method::GET, Method::POST])
        // allow requests from any origin
        .allow_origin(Any);

    let state = Arc::new(Mutex::new(trie));
    let app = Router::new()
        .route("/search", get(search))
        .with_state(state)
        .layer(cors_layer);
    let root_app = Router::new().nest(root.as_str(), app);

    let socket_str = format!("{}:{}", host, port);
    let listener = tokio::net::TcpListener::bind(socket_str).await?;
    tracing::info!("listening on {}", listener.local_addr().unwrap());
    axum::serve(listener, root_app).await.unwrap();

    Ok(())
}
