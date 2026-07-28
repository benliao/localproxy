mod config;
mod proxy;

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use serde::Serialize;
use tauri::{Manager, State};
use tokio::sync::mpsc;

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

#[tauri::command]
async fn start_proxy(
    state: State<'_, AppState>,
    bind: Option<String>,
    key_path: Option<String>,
) -> Result<Status, String> {
    let mut guard = state.running.lock().await;
    if guard.is_some() {
        return Err("代理已在运行".into());
    }

    let (upstream, key_file) =
        config::load_upstream(key_path.as_deref().map(std::path::Path::new))?;
    let bind_str = bind.unwrap_or_else(|| DEFAULT_BIND.to_string());
    let bind_addr: SocketAddr = bind_str
        .parse()
        .map_err(|_| format!("本地监听地址无效: {bind_str}"))?;

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
            "本地代理已启动 {} -> 上游 {}",
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
                push_log(&state.logs, "本地代理已停止".into());
            }
            None => return Err("代理未在运行".into()),
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

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .setup(|app| {
            app.manage(AppState::default());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            load_config,
            start_proxy,
            stop_proxy,
            status,
            logs,
            clear_logs
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

/// Headless entry point: `localproxy --cli [bind] [key_path]`, useful for testing.
pub async fn run_cli(bind: &str, key_path: Option<&str>) -> Result<(), String> {
    let (upstream, key_file) = config::load_upstream(key_path.map(std::path::Path::new))?;
    let addr: SocketAddr = bind.parse().map_err(|_| format!("地址无效: {bind}"))?;
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
