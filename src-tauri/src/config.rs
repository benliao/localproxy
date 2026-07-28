use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

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
            .ok_or_else(|| format!("第 {} 行缺少 '='", idx + 1))?;
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
    let ip = ip.filter(|v| !v.is_empty()).ok_or("缺少 ip")?;
    let port = port.filter(|v| !v.is_empty()).ok_or("缺少 port")?;
    let port: u16 = port.parse().map_err(|_| format!("port 无效: {port}"))?;
    Ok(Upstream {
        ip,
        port,
        user: user.unwrap_or_default(),
        password: password.unwrap_or_default(),
    })
}

/// Look for `.key` next to the executable, in the CWD, or walking up parent dirs.
pub fn find_key_file() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        let mut dir = exe.parent().map(Path::to_path_buf);
        while let Some(d) = dir {
            candidates.push(d.join(".key"));
            dir = d.parent().map(Path::to_path_buf);
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        let mut dir = Some(cwd);
        while let Some(d) = dir {
            candidates.push(d.join(".key"));
            dir = d.parent().map(Path::to_path_buf);
        }
    }
    candidates.into_iter().find(|p| p.is_file())
}

pub fn load_upstream(path: Option<&Path>) -> Result<(Upstream, PathBuf), String> {
    let path = match path {
        Some(p) => p.to_path_buf(),
        None => find_key_file().ok_or("找不到 .key 文件")?,
    };
    let contents =
        std::fs::read_to_string(&path).map_err(|e| format!("读取 {} 失败: {e}", path.display()))?;
    let upstream =
        parse_key_file(&contents).map_err(|e| format!("{} 解析失败: {e}", path.display()))?;
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
    fn rejects_missing_fields() {
        assert!(parse_key_file("user=u\npassword=p\n").is_err());
        assert!(parse_key_file("ip=h\nport=notanumber\n").is_err());
    }
}
