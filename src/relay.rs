use anyhow::{Context, Result};
use axum::{
    Json,
    extract::{Path, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use std::{
    collections::HashMap,
    path::PathBuf,
    process::Stdio,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::{
    process::Command,
    sync::{Mutex, Semaphore},
    task::JoinHandle,
};

#[derive(Clone)]
pub struct Relays(Arc<Mutex<Registry>>);
struct Registry {
    root: PathBuf,
    entries: Vec<Relay>,
    ids: HashMap<String, usize>,
    requests: Arc<Semaphore>,
}
struct Relay {
    source: Source,
    task: Option<JoinHandle<()>>,
    state: Arc<Mutex<WorkerState>>,
}
#[derive(Clone)]
enum Source {
    Url(String),
    Account(String),
}
struct WorkerState {
    message: String,
    last_access: Instant,
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
        let root = std::env::temp_dir().join(format!(
            "ferrumview-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
        ));
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
            requests: Arc::new(Semaphore::new(3)),
        }))))
    }
    async fn register(&self, key: String, source: Source) -> String {
        let mut registry = self.0.lock().await;
        let id = if let Some(id) = registry.ids.get(&key) {
            *id
        } else {
            let id = registry.entries.len();
            registry.entries.push(Relay {
                source,
                task: None,
                state: Arc::new(Mutex::new(WorkerState {
                    message: "Подключение…".into(),
                    last_access: Instant::now(),
                })),
            });
            registry.ids.insert(key, id);
            id
        };
        format!("/relay/{id}/index.m3u8")
    }
    pub async fn account_url(&self, id: String) -> String {
        self.register(format!("account:{id}"), Source::Account(id))
            .await
    }
    pub async fn local_url(&self, url: &str) -> String {
        if !is_dsi(url) {
            return url.to_owned();
        }
        let url = url.split('#').next().unwrap_or(url).to_owned();
        self.register(url.clone(), Source::Url(url)).await
    }
    async fn file(&self, id: usize, name: &str) -> Result<Option<Vec<u8>>> {
        let mut registry = self.0.lock().await;
        let root = registry.root.clone();
        let requests = registry.requests.clone();
        let entry = registry.entries.get_mut(id).context("Unknown camera")?;
        entry.state.lock().await.last_access = Instant::now();
        if name == "index.m3u8" && entry.task.as_ref().is_none_or(|t| t.is_finished()) {
            let source = entry.source.clone();
            let state = entry.state.clone();
            let worker_root = root.clone();
            entry.task = Some(tokio::spawn(async move {
                worker(id, source, worker_root, state, requests).await;
            }));
        }
        drop(registry);
        match tokio::fs::read(root.join(id.to_string()).join(name)).await {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
    pub async fn shutdown(&self) {
        let mut registry = self.0.lock().await;
        for entry in &mut registry.entries {
            if let Some(task) = entry.task.take() {
                task.abort();
                let _ = task.await;
            }
        }
        let _ = tokio::fs::remove_dir_all(&registry.root).await;
    }
}

async fn worker(
    id: usize,
    source: Source,
    root: PathBuf,
    state: Arc<Mutex<WorkerState>>,
    requests: Arc<Semaphore>,
) {
    let directory = root.join(id.to_string());
    let mut failures = 0u32;
    loop {
        if state.lock().await.last_access.elapsed() > Duration::from_secs(90) {
            return;
        }
        state.lock().await.message = "Получение адреса и подключение…".into();
        let outcome: Result<()> = async {
            // Only this worker owns this generated directory.
            if tokio::fs::try_exists(&directory).await? { tokio::fs::remove_dir_all(&directory).await?; }
            tokio::fs::create_dir(&directory).await?;
            let url = match &source {
                Source::Url(url) => url.clone(),
                Source::Account(camera_id) => {
                    let _permit = requests.acquire().await?;
                    crate::dsi::fresh_url(camera_id).await?
                }
            };
            let mut child = Command::new("ffmpeg")
                .env("OPENSSL_CONF", root.join("openssl.cnf"))
                .args(["-nostdin", "-hide_banner", "-loglevel", "error", "-rw_timeout", "15000000", "-tls_verify", "1", "-user_agent", "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 Chrome/150.0.0.0 Safari/537.36", "-headers", "Origin: https://video.dsi.ru\r\nReferer: https://video.dsi.ru/\r\n", "-i"])
                .arg(&url)
                .args(["-map", "0:v:0", "-map", "0:a:0?", "-c:v", "copy", "-c:a", "aac", "-f", "hls", "-hls_time", "2", "-hls_list_size", "6", "-hls_delete_threshold", "2", "-hls_start_number_source", "epoch", "-hls_flags", "delete_segments+temp_file+omit_endlist", "-hls_segment_filename", "segment_%d.ts", "index.m3u8"])
                .current_dir(&directory).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).kill_on_drop(true).spawn().context("Не удалось запустить FFmpeg")?;
            let started = Instant::now();
            loop {
                tokio::time::sleep(Duration::from_secs(2)).await;
                if state.lock().await.last_access.elapsed() > Duration::from_secs(90) {
                    let _ = child.kill().await;
                    return Ok(());
                }
                if child.try_wait()?.is_some() { anyhow::bail!("DSI прервал поток; получаем свежую ссылку"); }
                let metadata = tokio::fs::metadata(directory.join("index.m3u8")).await.ok();
                let age = metadata.as_ref().and_then(|m| m.modified().ok()).and_then(|t| t.elapsed().ok()).unwrap_or_else(|| started.elapsed());
                if age > Duration::from_secs(45) { let _ = child.kill().await; anyhow::bail!("Видео не поступает; обновляем адрес камеры"); }
                if metadata.is_some() {
                    state.lock().await.message = "Поток доступен".into();
                    if started.elapsed() > Duration::from_secs(30) { failures = 0; }
                }
            }
        }.await;
        match outcome {
            Ok(()) => return,
            Err(error) => {
                // Never return upstream responses, URLs or cookie values.
                let delay = 3u64.saturating_mul(1u64 << failures.min(4)).min(30);
                failures = failures.saturating_add(1);
                let message = format!("{error}. Повтор через {delay} с.");
                tracing::warn!(camera = id, message = %message, "Camera reconnect");
                state.lock().await.message = message;
                let _ = tokio::fs::remove_file(directory.join("index.m3u8")).await;
                tokio::time::sleep(Duration::from_secs(delay)).await;
            }
        }
    }
}

