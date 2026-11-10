//! The self-hosted sync backend (SYNC-004, SYNC-013): a thin HTTP shell.
//!
//! Every protocol decision — ordering, idempotency, conflicts — lives in
//! `marius-sync-service`, where the root workspace tests it on every
//! platform. This binary adds a socket, bearer-token authentication, and
//! nothing else. Run it behind a TLS-terminating reverse proxy for
//! anything beyond the loopback; clients refuse plain HTTP to non-local
//! hosts on their side too.
//!
//!     MARIUS_SYNC_TOKEN=<secret> marius-sync-server [store.db] [addr]

use std::sync::{Arc, Mutex};

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use marius_model::{DeviceId, PushRequest, VaultId};
use marius_sync_service::Store;

#[derive(Clone)]
struct App {
    store: Arc<Mutex<Store>>,
    token: Option<Arc<str>>,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    let mut args = std::env::args().skip(1);
    let store_path = args.next().unwrap_or_else(|| "marius-sync.db".to_owned());
    let addr = args.next().unwrap_or_else(|| "127.0.0.1:8787".to_owned());
    let token = std::env::var("MARIUS_SYNC_TOKEN").ok();
    if token.is_none() {
        tracing::warn!(
            "MARIUS_SYNC_TOKEN is not set: the server is open to anyone who can reach it"
        );
    }

    let store = Store::open(std::path::Path::new(&store_path)).expect("opening the store");
    let app = App {
        store: Arc::new(Mutex::new(store)),
        token: token.map(Into::into),
    };

    let router = Router::new()
        .route("/v1/vaults/{vault}/push", post(push))
        .route("/v1/vaults/{vault}/pull", get(pull))
        .with_state(app);

    tracing::info!("marius-sync-server on {addr}, store {store_path}");
    let listener = tokio::net::TcpListener::bind(&addr).await.expect("bind");
    axum::serve(listener, router).await.expect("serve");
}

fn authorized(app: &App, headers: &HeaderMap) -> bool {
    let Some(expected) = &app.token else {
        return true;
    };
    headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|presented| presented == expected.as_ref())
}

async fn push(
    State(app): State<App>,
    Path(vault): Path<String>,
    headers: HeaderMap,
    Json(request): Json<PushRequest>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    if !authorized(&app, &headers) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let vault: VaultId = vault.parse().map_err(|_| StatusCode::BAD_REQUEST)?;
    let mut store = app.store.lock().expect("store lock");
    let response = store
        .push(vault, &request)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(serde_json::to_value(response).expect("serialisable")))
}

#[derive(serde::Deserialize)]
struct PullParams {
    #[serde(default)]
    after: u64,
    device: String,
    #[serde(default = "default_limit")]
    limit: usize,
}

fn default_limit() -> usize {
    500
}

async fn pull(
    State(app): State<App>,
    Path(vault): Path<String>,
    headers: HeaderMap,
    Query(params): Query<PullParams>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    if !authorized(&app, &headers) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let vault: VaultId = vault.parse().map_err(|_| StatusCode::BAD_REQUEST)?;
    let device: DeviceId = params.device.parse().map_err(|_| StatusCode::BAD_REQUEST)?;
    let store = app.store.lock().expect("store lock");
    let response = store
        .pull(vault, device, params.after, params.limit.min(2_000))
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(serde_json::to_value(response).expect("serialisable")))
}
