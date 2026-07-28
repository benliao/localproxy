//! System proxy control.
//!
//! macOS: driven by `networksetup`, which writes to the system config store.
//! Modifying proxy settings for a network service does not require root on a
//! normal desktop install, so no privilege escalation prompt is involved.

use serde::Serialize;
use std::process::Command;

#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
pub struct ServiceProxy {
    pub service: String,
    pub https_enabled: bool,
    pub http_enabled: bool,
    pub server: String,
    pub port: u16,
}

#[cfg(target_os = "macos")]
fn networksetup(args: &[&str]) -> Result<String, String> {
    // Args are passed as a vector (never a shell string), so service names
    // containing spaces or quotes cannot be reinterpreted as commands.
    let out = Command::new("/usr/sbin/networksetup")
        .args(args)
        .output()
        .map_err(|e| format!("failed to run networksetup: {e}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let msg = if stderr.trim().is_empty() {
            stdout.trim().to_string()
        } else {
            stderr.trim().to_string()
        };
        return Err(format!(
            "networksetup {:?} failed: {msg}",
            args.first().unwrap_or(&"")
        ));
    }
    // networksetup exits 0 even for unknown services; it reports in stdout.
    if stdout.contains("not a recognized network service") {
        return Err(stdout.trim().to_string());
    }
    Ok(stdout)
}

#[cfg(not(target_os = "macos"))]
fn networksetup(_args: &[&str]) -> Result<String, String> {
    Err("system proxy configuration is only supported on macOS".into())
}

/// Parse the `Enabled/Server/Port` block that `-getsecurewebproxy` prints.
fn parse_proxy_block(out: &str) -> (bool, String, u16) {
    let mut enabled = false;
    let mut server = String::new();
    let mut port = 0u16;
    for line in out.lines() {
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let v = v.trim();
        match k.trim() {
            "Enabled" => enabled = v.eq_ignore_ascii_case("yes"),
            "Server" => server = v.to_string(),
            "Port" => port = v.parse().unwrap_or(0),
            _ => {}
        }
    }
    (enabled, server, port)
}

/// Network services that are currently enabled, in macOS priority order.
/// Disabled services are prefixed with `*` in the listing and skipped.
pub fn active_services() -> Result<Vec<String>, String> {
    let out = networksetup(&["-listallnetworkservices"])?;
    Ok(out
        .lines()
        .skip(1) // header line about the asterisk
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('*'))
        .map(str::to_string)
        .collect())
}

pub fn get_state() -> Result<Vec<ServiceProxy>, String> {
    let mut result = Vec::new();
    for service in active_services()? {
        let https = networksetup(&["-getsecurewebproxy", &service])?;
        let http = networksetup(&["-getwebproxy", &service])?;
        let (https_enabled, server, port) = parse_proxy_block(&https);
        let (http_enabled, _, _) = parse_proxy_block(&http);
        result.push(ServiceProxy {
            service,
            https_enabled,
            http_enabled,
            server,
            port,
        });
    }
    Ok(result)
}

/// True when the service already has a bypass list we should leave alone.
fn has_bypass_domains(service: &str) -> bool {
    match networksetup(&["-getproxybypassdomains", service]) {
        // macOS prints "There aren't any bypass domains set on <service>."
        Ok(out) => !out.trim().is_empty() && !out.contains("aren't any"),
        Err(_) => true, // unknown: don't touch it
    }
}

/// Point the system HTTPS proxy at `host:port` for the named services only.
/// Bypass entries keep loopback and .local traffic off the proxy.
/// Returns the services actually changed.
pub fn set_proxy(host: &str, port: u16, services: &[String]) -> Result<Vec<String>, String> {
    if services.is_empty() {
        return Err("select at least one network service to proxy".into());
    }
    // Only act on services macOS actually reports, so a stale or crafted name
    // from the frontend can't be handed to networksetup.
    let known = active_services()?;
    let port_s = port.to_string();
    let mut changed = Vec::new();
    let mut errors = Vec::new();
    for service in services {
        if !known.contains(service) {
            errors.push(format!("{service}: not an active network service"));
            continue;
        }
        // Only the secure (HTTPS) proxy is touched. A service may already hold
        // an HTTP proxy entry the user configured; overwriting it would discard
        // their setting for no benefit here.
        let steps: [&[&str]; 2] = [
            &["-setsecurewebproxy", service, host, &port_s],
            &["-setsecurewebproxystate", service, "on"],
        ];
        match steps.iter().try_for_each(|a| networksetup(a).map(|_| ())) {
            Ok(()) => {
                // Don't clobber a user-defined bypass list; only fill an empty one
                // so loopback and .local traffic stays off the proxy.
                if !has_bypass_domains(service) {
                    let _ = networksetup(&[
                        "-setproxybypassdomains",
                        service,
                        "127.0.0.1",
                        "localhost",
                        "*.local",
                    ]);
                }
                changed.push(service.clone());
            }
            // A VPN or virtual service may reject changes; keep going and report.
            Err(e) => errors.push(format!("{service}: {e}")),
        }
    }
    if changed.is_empty() {
        return Err(errors.join("; "));
    }
    Ok(changed)
}

/// Turn the system HTTPS proxy off for the named services only.
pub fn clear_proxy(services: &[String]) -> Result<Vec<String>, String> {
    let mut changed = Vec::new();
    let mut errors = Vec::new();
    for service in services {
        match networksetup(&["-setsecurewebproxystate", service, "off"]) {
            Ok(_) => changed.push(service.clone()),
            Err(e) => errors.push(format!("{service}: {e}")),
        }
    }
    if changed.is_empty() && !errors.is_empty() {
        return Err(errors.join("; "));
    }
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_enabled_proxy_block() {
        let out = "Enabled: Yes\nServer: 127.0.0.1\nPort: 8899\nAuthenticated Proxy Enabled: 0\n";
        assert_eq!(parse_proxy_block(out), (true, "127.0.0.1".into(), 8899));
    }

    #[test]
    fn parses_disabled_proxy_block() {
        let out = "Enabled: No\nServer:\nPort: 0\n";
        assert_eq!(parse_proxy_block(out), (false, String::new(), 0));
    }

    #[test]
    fn set_proxy_requires_a_selection() {
        // Empty selection must not fall back to "all services".
        let err = set_proxy("127.0.0.1", 8899, &[]).unwrap_err();
        assert!(err.contains("select at least one"), "{err}");
    }

    #[test]
    fn clear_proxy_with_no_targets_is_a_noop() {
        assert_eq!(clear_proxy(&[]).unwrap(), Vec::<String>::new());
    }

    #[test]
    fn ignores_unparsable_lines() {
        let out = "some banner\nEnabled: yes\nPort: not-a-number\n";
        assert_eq!(parse_proxy_block(out), (true, String::new(), 0));
    }
}
