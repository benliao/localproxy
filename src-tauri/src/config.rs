use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const KEY_FILE: &str = ".key";
const BYPASS_FILE: &str = "bypass.txt";
const APP_DIR: &str = "com.raroro.localproxy";

/// Hosts that should never go through the proxy. Loopback and `.local` are
/// included because sending them upstream breaks local development and mDNS.
pub const DEFAULT_BYPASS: [&str; 3] = ["127.0.0.1", "localhost", "*.local"];

/// Upstream proxy credentials, parsed from a `key=value` file (`.key`).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Upstream {
    pub ip: String,
    pub port: u16,
    pub user: String,
    #[serde(skip_serializing)]
    pub password: String,
}

impl Upstream {
    pub fn addr(&self) -> String {
        format!("{}:{}", self.ip, self.port)
    }

    /// `Proxy-Authorization: Basic base64(user:password)`
    pub fn basic_auth_header(&self) -> String {
        use base64::Engine as _;
        let raw = format!("{}:{}", self.user, self.password);
        let encoded = base64::engine::general_purpose::STANDARD.encode(raw);
        format!("Proxy-Authorization: Basic {encoded}")
    }
}

/// Parse a `.key` file: one `name=value` pair per line, `#` starts a comment.
pub fn parse_key_file(contents: &str) -> Result<Upstream, String> {
    let mut ip = None;
    let mut port = None;
    let mut user = None;
    let mut password = None;

    for (idx, line) in contents.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| format!("line {} is missing '='", idx + 1))?;
        let value = value.trim().to_string();
        match key.trim().to_ascii_lowercase().as_str() {
            "ip" | "host" | "server" => ip = Some(value),
            "port" => port = Some(value),
            "user" | "username" => user = Some(value),
            "password" | "pass" => password = Some(value),
            _ => {}
        }
    }
    build(ip, port, user, password)
}

fn build(
    ip: Option<String>,
    port: Option<String>,
    user: Option<String>,
    password: Option<String>,
) -> Result<Upstream, String> {
    let ip = ip.filter(|v| !v.is_empty()).ok_or("missing ip")?;
    let port = port.filter(|v| !v.is_empty()).ok_or("missing port")?;
    let port: u16 = port.parse().map_err(|_| format!("invalid port: {port}"))?;
    Ok(Upstream {
        ip,
        port,
        user: user.unwrap_or_default(),
        password: password.unwrap_or_default(),
    })
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
}

/// Per-user config directory. This is where a packaged app keeps its `.key`,
/// since the `.app` bundle itself is replaced on every update.
pub fn config_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("LOCALPROXY_CONFIG_DIR").map(PathBuf::from) {
        return dir;
    }
    match home_dir() {
        #[cfg(target_os = "macos")]
        Some(h) => h.join("Library/Application Support").join(APP_DIR),
        #[cfg(not(target_os = "macos"))]
        Some(h) => std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| h.join(".config"))
            .join(APP_DIR),
        None => PathBuf::from(".").join(APP_DIR),
    }
}

/// Resolve `.key` in priority order:
/// 1. `LOCALPROXY_KEY` env var (explicit override)
/// 2. the per-user config dir (where the packaged app stores it)
/// 3. next to the executable / CWD, walking up but never past `$HOME`
///
/// The `$HOME` boundary matters: in a `.app` bundle the exe lives under
/// `/Applications`, and walking to `/` would let anyone plant a `.key` in a
/// shared directory that we would then read credentials from.
pub fn find_key_file() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("LOCALPROXY_KEY").map(PathBuf::from) {
        if p.is_file() {
            return Some(p);
        }
    }
    let in_config = config_dir().join(KEY_FILE);
    if in_config.is_file() {
        return Some(in_config);
    }

    let home = home_dir();
    let starts = [
        std::env::current_exe()
            .ok()
            .and_then(|e| e.parent().map(Path::to_path_buf)),
        std::env::current_dir().ok(),
    ];
    let mut candidates: Vec<PathBuf> = Vec::new();
    for start in starts.into_iter().flatten() {
        let mut dir = Some(start);
        while let Some(d) = dir {
            // Only trust locations inside the user's own home directory.
            if let Some(h) = home.as_ref() {
                if !d.starts_with(h) {
                    break;
                }
            }
            candidates.push(d.join(KEY_FILE));
            dir = d.parent().map(Path::to_path_buf);
        }
    }
    candidates.into_iter().find(|p| p.is_file())
}

/// Write credentials to the per-user config dir with owner-only permissions.
pub fn save_upstream(up: &Upstream) -> Result<PathBuf, String> {
    let dir = config_dir();
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("failed to create {}: {e}", dir.display()))?;
    let path = dir.join(KEY_FILE);
    let body = format!(
        "ip={}\nport={}\nuser={}\npassword={}\n",
        up.ip, up.port, up.user, up.password
    );
    std::fs::write(&path, body).map_err(|e| format!("failed to write {}: {e}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // 0600: credentials must not be readable by other local users.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("failed to set permissions: {e}"))?;
    }
    Ok(path)
}

