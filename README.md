# LocalProxy

Tauri 2 桌面应用：在本机开一个**免密**的 HTTP/HTTPS 代理，把流量转发到**需要账号密码**的远端 HTTP 代理。

浏览器 / curl → `127.0.0.1:8899`（无需认证）→ 远端代理（自动注入 Basic 认证）→ 目标站点

HTTPS 通过标准 `CONNECT` 隧道透传，TLS 端到端加密，本程序不解密、不做中间人，因此不需要自签证书。

## 凭据文件 `.key`

放在项目根目录（或可执行文件的任一上级目录），`key=value` 每行一对：

```
ip=your-proxy-ip
port=your-proxy-port
user=your-user
password=your-password
```

`ip`/`host`、`user`/`username`、`password`/`pass` 均可；`#` 开头为注释。该文件已加入 `.gitignore`，密码不会通过前端接口返回。

## 运行

```bash
npm run dev      # 开发模式启动桌面应用
npm run build    # 打包 .app / .dmg
npm test         # 单元测试
```

界面里填写本地监听地址（默认 `127.0.0.1:8899`），点「启动」，下方可看到实时连接日志与计数。

无界面模式（调试用）：

```bash
npm run cli                                        # 127.0.0.1:8899，自动寻找 .key
./src-tauri/target/debug/localproxy --cli 127.0.0.1:8899 /path/to/.key
```

## 客户端配置

```bash
export https_proxy=http://127.0.0.1:8899
export http_proxy=http://127.0.0.1:8899
curl -x http://127.0.0.1:8899 https://example.com/
```

macOS 系统代理：系统设置 → 网络 → 详细信息 → 代理，网页代理与安全网页代理都填 `127.0.0.1` / `8899`。

## 安全说明

默认只监听 `127.0.0.1`，本机进程可直接使用，无需密码。若改成 `0.0.0.0:8899`，同一网络内任何人都能借用你的上游账号——除非你确实需要共享，否则不要这么做。客户端自带的 `Proxy-Authorization` / `Proxy-Connection` 头会被丢弃，只发送 `.key` 里的凭据。

## 结构

```
src-tauri/src/config.rs   .key 解析、Basic 头生成
src-tauri/src/proxy.rs    监听、请求头改写、CONNECT 隧道与双向转发
src-tauri/src/lib.rs      Tauri 命令（start/stop/status/logs）与状态管理
dist/                     前端（原生 HTML/CSS/JS）
scripts/                  图标生成、测试用的带认证上游代理
```
