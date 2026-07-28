mod config;
mod proxy;
mod sysproxy;

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use serde::Serialize;
use tauri::{Manager, State};
use tokio::sync::mpsc;

use config::Upstream;
use proxy::RunningProxy;

const MAX_LOG_LINES: usize = 500;
const DEFAULT_BIND: &str = "127.0.0.1:8899";

#[derive(Clone)]
struct Meta {
    local_addr: String,
    upstream: String,
    user: String,
    key_path: String,
}

#[derive(Default)]
pub struct AppState {
    running: tokio::sync::Mutex<Option<RunningProxy>>,
    meta: Mutex<Option<Meta>>,
    logs: Arc<Mutex<VecDeque<String>>>,
    /// Services whose proxy setting we changed, so the app can put exactly
    /// those back to direct connection when the proxy stops or it quits.
    sys_proxy_owned: Arc<Mutex<Vec<String>>>,
}

#[derive(Serialize)]
pub struct Status {
    running: bool,
    local_addr: Option<String>,
    upstream: Option<String>,
    upstream_user: Option<String>,
    key_path: Option<String>,
    active: u64,
    total: u64,
    failed: u64,
}

fn push_log(logs: &Arc<Mutex<VecDeque<String>>>, line: String) {
    if let Ok(mut q) = logs.lock() {
        if q.len() >= MAX_LOG_LINES {
            q.pop_front();
        }
        q.push_back(line);
    }
}

/// Read `.key` and report the upstream without exposing the password.
#[tauri::command]
fn load_config(path: Option<String>) -> Result<serde_json::Value, String> {
    let (up, p) = config::load_upstream(path.as_deref().map(std::path::Path::new))?;
    Ok(serde_json::json!({
        "upstream": up.addr(),
        "user": up.user,
        "key_path": p.display().to_string(),
        "has_password": !up.password.is_empty(),
    }))
}

/// Where a packaged app expects `.key` to live.
#[tauri::command]
fn config_location() -> String {
    config::config_dir().join(".key").display().to_string()
}

/// Persist credentials entered in the UI to the per-user config dir.
#[tauri::command]
fn save_config(
    ip: String,
    port: u16,
    user: String,
    password: String,
) -> Result<serde_json::Value, String> {
    let ip = ip.trim().to_string();
    if ip.is_empty() {
        return Err("upstream address must not be empty".into());
    }
    if port == 0 {
        return Err("invalid port".into());
    }
    let up = Upstream {
        ip,
        port,
        user: user.trim().to_string(),
        password,
    };
    let path = config::save_upstream(&up)?;
    Ok(serde_json::json!({
        "upstream": up.addr(),
        "user": up.user,
        "key_path": path.display().to_string(),
        "has_password": !up.password.is_empty(),
    }))
}

#[tauri::command]
async fn start_proxy(
    state: State<'_, AppState>,
    bind: Option<String>,
    key_path: Option<String>,
) -> Result<Status, String> {
    let mut guard = state.running.lock().await;
    if guard.is_some() {
        return Err("proxy is already running".into());
    }

    let (upstream, key_file) =
        config::load_upstream(key_path.as_deref().map(std::path::Path::new))?;
    let bind_str = bind.unwrap_or_else(|| DEFAULT_BIND.to_string());
    let bind_addr: SocketAddr = bind_str
        .parse()
        .map_err(|_| format!("invalid local listen address: {bind_str}"))?;

    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    let logs = Arc::clone(&state.logs);
    tokio::spawn(async move {
        while let Some(line) = rx.recv().await {
            push_log(&logs, line);
        }
    });

    let running = proxy::spawn(bind_addr, upstream.clone(), tx).await?;
    let meta = Meta {
        local_addr: running.addr.to_string(),
        upstream: upstream.addr(),
        user: upstream.user.clone(),
        key_path: key_file.display().to_string(),
    };
    push_log(
        &state.logs,
        format!(
            "local proxy started {} -> upstream {}",
            meta.local_addr, meta.upstream
        ),
    );
    *state.meta.lock().map_err(|e| e.to_string())? = Some(meta);
    *guard = Some(running);
    drop(guard);
    status(state).await
}

#[tauri::command]
async fn stop_proxy(state: State<'_, AppState>) -> Result<Status, String> {
    {
        let mut guard = state.running.lock().await;
        match guard.take() {
            Some(r) => {
                r.handle.abort();
                push_log(&state.logs, "local proxy stopped".into());
            }
            None => return Err("proxy is not running".into()),
        }
    }
    // The listener is gone; leaving a service pointed at it would break that
    // service's traffic, so roll back only the ones we changed.
    let owned = {
        let mut g = state.sys_proxy_owned.lock().map_err(|e| e.to_string())?;
        std::mem::take(&mut *g)
    };
    if !owned.is_empty() {
        match sysproxy::clear_proxy(&owned) {
            Ok(changed) => push_log(
                &state.logs,
                format!("system proxy also turned off: {}", changed.join(", ")),
            ),
            Err(e) => push_log(&state.logs, format!("failed to turn off system proxy: {e}")),
        }
    }
    status(state).await
}

