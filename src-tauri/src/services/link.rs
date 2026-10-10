//! These five dashboard URLs open only from Rust; the webview has no opener
//! permission. See `src-tauri/capabilities/default.json`.

pub fn open_claude_dashboard() -> Result<(), String> {
    tauri_plugin_opener::open_url("https://console.anthropic.com/settings/usage", None::<&str>)
        .map_err(|e| e.to_string())
}

pub fn open_codex_dashboard() -> Result<(), String> {
    tauri_plugin_opener::open_url("https://chatgpt.com", None::<&str>).map_err(|e| e.to_string())
}

pub fn open_cursor_dashboard() -> Result<(), String> {
    tauri_plugin_opener::open_url("https://www.cursor.com/settings", None::<&str>)
        .map_err(|e| e.to_string())
}

pub fn open_antigravity_dashboard() -> Result<(), String> {
    tauri_plugin_opener::open_url("https://antigravity.google.com", None::<&str>)
        .map_err(|e| e.to_string())
}

pub fn open_grok_dashboard() -> Result<(), String> {
    tauri_plugin_opener::open_url("https://grok.com/?_s=usage", None::<&str>)
        .map_err(|e| e.to_string())
}

fn is_service_status_url(value: &str) -> bool {
    let Ok(url) = tauri::Url::parse(value) else {
        return false;
    };
    url.scheme() == "https"
        && url.username().is_empty()
        && url.password().is_none()
        && url.port_or_known_default() == Some(443)
        && matches!(
            url.host_str(),
            Some("status.claude.com" | "status.openai.com" | "stspg.io")
        )
}

pub fn open_service_status_url(url: &str) -> Result<(), String> {
    if !is_service_status_url(url) {
        return Err("Invalid service-status URL.".into());
    }
    tauri_plugin_opener::open_url(url, None::<&str>).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::is_service_status_url;

    #[test]
    fn service_status_links_allow_only_trusted_https_origins() {
        assert!(is_service_status_url(
            "https://status.claude.com/incidents/example"
        ));
        assert!(is_service_status_url(
            "https://status.openai.com/incidents/example"
        ));
        assert!(is_service_status_url("https://stspg.io/sjkw7njwf0mt"));
        assert!(!is_service_status_url(
            "https://status.openai.com.evil.example/"
        ));
        assert!(!is_service_status_url(
            "http://status.claude.com/incidents/example"
        ));
        assert!(!is_service_status_url("https://stspg.io.evil.com/x"));
        assert!(!is_service_status_url(
            "https://evil.com/?u=https://status.claude.com/"
        ));
        assert!(!is_service_status_url("https://user@status.claude.com/"));
        assert!(!is_service_status_url("https://status.claude.com:8443/"));
    }
}
