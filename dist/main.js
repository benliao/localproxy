const invoke = window.__TAURI__.core.invoke;

const el = (id) => document.getElementById(id);
const ui = {
  bind: el("bind"),
  toggle: el("toggle"),
  reload: el("reload"),
  clear: el("clear"),
  state: el("state"),
  error: el("error"),
  upstream: el("upstream"),
  user: el("user"),
  keypath: el("keypath"),
  counts: el("counts"),
  logs: el("logs"),
};

let running = false;

function showError(e) {
  ui.error.textContent = e ? String(e) : "";
}

function render(status) {
  running = status.running;
  ui.state.textContent = running ? "运行中" : "已停止";
  ui.state.className = `badge ${running ? "on" : "off"}`;
  ui.toggle.textContent = running ? "停止" : "启动";
  ui.toggle.classList.toggle("stop", running);
  ui.bind.disabled = running;
  if (status.upstream) ui.upstream.textContent = status.upstream;
  if (status.upstream_user) ui.user.textContent = status.upstream_user;
  if (status.key_path) ui.keypath.textContent = status.key_path;
  if (running && status.local_addr) ui.bind.value = status.local_addr;
  ui.counts.textContent = `${status.active} 活跃 / ${status.total} 累计 / ${status.failed} 失败`;
}

async function loadConfig() {
  showError("");
  try {
    const c = await invoke("load_config", { path: null });
    ui.upstream.textContent = c.upstream;
    ui.user.textContent = c.has_password ? `${c.user} (已配置密码)` : c.user || "-";
    ui.keypath.textContent = c.key_path;
  } catch (e) {
    showError(e);
  }
}

ui.toggle.addEventListener("click", async () => {
  showError("");
  ui.toggle.disabled = true;
  try {
    const cmd = running ? "stop_proxy" : "start_proxy";
    const args = running ? {} : { bind: ui.bind.value.trim(), keyPath: null };
    render(await invoke(cmd, args));
  } catch (e) {
    showError(e);
  } finally {
    ui.toggle.disabled = false;
  }
});

ui.reload.addEventListener("click", loadConfig);
ui.clear.addEventListener("click", async () => {
  await invoke("clear_logs");
  ui.logs.textContent = "";
});

async function poll() {
  try {
    render(await invoke("status"));
    const lines = await invoke("logs");
    const atBottom = ui.logs.scrollTop + ui.logs.clientHeight >= ui.logs.scrollHeight - 20;
    ui.logs.textContent = lines.join("\n");
    if (atBottom) ui.logs.scrollTop = ui.logs.scrollHeight;
  } catch (e) {
    showError(e);
  }
}

await loadConfig();
await poll();
setInterval(poll, 1000);
