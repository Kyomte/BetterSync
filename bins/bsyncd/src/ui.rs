//! The status page (`GET /`): a single static HTML page that the tray app
//! opens in the default browser as `http://127.0.0.1:<port>/#token=<token>`.
//! The page itself is public; every piece of data comes from the API with
//! the token, which stays in the URL fragment (never sent to the server).

use axum::Router;
use axum::http::header;
use axum::response::IntoResponse;
use axum::routing::get;

const PAGE: &str = include_str!("ui.html");

async fn page() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
            // The page only talks to its own origin.
            (
                header::CONTENT_SECURITY_POLICY,
                "default-src 'self'; script-src 'unsafe-inline'; style-src 'unsafe-inline'",
            ),
        ],
        PAGE,
    )
}

pub fn router<S: Clone + Send + Sync + 'static>() -> Router<S> {
    Router::new().route("/", get(page))
}
