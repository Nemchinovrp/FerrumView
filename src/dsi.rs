use anyhow::{Context, Result, bail, ensure};
use std::process::Stdio;
use tokio::{io::AsyncWriteExt, process::Command};

pub fn camera_id(url: &str) -> Result<String> {
    let url = url.trim();
    if !url.is_empty() && url.bytes().all(|b| b.is_ascii_digit()) {
        return Ok(url.to_owned());
    }
    let uri: axum::http::Uri = url.parse().context("Некорректная ссылка страницы DSI")?;
    ensure!(
        uri.scheme_str() == Some("https")
            && uri.host() == Some("video.dsi.ru")
            && uri.port_u16().is_none_or(|p| p == 443),
        "Нужна HTTPS-ссылка на video.dsi.ru"
    );
    let parts: Vec<_> = uri.path().split('/').collect();
    ensure!(
        parts.len() == 5
            && parts[1] == "account"
            && parts[2] == "camera"
            && matches!(parts[4], "view.html" | "url.html")
            && !parts[3].is_empty()
            && parts[3].bytes().all(|b| b.is_ascii_digit()),
        "Нужна ссылка /account/camera/ID/url.html или view.html"
    );
    Ok(parts[3].to_owned())
}

pub fn parse_cameras(contents: &str) -> Result<Vec<(String, String)>> {
    contents
        .trim_start_matches('\u{feff}')
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty() && !line.trim().starts_with('#'))
        .map(|(index, line)| {
            let (name, value) = line.split_once('|').with_context(|| {
                format!(
                    "dsi-cameras.txt, строка {}: ожидается Название | ID",
                    index + 1
                )
            })?;
            ensure!(
                !name.trim().is_empty(),
                "dsi-cameras.txt, строка {}: укажите название",
                index + 1
            );
            let id = camera_id(value).with_context(|| {
                format!(
                    "dsi-cameras.txt, строка {}: укажите номер камеры или ссылку её страницы DSI",
                    index + 1
                )
            })?;
            Ok((name.trim().to_owned(), id))
        })
        .collect()
}

fn cookie(contents: &str) -> Result<String> {
    let value = contents
        .lines()
        .map(str::trim)
        .find_map(|line| line.strip_prefix("DSI_COOKIE="))
        .context("Добавьте DSI_COOKIE в .dsi.env")?
        .trim();
    ensure!(
        value != "'" && value != "\"",
        "DSI_COOKIE в .dsi.env некорректен"
    );
    let value = if (value.starts_with('\'') && value.ends_with('\''))
        || (value.starts_with('"') && value.ends_with('"'))
    {
        &value[1..value.len().saturating_sub(1)]
    } else {
        value
    };
    ensure!(
        !value.is_empty() && !value.chars().any(char::is_control),
        "DSI_COOKIE в .dsi.env пуст или некорректен"
    );
    Ok(value.to_owned())
}

fn hls_url(value: &str) -> Result<String> {
    let mut uri: axum::http::Uri = value
        .parse()
        .context("DSI вернул некорректный адрес потока")?;
    ensure!(
        uri.scheme_str() == Some("https") && crate::relay::is_dsi(value) && uri.query().is_none(),
        "DSI вернул неожиданный адрес потока"
    );
    let path = uri.path();
    let path = if let Some(tail) = path.strip_prefix("/rtsp/") {
        format!("/hls/{}", tail.trim_end_matches('/'))
    } else if path.starts_with("/hls/") {
        path.trim_end_matches('/').to_owned()
    } else {
        bail!("DSI вернул неизвестный формат потока")
    };
    let path = if path.ends_with("/playlist.m3u8") {
        path
    } else {
        format!("{path}/playlist.m3u8")
    };
    let mut parts = uri.into_parts();
    parts.path_and_query = Some(path.parse()?);
    uri = axum::http::Uri::from_parts(parts)?;
    Ok(uri.to_string())
}