#[tauri::command]
async fn status(state: State<'_, AppState>) -> Result<Status, String> {
    let guard = state.running.lock().await;
    let meta = state.meta.lock().map_err(|e| e.to_string())?.clone();
    let (active, total, failed) = match guard.as_ref() {
        Some(r) => (
            r.counters.active.load(Ordering::Relaxed),
            r.counters.total.load(Ordering::Relaxed),
            r.counters.failed.load(Ordering::Relaxed),
        ),
        None => (0, 0, 0),
    };
    Ok(Status {
        running: guard.is_some(),
        local_addr: meta.as_ref().map(|m| m.local_addr.clone()),
        upstream: meta.as_ref().map(|m| m.upstream.clone()),
        upstream_user: meta.as_ref().map(|m| m.user.clone()),
        key_path: meta.as_ref().map(|m| m.key_path.clone()),
        active,
        total,
        failed,
    })
}

#[tauri::command]
fn logs(state: State<'_, AppState>) -> Result<Vec<String>, String> {
    let q = state.logs.lock().map_err(|e| e.to_string())?;
    Ok(q.iter().cloned().collect())
}

#[tauri::command]
fn clear_logs(state: State<'_, AppState>) -> Result<(), String> {
    state.logs.lock().map_err(|e| e.to_string())?.clear();
    Ok(())
}

// Thin re-exports so the `sysproxy_e2e` example can drive the real code path.
pub fn sysproxy_get() -> Result<Vec<sysproxy::ServiceProxy>, String> {
    sysproxy::get_state()
}
pub fn sysproxy_set(host: &str, port: u16, services: &[String]) -> Result<Vec<String>, String> {
    sysproxy::set_proxy(host, port, services)
}
pub fn sysproxy_clear(services: &[String]) -> Result<Vec<String>, String> {
    sysproxy::clear_proxy(services)
}
pub fn sysproxy_services() -> Result<Vec<String>, String> {
    sysproxy::active_services()
}

/// Current system proxy settings per active network service.
#[tauri::command]
fn system_proxy_state() -> Result<Vec<sysproxy::ServiceProxy>, String> {
    sysproxy::get_state()
}

/// Route system HTTP/HTTPS traffic through the local listener.
/// Refuses when the local proxy is down, since that would break all traffic.
#[tauri::command]
async fn set_system_proxy(
    state: State<'_, AppState>,
    services: Vec<String>,
) -> Result<Vec<String>, String> {
    let listen = {
        let guard = state.running.lock().await;
        match guard.as_ref() {
            Some(r) => r.addr,
            None => return Err("start the local proxy before setting the system proxy".into()),
        }
    };
    let changed = sysproxy::set_proxy(&listen.ip().to_string(), listen.port(), &services)?;
    {
        let mut owned = state.sys_proxy_owned.lock().map_err(|e| e.to_string())?;
        for s in &changed {
            if !owned.contains(s) {
                owned.push(s.clone());
            }
        }
    }
    push_log(
        &state.logs,
        format!(
            "system proxy now points at {listen}: {}",
            changed.join(", ")
        ),
    );
    Ok(changed)
}

/// Restore direct connection.
#[tauri::command]
async fn clear_system_proxy(
    state: State<'_, AppState>,
    services: Option<Vec<String>>,
) -> Result<Vec<String>, String> {
    // No explicit list means "undo whatever we turned on".
    let targets = match services {
        Some(s) if !s.is_empty() => s,
        _ => state
            .sys_proxy_owned
            .lock()
            .map_err(|e| e.to_string())?
            .clone(),
    };
    if targets.is_empty() {
        return Err("this app has not set any system proxy".into());
    }
    let changed = sysproxy::clear_proxy(&targets)?;
    {
        let mut owned = state.sys_proxy_owned.lock().map_err(|e| e.to_string())?;
        owned.retain(|s| !changed.contains(s));
    }
    push_log(
        &state.logs,
        format!("system proxy turned off: {}", changed.join(", ")),
    );
    Ok(changed)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .setup(|app| {
            app.manage(AppState::default());
            Ok(())
        })
        .on_window_event(|window, event| {
            // Leaving the system pointed at a dead listener would take the
            // machine offline, so undo our own change on the way out.
            if matches!(event, tauri::WindowEvent::Destroyed) {
                let state = window.state::<AppState>();
                let owned = match state.sys_proxy_owned.lock() {
                    Ok(mut g) => std::mem::take(&mut *g),
                    Err(_) => Vec::new(),
                };
                if !owned.is_empty() {
                    let _ = sysproxy::clear_proxy(&owned);
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            load_config,
            save_config,
            config_location,
            start_proxy,
            stop_proxy,
            status,
            logs,
            clear_logs,
            system_proxy_state,
            set_system_proxy,
            clear_system_proxy
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

/// Headless entry point: `localproxy --cli [bind] [key_path]`, useful for testing.
pub async fn run_cli(bind: &str, key_path: Option<&str>) -> Result<(), String> {
    let (upstream, key_file) = config::load_upstream(key_path.map(std::path::Path::new))?;
    let addr: SocketAddr = bind
        .parse()
        .map_err(|_| format!("invalid address: {bind}"))?;
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    tokio::spawn(async move {
        while let Some(l) = rx.recv().await {
            println!("{l}");
        }
    });
    let running = proxy::spawn(addr, upstream.clone(), tx).await?;
    println!(
        "listening on {} -> upstream {} (key: {})",
        running.addr,
        upstream.addr(),
        key_file.display()
    );
    running.handle.await.map_err(|e| e.to_string())
}
