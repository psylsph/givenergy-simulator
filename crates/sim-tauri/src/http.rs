//! Optional HTTP server that mirrors the Tauri GUI on a plain browser port.
//!
//! The GivEnergy Plant Simulator GUI normally runs inside a Tauri webview and
//! talks to the Rust backend via Tauri's `invoke()` IPC. This module exposes
//! the **same** frontend over plain HTTP (default port **8001**) so the GUI is
//! reachable from any browser — not just the desktop app.
//!
//! It does three jobs:
//!
//! 1. **Serves the frontend** — `index.html` is the single self-contained file
//!    at `ui/index.html` (no external assets). It is embedded at build time via
//!    `include_str!`, with runtime overrides for live development.
//! 2. **Bridges IPC** — `POST /api/invoke/<command>` dispatches to the exact
//!    same `#[tauri::command]` functions the webview uses (we obtain a real
//!    `State` from `AppHandle` via the `Manager` trait, so no logic is
//!    duplicated).
//! 3. **Streams events** — `GET /api/events` is an SSE feed. We register
//!    `AppHandle` listeners for `state_changed` / `scenario_completed` /
//!    `recording_saved` and forward every emit to all connected browsers.
//!
//! The frontend detects "am I in Tauri?" via `window.__TAURI_INTERNALS__` and,
//! when absent, installs a tiny shim that backs `invoke`/`listen`/`dialog.save`
//! with `fetch()` against this server. See the `<script>` block injected at the
//! top of `ui/index.html`.

use crate::app_state::AppState;
use crate::commands;
use serde_json::Value;
use std::io;
use std::time::Duration;
use tauri::{AppHandle, Manager, State};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast;

/// Embedded copy of the frontend (compiled in at build time).
const INDEX_HTML: &str = include_str!("../../../ui/index.html");

/// Default port for the browser-accessible GUI.
pub const DEFAULT_WEB_PORT: u16 = 8001;

/// Resolve the web UI port: `$GIVSIM_WEB_PORT` overrides the default.
pub fn web_port() -> u16 {
    std::env::var("GIVSIM_WEB_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|&p| p > 0)
        .unwrap_or(DEFAULT_WEB_PORT)
}

/// Return the frontend HTML.
///
/// Resolution order (first hit wins):
/// 1. `$GIVSIM_UI_INDEX` — explicit path override.
/// 2. `ui/index.html` relative to the current working directory — picks up
///    live edits during development without a recompile.
/// 3. The build-time embedded copy — always available, used in production /
///    portable builds.
fn index_html() -> String {
    if let Ok(path) = std::env::var("GIVSIM_UI_INDEX") {
        if let Ok(s) = std::fs::read_to_string(&path) {
            return s;
        }
    }
    if let Ok(s) = std::fs::read_to_string("ui/index.html") {
        return s;
    }
    INDEX_HTML.to_string()
}

/// Run the HTTP server. Blocks forever; spawn on the Tauri async runtime.
pub async fn run_http_server(app: AppHandle, events: broadcast::Sender<String>) -> io::Result<()> {
    let port = web_port();
    let addr: std::net::SocketAddr = format!("0.0.0.0:{port}").parse().unwrap();
    let listener = TcpListener::bind(addr).await?;
    tracing::info!("Web UI (HTTP) server listening on http://0.0.0.0:{port}");
    loop {
        let (mut stream, peer) = listener.accept().await?;
        let app = app.clone();
        let events = events.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_connection(&mut stream, app, events).await {
                let benign = matches!(
                    e.kind(),
                    io::ErrorKind::ConnectionAborted
                        | io::ErrorKind::BrokenPipe
                        | io::ErrorKind::UnexpectedEof
                        | io::ErrorKind::ConnectionReset
                );
                if !benign {
                    tracing::debug!("HTTP connection from {peer} ended with error: {e}");
                }
            }
        });
    }
}

