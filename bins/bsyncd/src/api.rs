//! Local control API: HTTP + JSON on 127.0.0.1, for the CLI and the tray
//! app. Every request needs `Authorization: Bearer <token>`, where the token
//! is a random value in `<config dir>/api.token` (readable only by this
//! user). The bound address is written to `<config dir>/api.addr`.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, Result};
use axum::extract::{Path as UrlPath, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use bsync_core::DeviceId;
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::daemon::{Control, DaemonStatus, Event};

pub fn token_path(dir: &Path) -> PathBuf {
    dir.join("api.token")
}

pub fn addr_path(dir: &Path) -> PathBuf {
    dir.join("api.addr")
}

/// Read the token, creating a random one if needed.
pub fn load_or_create_token(dir: &Path) -> Result<String> {
    let path = token_path(dir);
    if let Ok(t) = std::fs::read_to_string(&path) {
        let t = t.trim().to_owned();
        if !t.is_empty() {
            return Ok(t);
        }
    }
    let token: String = (0..4)
        .map(|_| format!("{:016x}", crate::daemon::random_u64()))
        .collect();
    write_private(&path, token.as_bytes())?;
    Ok(token)
}

#[cfg(unix)]
fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let _ = std::fs::remove_file(path);
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(data)?;
    Ok(())
}

#[cfg(not(unix))]
fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    // The per-user config directory is already private to the user.
    std::fs::write(path, data)?;
    Ok(())
}

