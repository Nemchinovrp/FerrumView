mod inspect;
mod relay;

use anyhow::{Context, Result};
use axum::{Json, Router, extract::State, response::Html, routing::get};
use serde::Serialize;

#[derive(Clone)]
struct Config {
    stream_url: String,
    relays: relay::Relays,
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

    let relays = relay::Relays::new().await?;
    let app = Router::new()
        .route(
            "/",
            get(|| async { Html(include_str!("../static/index.html")) }),
        )
        .route("/api/config", get(config))
        .route("/relay/{id}/{name}", get(relay::serve))
        .with_state(Config {
            stream_url: first,
            relays: relays.clone(),
        });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:6523")
        .await
        .context("failed to bind http://127.0.0.1:6523")?;
    tracing::info!("Open http://127.0.0.1:6523 in your browser");
    let result = axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await;
    relays.shutdown().await;
    result?;
    Ok(())
}

#[derive(Serialize)]
struct Camera {
    name: String,
    url: String,
}

#[derive(Serialize)]
struct CameraConfig {
    stream_url: String,
    cameras: Vec<Camera>,
}

fn parse_cameras(contents: &str) -> Result<Vec<Camera>> {
    contents
        .trim_start_matches('\u{feff}')
        .lines()
        .enumerate()
        .filter_map(|(index, line)| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            Some((|| {
                let (name, url) = line
                    .split_once('|')
                    .with_context(|| format!("Строка {}: ожидается Название | URL", index + 1))?;
                let (name, url) = (name.trim(), url.trim());
                let uri: axum::http::Uri = url
                    .split('#')
                    .next()
                    .unwrap_or(url)
                    .parse()
                    .with_context(|| format!("Строка {}: некорректный URL", index + 1))?;
                anyhow::ensure!(
                    !name.is_empty()
                        && matches!(uri.scheme_str(), Some("http" | "https"))
                        && uri.host().is_some(),
                    "Строка {}: нужны название и HTTP(S) URL",
                    index + 1
                );
                Ok(Camera {
                    name: name.to_owned(),
                    url: url.to_owned(),
                })
            })())
        })
        .collect()
}

async fn config(
    State(config): State<Config>,
) -> Result<
    (
        [(axum::http::HeaderName, &'static str); 1],
        Json<CameraConfig>,
    ),
    (axum::http::StatusCode, String),
> {
    let result = async {
        let contents = match tokio::fs::read_to_string("streams.txt").await {
            Ok(contents) => contents,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                match tokio::fs::read_to_string("stream.txt").await {
                    Ok(contents) => contents,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
                    Err(error) => {
                        return Err(
                            anyhow::Error::new(error).context("Не удалось прочитать stream.txt")
                        );
                    }
                }
            }
            Err(error) => {
                return Err(anyhow::Error::new(error).context("Не удалось прочитать streams.txt"));
            }
        };
        let mut cameras = parse_cameras(&contents)?;
        for camera in &mut cameras {
            camera.url = config.relays.local_url(&camera.url).await;
        }
        let stream_url = config.relays.local_url(&config.stream_url).await;
        Ok((
            [(axum::http::header::CACHE_CONTROL, "no-store")],
            Json(CameraConfig {
                stream_url,
                cameras,
            }),
        ))
    }
    .await;
    result.map_err(|error: anyhow::Error| {
        (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            error.to_string(),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn camera_file_preserves_urls_and_ignores_comments() {
        let cameras = parse_cameras("\u{feff}# Камеры\n\n Двор | https://example.com/live.m3u8?token=a=b&x=1#video=copy\r\n").unwrap();
        assert_eq!(cameras.len(), 1);
        assert_eq!(cameras[0].name, "Двор");
        assert_eq!(
            cameras[0].url,
            "https://example.com/live.m3u8?token=a=b&x=1#video=copy"
        );
    }

    #[test]
    fn invalid_camera_reports_line_without_exposing_url() {
        for line in [
            "Название без ссылки",
            " | https://example.com",
            "Двор | javascript:secret",
            "Двор | https://",
        ] {
            let error = parse_cameras(&format!("# header\n{line}"))
                .err()
                .unwrap()
                .to_string();
            assert!(error.contains("Строка 2"));
            assert!(!error.contains("secret"));
        }
    }
}