fn valid_file(name: &str) -> bool {
    name == "index.m3u8"
        || name
            .strip_prefix("segment_")
            .and_then(|s| s.strip_suffix(".ts"))
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}
pub async fn status(State(config): State<crate::Config>, Path(id): Path<usize>) -> Response {
    let registry = config.relays.0.lock().await;
    let Some(entry) = registry.entries.get(id) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    (
        [(header::CACHE_CONTROL, "no-store")],
        Json(serde_json::json!({"message": entry.state.lock().await.message})),
    )
        .into_response()
}
pub async fn serve(
    State(config): State<crate::Config>,
    Path((id, name)): Path<(usize, String)>,
) -> Response {
    if !valid_file(&name) {
        return StatusCode::NOT_FOUND.into_response();
    }
    match config.relays.file(id, &name).await {
        Ok(Some(bytes)) => (
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
            .into_response(),
        Ok(None) => (
            StatusCode::SERVICE_UNAVAILABLE,
            [
                (header::RETRY_AFTER, "3"),
                (header::CACHE_CONTROL, "no-store"),
            ],
            "Camera is connecting",
        )
            .into_response(),
        Err(_) => (StatusCode::BAD_GATEWAY, "Camera relay unavailable").into_response(),
    }
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
    #[tokio::test]
    async fn account_ids_have_stable_routes_without_exposing_sources() {
        let relays = Relays::new().await.unwrap();
        let one = relays.account_url("18830".into()).await;
        assert_eq!(one, relays.account_url("18830".into()).await);
        assert_ne!(one, relays.account_url("18838".into()).await);
        assert!(!one.contains("18830"));
        relays.shutdown().await;
    }
}
