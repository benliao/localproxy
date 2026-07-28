const el = (id) => document.getElementById(id);

// Fail loudly in the UI: without the Tauri bridge nothing else can work.
const bridge = window.__TAURI__?.core?.invoke;
if (!bridge) {
  el("error").textContent =
    "未检测到 Tauri 接口（window.__TAURI__）。请通过应用本身运行，而不是在普通浏览器里打开 dist/index.html。";
  throw new Error("Tauri bridge unavailable");
}
const invoke = bridge;
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
  editor: el("editor"),
  ip: el("up-ip"),
  port: el("up-port"),
  user_in: el("up-user"),
  pass: el("up-pass"),
  save: el("save"),
  cfgpath: el("cfgpath"),
  sysOn: el("sys-on"),
  sysOff: el("sys-off"),
  sysState: el("sys-state"),
  sysList: el("sys-list"),
};

let running = false;

function showError(e) {
  ui.error.className = "error";
  ui.error.textContent = e ? String(e) : "";
}

function showOk(msg) {
  ui.error.className = "error ok";
  ui.error.textContent = msg;
}

function applyConfig(c) {
  ui.upstream.textContent = c.upstream;
  ui.user.textContent = c.has_password ? `${c.user} (已配置密码)` : c.user || "-";
  ui.keypath.textContent = c.key_path;
  const [ip, port] = c.upstream.split(":");
  ui.ip.value = ip;
  ui.port.value = port;
  ui.user_in.value = c.user || "";
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
  ui.cfgpath.textContent = await invoke("config_location");
  try {
    applyConfig(await invoke("load_config", { path: null }));
  } catch (e) {
    showError(e);
    // No credentials yet: open the editor so the next step is obvious.
    ui.editor.open = true;
  }
}

ui.save.addEventListener("click", async () => {
  showError("");
  try {
    applyConfig(
      await invoke("save_config", {
        ip: ui.ip.value.trim(),
        port: Number(ui.port.value),
        user: ui.user_in.value.trim(),
        password: ui.pass.value,
      })
    );
    ui.pass.value = "";
    ui.editor.open = false;
    showOk("已保存，可以启动了");
  } catch (e) {
    showError(e);
  }
});

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

// Which services the user ticked. Kept outside render so the periodic refresh
// never clobbers a selection in progress.
const selected = new Set();
let selectionSeeded = false;

function renderSystemProxy(services) {
  const on = services.filter((s) => s.https_enabled);
  ui.sysState.textContent = on.length ? `已启用 (${on.length})` : "未使用";
  ui.sysState.className = `badge ${on.length ? "on" : "off"}`;
  // First load: tick whatever is already proxied, else the primary service
  // (macOS lists services in priority order).
  if (!selectionSeeded && services.length) {
    selectionSeeded = true;
    if (on.length) on.forEach((s) => selected.add(s.service));
    else selected.add(services[0].service);
  }
  ui.sysList.replaceChildren(...services.map(serviceRow));
}

function serviceRow(s) {
  const li = document.createElement("li");
  li.className = s.https_enabled ? "svc on" : "svc";
  const label = document.createElement("label");
  const box = document.createElement("input");
  box.type = "checkbox";
  box.checked = selected.has(s.service);
  box.addEventListener("change", () => {
    if (box.checked) selected.add(s.service);
    else selected.delete(s.service);
  });
  const text = document.createElement("span");
  text.textContent = `${s.service} — ${s.https_enabled ? `${s.server}:${s.port}` : "直连"}`;
  label.append(box, text);
  li.append(label);
  return li;
}

async function refreshSystemProxy() {
  try {
    renderSystemProxy(await invoke("system_proxy_state"));
  } catch (e) {
    showError(e);
  }
}

async function systemProxy(cmd, okMsg, services) {
  showError("");
  ui.sysOn.disabled = ui.sysOff.disabled = true;
  try {
    const changed = await invoke(cmd, { services });
    showOk(`${okMsg}${changed.length ? "：" + changed.join("、") : ""}`);
    await refreshSystemProxy();
  } catch (e) {
    showError(e);
  } finally {
    ui.sysOn.disabled = ui.sysOff.disabled = false;
  }
}

ui.sysOn.addEventListener("click", () =>
  systemProxy("set_system_proxy", "系统代理已开启", [...selected])
);
// Restoring falls back to "whatever this app turned on" when nothing is ticked.
ui.sysOff.addEventListener("click", () =>
  systemProxy("clear_system_proxy", "已恢复直连", selected.size ? [...selected] : null)
);

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
await refreshSystemProxy();
setInterval(poll, 1000);
// Cheaper cadence: each refresh shells out to networksetup per service.
setInterval(refreshSystemProxy, 5000);