async fn handle_connection(
    stream: &mut TcpStream,
    app: AppHandle,
    events: broadcast::Sender<String>,
) -> io::Result<()> {
    let (method, path, _headers, body) = match read_request(stream).await? {
        Some(r) => r,
        None => return Ok(()), // client closed before sending anything
    };

    let clean = path.split('?').next().unwrap_or(&path);
    match (method.as_str(), clean) {
        ("GET", "/") | ("GET", "/index.html") => {
            let html = index_html();
            respond(stream, 200, "text/html; charset=utf-8", html.as_bytes()).await
        }

        ("POST", p) if p.starts_with("/api/invoke/") => {
            let cmd = &p["/api/invoke/".len()..];
            let args: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            match dispatch(&app, cmd, args).await {
                Ok(v) => respond(stream, 200, "application/json", v.to_string().as_bytes()).await,
                Err((code, msg)) => {
                    let payload = serde_json::json!({ "error": msg }).to_string();
                    respond(stream, code, "application/json", payload.as_bytes()).await
                }
            }
        }

        ("GET", "/api/events") => sse_loop(stream, events).await,

        ("GET", "/api/export/recordings") => {
            let format = query_param(&path, "format").unwrap_or_else(|| "csv".to_string());
            export_recordings(stream, &app, &format).await
        }
        ("GET", "/api/export/config") => export_config(stream, &app).await,

        ("POST", "/api/scenario") => load_scenario_body(stream, &app, &body).await,

        ("OPTIONS", _) => respond(stream, 204, "text/plain", &[]).await,

        _ => respond(stream, 404, "text/plain", b"Not Found").await,
    }
}

// ---------------------------------------------------------------------------
// Request parsing
// ---------------------------------------------------------------------------

async fn read_request(
    stream: &mut TcpStream,
) -> io::Result<Option<(String, String, Vec<(String, String)>, Vec<u8>)>> {
    let mut buf: Vec<u8> = Vec::with_capacity(2048);
    let mut tmp = [0u8; 4096];
    let header_end = loop {
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            return if buf.is_empty() {
                Ok(None)
            } else {
                Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "incomplete request header",
                ))
            };
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(idx) = find_subsequence(&buf, b"\r\n\r\n") {
            break idx;
        }
        if buf.len() > 1 << 20 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "request header too large",
            ));
        }
    };

    let header_str = std::str::from_utf8(&buf[..header_end])
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "non-utf8 request header"))?;
    let mut lines = header_str.split("\r\n");
    let req_line = lines.next().unwrap_or("");
    let mut parts = req_line.splitn(3, ' ');
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("/").to_string();

    let mut headers: Vec<(String, String)> = Vec::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
        }
    }

    let body_start = header_end + 4;
    let mut body: Vec<u8> = if buf.len() > body_start {
        buf[body_start..].to_vec()
    } else {
        Vec::new()
    };
    let content_length: usize = header_value(&headers, "content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    while body.len() < content_length {
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "body truncated",
            ));
        }
        body.extend_from_slice(&tmp[..n]);
    }
    body.truncate(content_length);

    Ok(Some((method, path, headers, body)))
}

// ---------------------------------------------------------------------------
// Dispatch to the real #[tauri::command] functions
// ---------------------------------------------------------------------------

/// Macro: extract a nested `params` object and deserialize into a struct.
///
/// The target type is inferred from the call site (`let params = param!(args);`
/// passed straight into the command), so no type annotation is needed.
macro_rules! param {
    ($args:expr) => {
        match serde_json::from_value(
            $args
                .get("params")
                .cloned()
                .unwrap_or(serde_json::Value::Null),
        ) {
            Ok(v) => v,
            Err(e) => return Err((400u16, format!("invalid params: {e}"))),
        }
    };
}

/// Macro: extract a single flat field and deserialize.
macro_rules! flat {
    ($args:expr, $field:expr) => {
        match serde_json::from_value(
            $args
                .get($field)
                .cloned()
                .unwrap_or(serde_json::Value::Null),
        ) {
            Ok(v) => v,
            Err(e) => return Err((400u16, format!("invalid `{}`: {e}", $field))),
        }
    };
}

/// Wrap a command's `Result<T, String>` as the HTTP-level result.
fn ok<T: serde::Serialize>(r: Result<T, String>) -> Result<Value, (u16, String)> {
    r.map(|v| serde_json::to_value(&v).unwrap_or(Value::Null))
        .map_err(|e| (500, e))
}