// ----- wire types (shared with the CLI) -----

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BlockedDto {
    pub path: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FolderDto {
    pub id: String,
    pub path: String,
    /// "idle", "syncing", "paused" or "error".
    pub state: String,
    pub error: Option<String>,
    pub pending: usize,
    pub transfers: usize,
    pub local_entries: u64,
    pub shared_with: Vec<String>,
    pub blocked: Vec<BlockedDto>,
    pub conflicts: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PeerDto {
    pub id: String,
    pub name: String,
    pub connected: bool,
    pub address: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StatusDto {
    pub device_id: String,
    pub device_name: String,
    pub listen: Option<String>,
    pub folders: Vec<FolderDto>,
    pub peers: Vec<PeerDto>,
}

impl StatusDto {
    pub fn from_status(s: &DaemonStatus, config: &Config) -> Self {
        Self {
            device_id: s.device_id.map(|d| d.to_string()).unwrap_or_default(),
            device_name: s.device_name.clone(),
            listen: s.listen.map(|a| a.to_string()),
            folders: s
                .folders
                .iter()
                .map(|f| {
                    let state = if f.error.is_some() {
                        "error"
                    } else if f.paused {
                        "paused"
                    } else if f.engine.pending > 0 || f.engine.in_flight > 0 {
                        "syncing"
                    } else {
                        "idle"
                    };
                    FolderDto {
                        id: f.id.clone(),
                        path: f.path.display().to_string(),
                        state: state.into(),
                        error: f.error.clone(),
                        pending: f.engine.pending,
                        transfers: f.engine.in_flight,
                        local_entries: f.local_entries,
                        shared_with: config
                            .folders
                            .iter()
                            .find(|c| c.id == f.id)
                            .map(|c| c.devices.clone())
                            .unwrap_or_default(),
                        blocked: f
                            .engine
                            .blocked
                            .iter()
                            .map(|(path, reason)| BlockedDto {
                                path: path.clone(),
                                reason: reason.clone(),
                            })
                            .collect(),
                        conflicts: f.conflicts.clone(),
                    }
                })
                .collect(),
            peers: s
                .peers
                .iter()
                .map(|p| PeerDto {
                    id: p.id.to_string(),
                    name: p.name.clone(),
                    connected: p.connected,
                    address: p.address.map(|a| a.to_string()),
                })
                .collect(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairRequest {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub addresses: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceRequest {
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FolderRequest {
    pub id: String,
    pub path: String,
    #[serde(default)]
    pub devices: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OkDto {
    pub ok: bool,
    pub message: String,
}

// ----- server -----

#[derive(Clone)]
struct ApiState {
    events: mpsc::Sender<Event>,
    config_dir: PathBuf,
    token: String,
}

struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.0,
            Json(OkDto {
                ok: false,
                message: self.1,
            }),
        )
            .into_response()
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        Self(StatusCode::BAD_REQUEST, format!("{e:#}"))
    }
}

type ApiResult<T> = std::result::Result<Json<T>, ApiError>;

fn ok(message: impl Into<String>) -> ApiResult<OkDto> {
    Ok(Json(OkDto {
        ok: true,
        message: message.into(),
    }))
}

/// Ask the engine thread something, without blocking the async runtime.
async fn ask<T: Send + 'static>(
    state: &ApiState,
    make: impl FnOnce(mpsc::Sender<T>) -> Control + Send + 'static,
) -> std::result::Result<T, ApiError> {
    let events = state.events.clone();
    tokio::task::spawn_blocking(move || {
        let (tx, rx) = mpsc::channel();
        events
            .send(Event::Control(make(tx)))
            .map_err(|_| anyhow::anyhow!("daemon is stopping"))?;
        rx.recv_timeout(Duration::from_secs(30))
            .map_err(|_| anyhow::anyhow!("daemon did not answer"))
    })
    .await
    .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    .map_err(|e| ApiError(StatusCode::SERVICE_UNAVAILABLE, e.to_string()))
}

/// Change config.json and make the daemon pick it up.
async fn edit_config(
    state: &ApiState,
    edit: impl FnOnce(&mut Config) -> Result<()>,
) -> std::result::Result<(), ApiError> {
    let mut c = Config::load(&state.config_dir)?;
    edit(&mut c)?;
    c.save(&state.config_dir)?;
    ask(state, Control::Reload)
        .await?
        .map_err(|e| ApiError(StatusCode::BAD_REQUEST, e))
}

fn parse_device(s: &str) -> std::result::Result<DeviceId, ApiError> {
    s.parse()
        .map_err(|_| ApiError(StatusCode::BAD_REQUEST, format!("{s:?} is not a device ID")))
}

async fn status(State(state): State<ApiState>) -> ApiResult<StatusDto> {
    let s = ask(&state, Control::Status).await?;
    let config = Config::load(&state.config_dir)?;
    Ok(Json(StatusDto::from_status(&s, &config)))
}

async fn pair(State(state): State<ApiState>, Json(req): Json<PairRequest>) -> ApiResult<OkDto> {
    let id = parse_device(&req.id)?;
    edit_config(&state, |c| {
        c.pair(id, req.name, req.addresses);
        Ok(())
    })
    .await?;
    ok(format!("paired with {id}"))
}

async fn unpair(State(state): State<ApiState>, Json(req): Json<DeviceRequest>) -> ApiResult<OkDto> {
    let id = parse_device(&req.id)?;
    edit_config(&state, |c| {
        c.unpair(&id);
        Ok(())
    })
    .await?;
    ok(format!("unpaired {id}"))
}

async fn add_folder(
    State(state): State<ApiState>,
    Json(req): Json<FolderRequest>,
) -> ApiResult<OkDto> {
    let devices = req
        .devices
        .iter()
        .map(|d| parse_device(d))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let path = PathBuf::from(&req.path);
    if !path.is_absolute() {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "folder path must be absolute".into(),
        ));
    }
    std::fs::create_dir_all(&path)
        .with_context(|| format!("creating {}", path.display()))
        .map_err(ApiError::from)?;
    edit_config(&state, |c| c.add_folder(&req.id, path, &devices)).await?;
    ok(format!("added folder {}", req.id))
}

async fn share_folder(
    State(state): State<ApiState>,
    UrlPath(id): UrlPath<String>,
    Json(req): Json<DeviceRequest>,
) -> ApiResult<OkDto> {
    let device = parse_device(&req.id)?;
    edit_config(&state, |c| c.share_folder(&id, device)).await?;
    ok(format!("shared {id} with {device}"))
}

async fn set_paused(state: &ApiState, id: String, paused: bool) -> ApiResult<OkDto> {
    edit_config(state, |c| {
        c.folder_mut(&id)
            .with_context(|| format!("no folder {id:?}"))?
            .paused = paused;
        Ok(())
    })
    .await?;
    let id2 = id.clone();
    ask(state, move |tx| Control::Pause(id2, paused, tx))
        .await?
        .map_err(|e| ApiError(StatusCode::BAD_REQUEST, e))?;
    ok(format!(
        "{} {id}",
        if paused { "paused" } else { "resumed" }
    ))
}

async fn pause(State(state): State<ApiState>, UrlPath(id): UrlPath<String>) -> ApiResult<OkDto> {
    set_paused(&state, id, true).await
}

async fn resume(State(state): State<ApiState>, UrlPath(id): UrlPath<String>) -> ApiResult<OkDto> {
    set_paused(&state, id, false).await
}

async fn rescan(State(state): State<ApiState>, UrlPath(id): UrlPath<String>) -> ApiResult<OkDto> {
    let id2 = id.clone();
    ask(&state, move |tx| Control::Rescan(id2, tx))
        .await?
        .map_err(|e| ApiError(StatusCode::BAD_REQUEST, e))?;
    ok(format!("rescanned {id}"))
}

async fn auth(State(state): State<ApiState>, req: Request, next: Next) -> Response {
    let expected = format!("Bearer {}", state.token);
    let given = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    if given.map(str::as_bytes) != Some(expected.as_bytes()) {
        return ApiError(
            StatusCode::UNAUTHORIZED,
            "missing or wrong API token".into(),
        )
        .into_response();
    }
    next.run(req).await
}

fn router(state: ApiState) -> Router {
    Router::new()
        .route("/api/status", get(status))
        .route("/api/pair", post(pair))
        .route("/api/unpair", post(unpair))
        .route("/api/folders", post(add_folder))
        .route("/api/folders/{id}/share", post(share_folder))
        .route("/api/folders/{id}/pause", post(pause))
        .route("/api/folders/{id}/resume", post(resume))
        .route("/api/folders/{id}/rescan", post(rescan))
        .route_layer(middleware::from_fn_with_state(state.clone(), auth))
        .with_state(state)
}

/// Start serving on `addr` (loopback only). Must be called inside the
/// daemon's Tokio runtime. Returns the bound address.
pub async fn serve(
    addr: SocketAddr,
    events: mpsc::Sender<Event>,
    config_dir: &Path,
) -> Result<SocketAddr> {
    anyhow::ensure!(
        addr.ip().is_loopback(),
        "the control API only listens on loopback"
    );
    let token = load_or_create_token(config_dir)?;
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding the control API to {addr}"))?;
    let bound = listener.local_addr()?;
    std::fs::write(addr_path(config_dir), bound.to_string())?;
    let app = router(ApiState {
        events,
        config_dir: config_dir.to_path_buf(),
        token,
    });
    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            tracing::error!("control API stopped: {e}");
        }
    });
    Ok(bound)
}
