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
    /// Services whose proxy setting we changed, with the bypass list each one
    /// had before, so the app can restore exactly those when it stops or quits.
    sys_proxy_owned: Arc<Mutex<Vec<sysproxy::Applied>>>,
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

/// Hosts and domains that should keep a direct connection while the system
/// proxy is on. Falls back to a sensible local-network default.
#[tauri::command]
fn load_bypass_list() -> Vec<String> {
    config::load_bypass()
}

/// Persist the bypass list and report where it landed.
#[tauri::command]
fn save_bypass_list(
    state: State<'_, AppState>,
    entries: Vec<String>,
) -> Result<Vec<String>, String> {
    let (clean, path) = config::save_bypass(&entries)?;
    push_log(
        &state.logs,
        format!("bypass list saved to {}", path.display()),
    );
    Ok(clean)
}

// Thin re-exports so the `sysproxy_e2e` example can drive the real code path.
pub fn sysproxy_get() -> Result<Vec<sysproxy::ServiceProxy>, String> {
    sysproxy::get_state()
}
pub fn sysproxy_set(
    host: &str,
    port: u16,
    services: &[String],
    bypass: &[String],
) -> Result<Vec<String>, String> {
    sysproxy::set_proxy(host, port, services, bypass)
        .map(|a| a.into_iter().map(|x| x.service).collect())
}
pub fn sysproxy_clear(services: &[String]) -> Result<Vec<String>, String> {
    // The e2e helper has no record of prior state, so leave bypass lists alone.
    let targets: Vec<sysproxy::Applied> = services
        .iter()
        .cloned()
        .map(sysproxy::Applied::bypass_unknown)
        .collect();
    sysproxy::clear_proxy(&targets)
}
pub fn sysproxy_default_bypass() -> Vec<String> {
    config::load_bypass()
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
    bypass: Option<Vec<String>>,
) -> Result<Vec<String>, String> {
    let listen = {
        let guard = state.running.lock().await;
        match guard.as_ref() {
            Some(r) => r.addr,
            None => return Err("start the local proxy before setting the system proxy".into()),
        }
    };
    // An explicit list (even an empty one) wins; otherwise use what's on disk.
    let bypass = match bypass {
        Some(list) => config::sanitize_bypass(&list)?,
        None => config::load_bypass(),
    };
    let applied = sysproxy::set_proxy(&listen.ip().to_string(), listen.port(), &services, &bypass)?;
    let changed: Vec<String> = applied.iter().map(|a| a.service.clone()).collect();
    {
        let mut owned = state.sys_proxy_owned.lock().map_err(|e| e.to_string())?;
        for a in applied {
            // Keep the first record: it holds the bypass list from before this
            // app touched anything, which is what rollback must restore.
            if !owned.iter().any(|o| o.service == a.service) {
                owned.push(a);
            }
        }
    }
    push_log(
        &state.logs,
        format!(
            "system proxy now points at {listen}: {} (bypass: {})",
            changed.join(", "),
            if bypass.is_empty() {
                "none".to_string()
            } else {
                bypass.join(", ")
            }
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
    let owned_now = state
        .sys_proxy_owned
        .lock()
        .map_err(|e| e.to_string())?
        .clone();
    // No explicit list means "undo whatever we turned on".
    let targets: Vec<sysproxy::Applied> = match services {
        Some(names) if !names.is_empty() => names
            .into_iter()
            .map(
                |service| match owned_now.iter().find(|o| o.service == service) {
                    // Restore the bypass list this app replaced, when we know it.
                    Some(prev) => prev.clone(),
                    None => sysproxy::Applied::bypass_unknown(service),
                },
            )
            .collect(),
        _ => owned_now,
    };
    if targets.is_empty() {
        return Err("this app has not set any system proxy".into());
    }
    let changed = sysproxy::clear_proxy(&targets)?;
    {
        let mut owned = state.sys_proxy_owned.lock().map_err(|e| e.to_string())?;
        owned.retain(|o| !changed.contains(&o.service));
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
            clear_system_proxy,
            load_bypass_list,
            save_bypass_list
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

/// Resolve when the process is asked to quit, naming the signal that arrived.
/// On non-Unix targets only Ctrl+C exists.
async fn wait_for_shutdown() -> &'static str {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            // Without SIGTERM we can still honour Ctrl+C.
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
                return "SIGINT";
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => "SIGINT",
            _ = term.recv() => "SIGTERM",
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
        "Ctrl+C"
    }
}

/// Active services whose HTTPS proxy currently points at `listen`. Used on
/// shutdown so the CLI only reverts settings that would otherwise dead-end.
fn services_pointing_at(listen: SocketAddr) -> Vec<sysproxy::Applied> {
    let host = listen.ip().to_string();
    sysproxy::get_state()
        .unwrap_or_default()
        .into_iter()
        .filter(|s| s.https_enabled && s.port == listen.port() && s.server == host)
        .map(|s| sysproxy::Applied::bypass_unknown(s.service))
        .collect()
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
    let listen = running.addr;
    println!(
        "listening on {listen} -> upstream {} (key: {})",
        upstream.addr(),
        key_file.display()
    );

    tokio::select! {
        joined = running.handle => joined.map_err(|e| e.to_string()),
        signal = wait_for_shutdown() => {
            println!("{signal} received, shutting down");
            // Any service still pointed at this listener would lose network
            // access once we exit, so hand those back to a direct connection.
            let stranded = services_pointing_at(listen);
            if !stranded.is_empty() {
                match sysproxy::clear_proxy(&stranded) {
                    Ok(done) => println!("system proxy turned off: {}", done.join(", ")),
                    Err(e) => eprintln!("system proxy not restored: {e}"),
                }
            }
            Ok(())
        }
    }
}