#[allow(clippy::too_many_lines)]
async fn dispatch(app: &AppHandle, cmd: &str, args: Value) -> Result<Value, (u16, String)> {
    use commands as c;
    let state: State<'_, AppState> = app.state::<AppState>();

    match cmd {
        // --- Plant lifecycle -------------------------------------------------
        "create_plant" => {
            let params = param!(args);
            ok(c::create_plant(app.clone(), state, params).await)
        }
        "load_scenario" => {
            let params = param!(args);
            ok(c::load_scenario(state, params).await)
        }
        "start_simulation" => {
            let params = param!(args);
            ok(c::start_simulation(app.clone(), state, params).await)
        }
        "pause_simulation" => ok(c::pause_simulation(state).await),

        // --- Faults ----------------------------------------------------------
        "inject_fault" => {
            let params = param!(args);
            ok(c::inject_fault(app.clone(), state, params).await)
        }
        "clear_fault" => {
            let params = param!(args);
            ok(c::clear_fault(state, params).await)
        }

        // --- Inverter / solar / load controls --------------------------------
        "set_mode" => {
            let params = param!(args);
            ok(c::set_mode(state, params).await)
        }
        "set_weather" => {
            let params = param!(args);
            ok(c::set_weather(state, params).await)
        }
        "set_solar_override" => {
            let params = param!(args);
            ok(c::set_solar_override(app.clone(), state, params).await)
        }
        "set_load_override" => {
            let params = param!(args);
            ok(c::set_load_override(app.clone(), state, params).await)
        }
        "set_ct_meter" => {
            let params = param!(args);
            ok(c::set_ct_meter(app.clone(), state, params).await)
        }
        "set_battery_soc" => {
            let params = param!(args);
            ok(c::set_battery_soc(app.clone(), state, params).await)
        }
        "set_battery_soh" => {
            let params = param!(args);
            ok(c::set_battery_soh(app.clone(), state, params).await)
        }
        "start_calibration" => {
            let params = param!(args);
            ok(c::start_calibration(app.clone(), state, params).await)
        }
        "cancel_calibration" => ok(c::cancel_calibration(app.clone(), state).await),
        "set_tick_interval" => {
            let params = param!(args);
            ok(c::set_tick_interval(app.clone(), state, params).await)
        }

        // --- State / persistence ---------------------------------------------
        "get_current_state" => ok(c::get_current_state(state).await),
        "save_plant" => ok(c::save_plant(app.clone(), state).await),
        "has_saved_plant" => ok(c::has_saved_plant(app.clone()).await),
        "load_plant" => ok(c::load_plant(app.clone(), state).await),

        // --- Exports (browser uses dedicated /api/export/* endpoints instead) -
        "export_recording" => {
            let params = param!(args);
            ok(c::export_recording(app.clone(), state, params).await)
        }
        "export_config" => {
            let path = flat!(args, "path");
            ok(c::export_config(app.clone(), state, path).await)
        }

        // --- EVC -------------------------------------------------------------
        "set_evc_enabled" => {
            let enabled = flat!(args, "enabled");
            ok(c::set_evc_enabled(state, enabled).await)
        }
        "set_evc_charge_control" => {
            let mode = flat!(args, "mode");
            ok(c::set_evc_charge_control(state, mode).await)
        }
        "set_evc_charge_current" => {
            let deci_amps = flat!(args, "deciAmps");
            ok(c::set_evc_charge_current(state, deci_amps).await)
        }
        "set_evc_session_energy" => {
            let kwh = flat!(args, "kwh");
            ok(c::set_evc_session_energy(state, kwh).await)
        }
        "set_evc_cable_status" => {
            let status = flat!(args, "status");
            ok(c::set_evc_cable_status(state, status).await)
        }
        "get_evc_state" => ok(c::get_evc_state(state).await),
        "set_evc_port" => {
            let port = flat!(args, "port");
            ok(c::set_evc_port(state, port).await)
        }
        "get_evc_port" => ok(c::get_evc_port(state).await),

        // --- Firmware --------------------------------------------------------
        "set_dsp_firmware" => {
            let version = flat!(args, "version");
            ok(c::set_dsp_firmware(state, version).await)
        }
        "set_arm_firmware" => {
            let version = flat!(args, "version");
            ok(c::set_arm_firmware(state, version).await)
        }
        "set_inverter_temperature" => {
            let celsius = flat!(args, "celsius");
            ok(c::set_inverter_temperature(app.clone(), state, celsius).await)
        }

        other => Err((404, format!("unknown command: {other}"))),
    }
}

