use crate::{
    model::{Document, Entry, MAX_TOTAL},
    service::Service,
};
use axum::{
    extract::{DefaultBodyLimit, Request, State},
    http::{HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

#[derive(Clone)]
pub enum Auth {
    Gateway,
    DevelopmentToken(String),
}

#[derive(Clone)]
struct WebState {
    service: Arc<Service>,
    auth: Auth,
}

type ApiResult = Result<Json<serde_json::Value>, ApiError>;
struct ApiError(anyhow::Error);
impl From<anyhow::Error> for ApiError {
    fn from(value: anyhow::Error) -> Self {
        Self(value)
    }
}
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let message = self.0.to_string();
        let status = if message.contains("conflict:") || message.contains("recovery:") {
            StatusCode::CONFLICT
        } else {
            StatusCode::BAD_REQUEST
        };
        (status, Json(json!({"error": message}))).into_response()
    }
}

fn denied(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}

async fn authorize(State(state): State<WebState>, request: Request, next: Next) -> Response {
    let headers = request.headers();
    let authorized = match &state.auth {
        Auth::Gateway => {
            headers.get("x-trim-isadmin").is_some_and(|s| s == "true")
                && headers
                    .get("x-trim-userid")
                    .and_then(|s| s.to_str().ok())
                    .is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
        }
        Auth::DevelopmentToken(token) => headers
            .get("authorization")
            .and_then(|s| s.to_str().ok())
            .is_some_and(|s| {
                s.strip_prefix("Bearer ")
                    .is_some_and(|given| constant_time_equal(given.as_bytes(), token.as_bytes()))
            }),
    };
    if !authorized {
        return denied(
            StatusCode::FORBIDDEN,
            "需要管理员身份；本地开发请填写访问令牌",
        );
    }
    if headers
        .get("sec-fetch-site")
        .is_some_and(|s| s == "cross-site")
    {
        return denied(StatusCode::FORBIDDEN, "cross-site requests are refused");
    }
    if request.method() != axum::http::Method::GET
        && headers
            .get("x-switchhosts-request")
            .is_none_or(|s| s != "1")
    {
        return denied(StatusCode::FORBIDDEN, "missing request protection header");
    }
    next.run(request).await
}

fn constant_time_equal(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter()
        .zip(b)
        .fold(0_u8, |difference, (x, y)| difference | (x ^ y))
        == 0
}

async fn security_headers(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert("cache-control", "no-store".parse().unwrap());
    headers.insert("x-content-type-options", "nosniff".parse().unwrap());
    headers.insert("referrer-policy", "no-referrer".parse().unwrap());
    headers.insert("content-security-policy", "default-src 'self'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self' data:; frame-ancestors 'self'; base-uri 'self'; form-action 'none'; object-src 'none'".parse().unwrap());
    response
}

pub fn router(service: Arc<Service>, auth: Auth) -> Router {
    let state = WebState { service, auth };
    let api = Router::new()
        .route("/state", get(read_state))
        .route("/entry", post(save_entry))
        .route("/delete", post(delete_entry))
        .route("/refresh", post(refresh))
        .route("/restore", post(restore))
        .route("/reconcile", post(reconcile))
        .route("/keep-current", post(keep_current))
        .route("/export", get(export))
        .route("/import", post(import))
        .route_layer(middleware::from_fn_with_state(state.clone(), authorize));
    let app = Router::new()
        .route(
            "/",
            get(|| async { Html(include_str!("../../web/index.html")) }),
        )
        .route(
            "/app.js",
            get(|| async {
                (
                    [("content-type", "text/javascript; charset=utf-8")],
                    include_str!("../../web/app.js"),
                )
            }),
        )
        .route(
            "/style.css",
            get(|| async {
                (
                    [("content-type", "text/css; charset=utf-8")],
                    include_str!("../../web/style.css"),
                )
            }),
        )
        .nest("/api", api)
        .layer(DefaultBodyLimit::max(MAX_TOTAL * 8))
        .with_state(state);
    // Canonical prefix works both as a fnOS iframe and on loopback in development.
    Router::new()
        .nest("/app/switchhosts", app)
        .route(
            "/app/switchhosts/",
            get(|| async { Html(include_str!("../../web/index.html")) }),
        )
        .layer(middleware::from_fn(security_headers))
}

async fn read_state(State(state): State<WebState>) -> ApiResult {
    let document = state.service.document().await;
    let (hosts, hosts_error) = match state.service.target.read().await {
        Ok(text) => (Some(text), None),
        Err(error) => (None, Some(error.to_string())),
    };
    Ok(Json(
        json!({"document": document, "hosts": hosts, "hosts_error": hosts_error, "recovery_required": state.service.recovery_required()}),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Save {
    revision: u64,
    entry: Entry,
}
async fn save_entry(State(state): State<WebState>, Json(input): Json<Save>) -> ApiResult {
    state
        .service
        .save_entry(input.revision, input.entry)
        .await?;
    Ok(Json(json!({"ok": true})))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Id {
    revision: u64,
    id: String,
}
async fn delete_entry(State(state): State<WebState>, Json(input): Json<Id>) -> ApiResult {
    state.service.delete(input.revision, &input.id).await?;
    Ok(Json(json!({"ok": true})))
}
async fn restore(State(state): State<WebState>, Json(input): Json<Id>) -> ApiResult {
    state.service.restore(input.revision, &input.id).await?;
    Ok(Json(json!({"ok": true})))
}
async fn refresh(State(state): State<WebState>, Json(input): Json<Id>) -> ApiResult {
    state
        .service
        .document()
        .await
        .check_revision(input.revision)?;
    state.service.refresh(&input.id).await?;
    Ok(Json(json!({"ok": true})))
}

async fn reconcile(State(state): State<WebState>, _headers: HeaderMap) -> ApiResult {
    state.service.reconcile().await?;
    Ok(Json(json!({"ok": true})))
}
#[derive(Deserialize)]
struct Revision {
    revision: u64,
}
async fn keep_current(State(state): State<WebState>, Json(input): Json<Revision>) -> ApiResult {
    state.service.keep_current(input.revision).await?;
    Ok(Json(json!({"ok": true})))
}
async fn export(State(state): State<WebState>) -> ApiResult {
    let mut document = state.service.document().await;
    document.history.clear();
    Ok(Json(
        serde_json::to_value(document).map_err(anyhow::Error::from)?,
    ))
}
#[derive(Deserialize)]
struct Import {
    revision: u64,
    document: Document,
}
async fn import(State(state): State<WebState>, Json(input): Json<Import>) -> ApiResult {
    state.service.import(input.revision, input.document).await?;
    Ok(Json(json!({"ok": true})))
}