/// Validate and normalize a bypass list coming from the UI or a file.
///
/// Entries are handed to `networksetup` as separate argv items, so the risk is
/// not shell injection but an entry being read as a flag. Rejecting a leading
/// `-` and any internal whitespace keeps each entry a single, inert operand.
pub fn sanitize_bypass(entries: &[String]) -> Result<Vec<String>, String> {
    let mut out: Vec<String> = Vec::new();
    for raw in entries {
        let e = raw.trim();
        if e.is_empty() || e.starts_with('#') {
            continue;
        }
        if e.starts_with('-') {
            return Err(format!("bypass entry must not start with '-': {e}"));
        }
        if e.split_whitespace().count() > 1 {
            return Err(format!("bypass entry must not contain spaces: {e}"));
        }
        // "Empty" is networksetup's sentinel for clearing the list; a literal
        // entry would silently wipe everything instead of being added.
        if e.eq_ignore_ascii_case("empty") {
            return Err("\"Empty\" is reserved by macOS; remove all entries instead".into());
        }
        if !out.iter().any(|k| k == e) {
            out.push(e.to_string());
        }
    }
    Ok(out)
}

/// Bypass list from the per-user config dir, falling back to the defaults when
/// the file is absent. A file that exists but is empty means "no bypass".
pub fn load_bypass() -> Vec<String> {
    let path = config_dir().join(BYPASS_FILE);
    match std::fs::read_to_string(&path) {
        Ok(body) => {
            let lines: Vec<String> = body.lines().map(str::to_string).collect();
            sanitize_bypass(&lines).unwrap_or_default()
        }
        Err(_) => DEFAULT_BYPASS.iter().map(|s| s.to_string()).collect(),
    }
}

/// Persist the bypass list, one entry per line.
pub fn save_bypass(entries: &[String]) -> Result<(Vec<String>, PathBuf), String> {
    let clean = sanitize_bypass(entries)?;
    let dir = config_dir();
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("failed to create {}: {e}", dir.display()))?;
    let path = dir.join(BYPASS_FILE);
    let mut body = clean.join("\n");
    body.push('\n');
    std::fs::write(&path, body).map_err(|e| format!("failed to write {}: {e}", path.display()))?;
    Ok((clean, path))
}

pub fn load_upstream(path: Option<&Path>) -> Result<(Upstream, PathBuf), String> {
    let path = match path {
        Some(p) => p.to_path_buf(),
        None => find_key_file().ok_or_else(|| {
            format!(
                ".key not found. Enter the upstream proxy details in the app, or create {} manually",
                config_dir().join(KEY_FILE).display()
            )
        })?,
    };
    let contents = std::fs::read_to_string(&path)
        .map_err(|e| format!("failed to read {}: {e}", path.display()))?;
    let upstream = parse_key_file(&contents)
        .map_err(|e| format!("failed to parse {}: {e}", path.display()))?;
    Ok((upstream, path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_basic_file() {
        let up = parse_key_file("ip=203.0.113.7\nport=808\nuser=alice\npassword=s3cret\n").unwrap();
        assert_eq!(up.addr(), "203.0.113.7:808");
        assert_eq!(
            up.basic_auth_header(),
            "Proxy-Authorization: Basic YWxpY2U6czNjcmV0"
        );
    }

    #[test]
    fn ignores_comments_and_unknown_keys() {
        let up = parse_key_file("# c\nHOST=h\nPORT=1080\nuser=u\npass=p\nnote=x\n").unwrap();
        assert_eq!(up.ip, "h");
        assert_eq!(up.port, 1080);
        assert_eq!(up.user, "u");
    }

    #[test]
    fn config_dir_honors_override() {
        // Uses a process-wide env var, so keep it in one test.
        let tmp = std::env::temp_dir().join("lp-cfg-test");
        std::env::set_var("LOCALPROXY_CONFIG_DIR", &tmp);
        assert_eq!(config_dir(), tmp);

        let up = Upstream {
            ip: "10.1.2.3".into(),
            port: 8080,
            user: "bob".into(),
            password: "pw".into(),
        };
        let saved = save_upstream(&up).unwrap();
        assert_eq!(saved, tmp.join(".key"));

        let (round, _) = load_upstream(None).unwrap();
        assert_eq!(round.ip, "10.1.2.3");
        assert_eq!(round.port, 8080);
        assert_eq!(round.password, "pw");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&saved).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "credentials must be owner-only");
        }

        std::fs::remove_dir_all(&tmp).ok();
        std::env::remove_var("LOCALPROXY_CONFIG_DIR");
    }

    #[test]
    fn sanitize_bypass_trims_dedups_and_drops_comments() {
        let got = sanitize_bypass(&[
            " *.corp.example ".into(),
            "".into(),
            "# a comment".into(),
            "localhost".into(),
            "localhost".into(),
        ])
        .unwrap();
        assert_eq!(got, vec!["*.corp.example", "localhost"]);
    }

    #[test]
    fn sanitize_bypass_rejects_unsafe_entries() {
        // A leading '-' would be read as a networksetup flag.
        assert!(sanitize_bypass(&["-setwebproxy".into()]).is_err());
        assert!(sanitize_bypass(&["a.com b.com".into()]).is_err());
        // macOS treats "Empty" as "clear the whole list".
        assert!(sanitize_bypass(&["Empty".into()]).is_err());
    }

    #[test]
    fn rejects_missing_fields() {
        assert!(parse_key_file("user=u\npassword=p\n").is_err());
        assert!(parse_key_file("ip=h\nport=notanumber\n").is_err());
    }
}