pub async fn fresh_url(id: &str) -> Result<String> {
    let contents = tokio::fs::read_to_string(".dsi.env")
        .await
        .context("Не удалось прочитать .dsi.env; сохраните cookie сессии DSI")?;
    let cookie = cookie(&contents)?;
    // Cookie goes through stdin, never shell arguments, logs, or browser JSON.
    let escaped = cookie.replace('\\', "\\\\").replace('"', "\\\"");
    let mut child = Command::new("curl")
        .args(["--silent", "--show-error", "--fail", "--connect-timeout", "8", "--max-time", "20", "--max-filesize", "1048576", "--config", "-", "--header", "Accept: application/json, text/javascript, */*; q=0.01", "--header", "X-Requested-With: XMLHttpRequest", "--user-agent", "Mozilla/5.0", "--referer"])
        .arg(format!("https://video.dsi.ru/account/camera/{id}/view.html"))
        .arg(format!("https://video.dsi.ru/account/camera/{id}/url.html?time=&timeZoneOffset=10800&format=hls&_={}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_millis()))
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).kill_on_drop(true).spawn().context("Не удалось запустить curl для API DSI")?;
    let mut stdin = child
        .stdin
        .take()
        .context("Не удалось передать cookie API-клиенту")?;
    stdin
        .write_all(format!("cookie = \"{escaped}\"\n").as_bytes())
        .await?;
    drop(stdin);
    let output = child.wait_with_output().await?;
    ensure!(
        output.status.success(),
        "API DSI недоступен; проверьте соединение и авторизацию"
    );
    let data: serde_json::Value = serde_json::from_slice(&output.stdout).map_err(|_| {
        anyhow::anyhow!(
            "Сессия DSI недействительна: войдите на video.dsi.ru и обновите DSI_COOKIE в .dsi.env"
        )
    })?;
    ensure!(
        data["Error"] == false && data["Status"] == true,
        "DSI не выдал ссылку: проверьте доступ к камере и обновите DSI_COOKIE в .dsi.env"
    );
    hls_url(data["URL"].as_str().context("DSI не вернул адрес видео")?)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn camera_list_accepts_ids_and_preserves_repeated_names() {
        let cameras =
            parse_cameras("\u{feff}# cameras\n\n Подъезд 132 | 18830\r\nПодъезд 132 | 18838\n")
                .unwrap();
        assert_eq!(
            cameras,
            vec![
                ("Подъезд 132".into(), "18830".into()),
                ("Подъезд 132".into(), "18838".into())
            ]
        );
    }
    #[test]
    fn invalid_camera_ids_report_the_line() {
        for value in ["", "18830?x=1", "-1", "18 830", "１８８３０"] {
            let error = parse_cameras(&format!("# comment\nПодъезд | {value}")).unwrap_err();
            assert!(error.to_string().contains("строка 2"));
        }
        assert!(parse_cameras(" | 18830").is_err());
    }
    #[test]
    fn only_account_camera_urls_are_accepted() {
        assert_eq!(
            camera_id("https://video.dsi.ru/account/camera/18830/url.html?format=hls").unwrap(),
            "18830"
        );
        assert!(camera_id("https://evil.test/account/camera/18830/url.html").is_err());
        assert!(camera_id("https://video.dsi.ru/hls/123/playlist.m3u8").is_err());
    }
    #[test]
    fn cookie_quotes_and_secret_errors() {
        assert_eq!(
            cookie("# test\nDSI_COOKIE='a=1; b=2'\n").unwrap(),
            "a=1; b=2"
        );
        assert!(cookie("DSI_COOKIE=''").is_err());
        assert!(cookie("DSI_COOKIE='").is_err());
        assert!(
            cookie("DSI_COOKIE=secret\tvalue")
                .unwrap_err()
                .to_string()
                .find("secret")
                .is_none()
        );
    }
    #[test]
    fn api_url_is_converted_without_damaging_token() {
        assert_eq!(
            hls_url("https://video9.dsi.ru:8091/rtsp/123/a.b,c-d/").unwrap(),
            "https://video9.dsi.ru:8091/hls/123/a.b,c-d/playlist.m3u8"
        );
        assert!(hls_url("https://dsi.ru.evil.test/rtsp/123/token").is_err());
        assert!(hls_url("http://video9.dsi.ru/rtsp/123/token").is_err());
    }
}
