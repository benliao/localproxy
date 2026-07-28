# LocalProxy

Tauri 2 desktop app for macOS: runs a **no-auth** HTTP/HTTPS proxy on your machine and forwards
traffic to a remote HTTP proxy that **requires a username and password**.

Browser / curl → `127.0.0.1:8899` (no auth) → remote proxy (Basic auth injected) → target site

HTTPS is tunneled with a standard `CONNECT`, so TLS stays end to end. This app never decrypts
traffic and never acts as a man in the middle, which means no self-signed certificate is needed.

## Credentials file `.key`

Put it in the project root (or any parent directory of the executable), one `key=value` per line:

```
ip=your-proxy-ip
port=your-proxy-port
user=your-user
password=your-password
```

`ip`/`host`, `user`/`username` and `password`/`pass` are all accepted; lines starting with `#` are
comments. The file is listed in `.gitignore`, and the password is never returned through the
frontend API.

## Running

```bash
npm run dev      # launch the desktop app in dev mode
npm run build    # bundle .app / .dmg
npm test         # unit tests
```

Enter the local listen address in the UI (default `127.0.0.1:8899`) and hit Start. Live connection
logs and counters show up below.

Headless mode (for debugging):

```bash
npm run cli                                        # 127.0.0.1:8899, auto-discovers .key
./src-tauri/target/debug/localproxy --cli 127.0.0.1:8899 /path/to/.key
```

On `SIGINT` (Ctrl+C) or `SIGTERM`, headless mode checks which network services still point their
HTTPS proxy at its own listener and switches those back to a direct connection before exiting, so a
stopped proxy never leaves the machine pointed at a dead port.

## Client configuration

```bash
export https_proxy=http://127.0.0.1:8899
export http_proxy=http://127.0.0.1:8899
curl -x http://127.0.0.1:8899 https://example.com/
```

## System proxy (macOS)

The System proxy card lists your network services (Wi-Fi, Ethernet, ...). Tick the ones that should
route through the local listener and hit Apply to selected. Only the **HTTPS** proxy is touched via
`networksetup -setsecurewebproxy`; your existing HTTP proxy entry is left as it is. Stopping the
local proxy or quitting the app restores a direct connection on every service this app changed.

### Bypass list

Hosts in the bypass list keep a direct connection while the system proxy is on. Put one entry per
line (`localhost`, `*.internal.example`, `192.168.0.0/16`); it is applied to the ticked services via
`networksetup -setproxybypassdomains`. Save bypass list persists it to the config directory
(`bypass.txt`) so it is reused next time. Each service's previous bypass list is captured before the
first change and restored when the proxy is turned off. Entries starting with `-`, containing
whitespace, or equal to macOS's reserved `Empty` sentinel are rejected.

Note that `curl` does not read the macOS system proxy settings. To verify, use something that does,
for example Python's `urllib` (which honors `getproxies()`), or a browser.

## Security notes

The listener binds `127.0.0.1` only, so local processes can use it without a password. Changing it
to `0.0.0.0:8899` would let anyone on the same network borrow your upstream account, so don't do
that unless sharing is exactly what you want. Client-supplied `Proxy-Authorization` and
`Proxy-Connection` headers are dropped; only the credentials from `.key` are sent upstream.

## Layout

```
src-tauri/src/config.rs   .key parsing, Basic header generation
src-tauri/src/proxy.rs    listener, header rewriting, CONNECT tunnel and bidirectional relay
src-tauri/src/sysproxy.rs macOS networksetup wrapper for per-service proxy control
src-tauri/src/lib.rs      Tauri commands (start/stop/status/logs) and state management
dist/                     frontend (plain HTML/CSS/JS)
scripts/                  icon generation, authenticated upstream proxy for testing
```
