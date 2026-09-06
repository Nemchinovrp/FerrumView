mod inspect;

use anyhow::{Context, Result};
use axum::{Json, Router, extract::State, response::Html, routing::get};
use serde::Serialize;

#[derive(Clone, Serialize)]
struct Config {
    stream_url: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let mut args = std::env::args().skip(1);
    let first = args.next().unwrap_or_default();
    if first == "--inspect" {
        let url = args
            .next()
            .context("usage: ferrumview --inspect <hls-url>")?;
        return tokio::task::spawn_blocking(move || inspect::run(&url)).await?;
    }

    let app = Router::new()
        .route(
            "/",
            get(|| async { Html(include_str!("../static/index.html")) }),
        )
        .route("/api/config", get(config))
        .with_state(Config { stream_url: first });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:3000")
        .await
        .context("failed to bind http://127.0.0.1:3000")?;
    tracing::info!("Open http://127.0.0.1:3000 in your browser");
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}

async fn config(
    State(config): State<Config>,
) -> ([(axum::http::HeaderName, &'static str); 1], Json<Config>) {
    (
        [(axum::http::header::CACHE_CONTROL, "no-store")],
        Json(config),
    )
}
