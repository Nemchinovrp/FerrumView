use anyhow::{Context, Result};
use axum::{
    extract::{Path, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use std::{
    collections::HashMap,
    path::PathBuf,
    process::Stdio,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    process::{Child, Command},
    sync::Mutex,
};

#[derive(Clone)]
pub struct Relays(Arc<Mutex<Registry>>);
struct Registry {
    root: PathBuf,
    entries: Vec<Relay>,
    ids: HashMap<String, usize>,
}
struct Relay {
    url: String,
    child: Option<Child>,
    started: Option<SystemTime>,
}

pub fn is_dsi(url: &str) -> bool {
    let Ok(uri) = url
        .split('#')
        .next()
        .unwrap_or(url)
        .parse::<axum::http::Uri>()
    else {
        return false;
    };
    let host = uri.host().unwrap_or("").to_ascii_lowercase();
    matches!(uri.scheme_str(), Some("http" | "https"))
        && (host == "dsi.ru" || host.ends_with(".dsi.ru"))
}

impl Relays {
    pub async fn new() -> Result<Self> {
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let root = std::env::temp_dir().join(format!("ferrumview-{}-{nonce}", std::process::id()));
        tokio::fs::create_dir(&root).await?;
        tokio::fs::write(
            root.join("openssl.cnf"),
            include_str!("../config/dsi-openssl.cnf"),
        )
        .await?;
        Ok(Self(Arc::new(Mutex::new(Registry {
            root,
            entries: Vec::new(),
            ids: HashMap::new(),
        }))))
    }

    pub async fn local_url(&self, url: &str) -> String {
        if !is_dsi(url) {
            return url.to_owned();
        }
        let url = url.split('#').next().unwrap_or(url);
        let mut registry = self.0.lock().await;
        let id = if let Some(id) = registry.ids.get(url) {
            *id
        } else {
            let id = registry.entries.len();
            registry.entries.push(Relay {
                url: url.to_owned(),
                child: None,
                started: None,
            });
            registry.ids.insert(url.to_owned(), id);
            id
        };
        format!("/relay/{id}/index.m3u8")
    }

    async fn file(&self, id: usize, name: &str) -> Result<Option<Vec<u8>>> {
        let mut registry = self.0.lock().await;
        let root = registry.root.clone();
        let directory = root.join(id.to_string());
        let entry = registry.entries.get_mut(id).context("Unknown camera")?;
        let alive = match entry.child.as_mut() {
            Some(child) => child.try_wait()?.is_none(),
            None => false,
        };
        let modified = tokio::fs::metadata(directory.join("index.m3u8"))
            .await
            .ok()
            .and_then(|m| m.modified().ok())
            .or(entry.started);
        let stale = modified
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > Duration::from_secs(45));
        if name == "index.m3u8" && (!alive || stale) {
            // Limit retries when a remote camera refuses the connection.
            if !alive
                && entry
                    .started
                    .and_then(|t| t.elapsed().ok())
                    .is_some_and(|age| age < Duration::from_secs(5))
            {
                return Ok(None);
            }
            if let Some(mut child) = entry.child.take() {
                let _ = child.kill().await;
            }
            if tokio::fs::try_exists(&directory).await? {
                tokio::fs::remove_dir_all(&directory).await?;
            }
            tokio::fs::create_dir(&directory).await?;
            entry.started = Some(SystemTime::now());
            entry.child = Some(
                Command::new("ffmpeg")
                    .env("OPENSSL_CONF", root.join("openssl.cnf"))
                    .args([
                        "-nostdin",
                        "-hide_banner",
                        "-loglevel",
                        "error",
                        "-rw_timeout",
                        "15000000",
                        "-tls_verify",
                        "1",
                        "-user_agent",
                        "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 Chrome/150.0.0.0 Safari/537.36",
                        "-headers",
                        "Origin: https://video.dsi.ru\r\nReferer: https://video.dsi.ru/\r\n",
                        "-i",
                    ])
                    .arg(&entry.url)
                    .args([
                        "-map",
                        "0:v:0",
                        "-map",
                        "0:a:0?",
                        "-c:v",
                        "copy",
                        "-c:a",
                        "aac",
                        "-f",
                        "hls",
                        "-hls_time",
                        "2",
                        "-hls_list_size",
                        "6",
                        "-hls_delete_threshold",
                        "2",
                        "-hls_start_number_source",
                        "epoch",
                        "-hls_flags",
                        "delete_segments+temp_file+omit_endlist",
                        "-hls_segment_filename",
                        "segment_%d.ts",
                        "index.m3u8",
                    ])
                    .current_dir(&directory)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .kill_on_drop(true)
                    .spawn()
                    .context("Failed to start FFmpeg")?,
            );
            tracing::info!(camera = id, "Started DSI relay");
        }
        drop(registry);
        match tokio::fs::read(directory.join(name)).await {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    pub async fn shutdown(&self) {
        let mut registry = self.0.lock().await;
        for entry in &mut registry.entries {
            if let Some(mut child) = entry.child.take() {
                let _ = child.kill().await;
            }
        }
        let _ = tokio::fs::remove_dir_all(&registry.root).await;
    }
}

fn valid_file(name: &str) -> bool {
    name == "index.m3u8"
        || name
            .strip_prefix("segment_")
            .and_then(|s| s.strip_suffix(".ts"))
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

pub async fn serve(
    State(config): State<crate::Config>,
    Path((id, name)): Path<(usize, String)>,
) -> Response {
    if !valid_file(&name) {
        return StatusCode::NOT_FOUND.into_response();
    }
    {
        match config.relays.file(id, &name).await {
            Ok(Some(bytes)) => {
                return (
                    [
                        (
                            header::CONTENT_TYPE,
                            if name.ends_with(".m3u8") {
                                "application/vnd.apple.mpegurl"
                            } else {
                                "video/mp2t"
                            },
                        ),
                        (header::CACHE_CONTROL, "no-store"),
                    ],
                    bytes,
                )
                    .into_response();
            }
            Ok(None) => {}
            Err(_) => return (StatusCode::BAD_GATEWAY, "Camera relay unavailable").into_response(),
        }
    }
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [
            (header::RETRY_AFTER, "3"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        "Camera is connecting",
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn legacy_tls_is_limited_to_dsi_hosts() {
        assert!(is_dsi(
            "https://video9.dsi.ru:8091/hls/test/index.m3u8#video=copy"
        ));
        for url in [
            "https://dsi.ru.evil.com/a",
            "https://evildsi.ru/a",
            "https://vs6.newbwc.ru/a",
            "file:///dsi.ru",
        ] {
            assert!(!is_dsi(url));
        }
    }
    #[test]
    fn only_generated_hls_files_are_served() {
        assert!(valid_file("index.m3u8"));
        assert!(valid_file("segment_12345.ts"));
        for file in [
            "../openssl.cnf",
            "openssl.cnf",
            "segment_../a.ts",
            "segment_.ts",
            "index.m3u8.tmp",
        ] {
            assert!(!valid_file(file));
        }
    }
}