// ---------------------------------------------------------------------------
// Browser-friendly endpoints (downloads + scenario upload)
// ---------------------------------------------------------------------------

async fn export_recordings(
    stream: &mut TcpStream,
    app: &AppHandle,
    format: &str,
) -> io::Result<()> {
    let state: State<'_, AppState> = app.state::<AppState>();
    let ext = match format {
        "jsonl" => "jsonl",
        "json" => "json",
        _ => "csv",
    };
    let path = temp_path(ext);
    let result = c_export_recording(app, state, &path, format).await;
    let bytes = std::fs::read(&path).unwrap_or_default();
    let _ = std::fs::remove_file(&path);
    match result {
        Ok(()) => {
            respond_attachment(
                stream,
                "text/csv; charset=utf-8",
                &format!("recording.{ext}"),
                &bytes,
            )
            .await
        }
        Err(e) => {
            let payload = serde_json::json!({ "error": e }).to_string();
            respond(stream, 500, "application/json", payload.as_bytes()).await
        }
    }
}

async fn export_config(stream: &mut TcpStream, app: &AppHandle) -> io::Result<()> {
    let state: State<'_, AppState> = app.state::<AppState>();
    let path = temp_path("json");
    let result =
        commands::export_config(app.clone(), state, path.to_string_lossy().into_owned()).await;
    let bytes = std::fs::read(&path).unwrap_or_default();
    let _ = std::fs::remove_file(&path);
    match result {
        Ok(()) => respond_attachment(stream, "application/json", "plant-config.json", &bytes).await,
        Err(e) => {
            let payload = serde_json::json!({ "error": e }).to_string();
            respond(stream, 500, "application/json", payload.as_bytes()).await
        }
    }
}

async fn load_scenario_body(
    stream: &mut TcpStream,
    app: &AppHandle,
    body: &[u8],
) -> io::Result<()> {
    let state: State<'_, AppState> = app.state::<AppState>();
    let yaml = String::from_utf8_lossy(body);
    let path = temp_path("yaml");
    if let Err(e) = std::fs::write(&path, yaml.as_bytes()) {
        let payload = serde_json::json!({ "error": e.to_string() }).to_string();
        return respond(stream, 500, "application/json", payload.as_bytes()).await;
    }
    let result = commands::load_scenario(
        state,
        commands::LoadScenarioParams {
            path: path.to_string_lossy().into_owned(),
        },
    )
    .await;
    let _ = std::fs::remove_file(&path);
    match result {
        Ok(info) => {
            let body = serde_json::to_vec(&info).unwrap_or_default();
            respond(stream, 200, "application/json", &body).await
        }
        Err(e) => {
            let payload = serde_json::json!({ "error": e }).to_string();
            respond(stream, 500, "application/json", payload.as_bytes()).await
        }
    }
}

/// Helper that calls `commands::export_recording` with an owned temp path.
async fn c_export_recording(
    app: &AppHandle,
    state: State<'_, AppState>,
    path: &std::path::Path,
    format: &str,
) -> Result<(), String> {
    commands::export_recording(
        app.clone(),
        state,
        commands::ExportRecordingParams {
            path: path.to_string_lossy().into_owned(),
            format: format.to_string(),
        },
    )
    .await
    .map(|_| ())
}

// ---------------------------------------------------------------------------
// Server-Sent Events
// ---------------------------------------------------------------------------

async fn sse_loop(stream: &mut TcpStream, events: broadcast::Sender<String>) -> io::Result<()> {
    let header = concat!(
        "HTTP/1.1 200 OK\r\n",
        "Content-Type: text/event-stream\r\n",
        "Cache-Control: no-store\r\n",
        "Connection: keep-alive\r\n",
        "Access-Control-Allow-Origin: *\r\n",
        "\r\n",
    );
    stream.write_all(header.as_bytes()).await?;
    stream.flush().await?;

    let mut rx = events.subscribe();
    let mut keepalive = tokio::time::interval(Duration::from_secs(15));
    keepalive.tick().await; // discard the immediate first tick
    loop {
        tokio::select! {
            res = rx.recv() => match res {
                Ok(msg) => {
                    let frame = format!("data: {msg}\n\n");
                    if stream.write_all(frame.as_bytes()).await.is_err() { return Ok(()); }
                    if stream.flush().await.is_err() { return Ok(()); }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return Ok(()),
            },
            _ = keepalive.tick() => {
                if stream.write_all(b": keepalive\n\n").await.is_err() { return Ok(()); }
                if stream.flush().await.is_err() { return Ok(()); }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Response writers + small helpers
// ---------------------------------------------------------------------------

async fn respond(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
) -> io::Result<()> {
    let reason = status_reason(status);
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {len}\r\n\
         Cache-Control: no-store\r\n\
         Access-Control-Allow-Origin: *\r\n\
         Access-Control-Allow-Methods: GET, POST, OPTIONS\r\n\
         Access-Control-Allow-Headers: Content-Type\r\n\
         Connection: close\r\n\
         \r\n",
        len = body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body).await?;
    stream.flush().await?;
    Ok(())
}

async fn respond_attachment(
    stream: &mut TcpStream,
    content_type: &str,
    filename: &str,
    body: &[u8],
) -> io::Result<()> {
    let head = format!(
        "HTTP/1.1 200 OK\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {len}\r\n\
         Content-Disposition: attachment; filename=\"{filename}\"\r\n\
         Cache-Control: no-store\r\n\
         Connection: close\r\n\
         \r\n",
        len = body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body).await?;
    stream.flush().await?;
    Ok(())
}

fn status_reason(code: u16) -> &'static str {
    match code {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        404 => "Not Found",
        500 => "Internal Server Error",
        _ => "OK",
    }
}

fn temp_path(ext: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id();
    std::env::temp_dir().join(format!("givsim-{pid}-{nanos}.{ext}"))
}

fn header_value<'a>(headers: &'a [(String, String)], key: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

fn query_param(path: &str, key: &str) -> Option<String> {
    let q = path.split_once('?').map(|(_, q)| q)?;
    for pair in q.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            if k.eq_ignore_ascii_case(key) {
                return Some(percent_decode(v));
            }
        }
    }
    None
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) =
                u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or(""), 16)
            {
                out.push(b);
                i += 3;
                continue;
            }
        }
        if bytes[i] == b'+' {
            out.push(b' ');
        } else {
            out.push(bytes[i]);
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_header_terminator() {
        assert_eq!(
            find_subsequence(b"GET / HTTP/1.1\r\n\r\n", b"\r\n\r\n"),
            Some(14)
        );
        assert_eq!(find_subsequence(b"no terminator here", b"\r\n\r\n"), None);
    }

    #[test]
    fn parses_header_value_case_insensitively() {
        let h = vec![("content-length".into(), "42".into())];
        assert_eq!(header_value(&h, "content-length"), Some("42"));
        assert_eq!(header_value(&h, "missing"), None);
    }

    #[test]
    fn extracts_query_param() {
        assert_eq!(
            query_param("/api/export/recordings?format=csv", "format"),
            Some("csv".into())
        );
        assert_eq!(query_param("/api/export/recordings", "format"), None);
    }

    #[test]
    fn percent_decodes_value() {
        assert_eq!(percent_decode("foo%20bar"), "foo bar");
        assert_eq!(percent_decode("a+b"), "a b");
        assert_eq!(percent_decode("plain"), "plain");
    }

    #[test]
    fn web_port_defaults_to_8001() {
        // No env override set in normal test runs.
        std::env::remove_var("GIVSIM_WEB_PORT");
        assert_eq!(web_port(), DEFAULT_WEB_PORT);
        std::env::set_var("GIVSIM_WEB_PORT", "9999");
        assert_eq!(web_port(), 9999);
        std::env::set_var("GIVSIM_WEB_PORT", "not-a-number");
        assert_eq!(web_port(), DEFAULT_WEB_PORT);
        std::env::remove_var("GIVSIM_WEB_PORT");
    }
}
