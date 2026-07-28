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
    /// Hosts macOS currently keeps off the proxy for this service.
    pub bypass: Vec<String>,
}

/// A service this app switched over, plus the bypass list it replaced, so the
/// original setting can be put back verbatim. `None` means the prior list is
/// unknown (e.g. a process restart), in which case rollback leaves it as is
/// rather than guessing and wiping the user's entries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Applied {
    pub service: String,
    pub previous_bypass: Option<Vec<String>>,
}

impl Applied {
    /// A record for a service whose earlier bypass list we never captured.
    pub fn bypass_unknown(service: String) -> Self {
        Self {
            service,
            previous_bypass: None,
        }
    }
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
        let bypass = get_bypass(&service);
        result.push(ServiceProxy {
            service,
            https_enabled,
            http_enabled,
            server,
            port,
            bypass,
        });
    }
    Ok(result)
}

/// Parse the bypass domain listing, one entry per line.
/// macOS prints "There aren't any bypass domains set on <service>." when empty.
fn parse_bypass(out: &str) -> Vec<String> {
    if out.contains("aren't any") {
        return Vec::new();
    }
    out.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

fn get_bypass(service: &str) -> Vec<String> {
    networksetup(&["-getproxybypassdomains", service])
        .map(|out| parse_bypass(&out))
        .unwrap_or_default()
}

/// Replace a service's bypass list. An empty list clears it, which macOS spells
/// as the literal argument `Empty`.
fn write_bypass(service: &str, entries: &[String]) -> Result<(), String> {
    let mut args: Vec<&str> = vec!["-setproxybypassdomains", service];
    if entries.is_empty() {
        args.push("Empty");
    } else {
        args.extend(entries.iter().map(String::as_str));
    }
    networksetup(&args).map(|_| ())
}

/// Point the system HTTPS proxy at `host:port` for the named services only,
/// and set `bypass` as the list of hosts that stay on a direct connection.
/// Returns what changed, including each service's prior bypass list.
pub fn set_proxy(
    host: &str,
    port: u16,
    services: &[String],
    bypass: &[String],
) -> Result<Vec<Applied>, String> {
    if services.is_empty() {
        return Err("select at least one network service to proxy".into());
    }
    // Only act on services macOS actually reports, so a stale or crafted name
    // from the frontend can't be handed to networksetup.
    let known = active_services()?;
    let port_s = port.to_string();
    let mut changed: Vec<Applied> = Vec::new();
    let mut errors = Vec::new();
    for service in services {
        if !known.contains(service) {
            errors.push(format!("{service}: not an active network service"));
            continue;
        }
        // Capture the current list first: applying ours overwrites it, and the
        // rollback has to restore exactly what the user had.
        let previous_bypass = get_bypass(service);
        // Only the secure (HTTPS) proxy is touched. A service may already hold
        // an HTTP proxy entry the user configured; overwriting it would discard
        // their setting for no benefit here.
        let steps: [&[&str]; 2] = [
            &["-setsecurewebproxy", service, host, &port_s],
            &["-setsecurewebproxystate", service, "on"],
        ];
        match steps.iter().try_for_each(|a| networksetup(a).map(|_| ())) {
            Ok(()) => {
                if let Err(e) = write_bypass(service, bypass) {
                    // The proxy itself is live; a failed bypass write only means
                    // more traffic is proxied than asked, so report and continue.
                    errors.push(format!("{service}: bypass list not applied: {e}"));
                }
                changed.push(Applied {
                    service: service.clone(),
                    previous_bypass: Some(previous_bypass),
                });
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
pub fn clear_proxy(targets: &[Applied]) -> Result<Vec<String>, String> {
    let mut changed = Vec::new();
    let mut errors = Vec::new();
    for t in targets {
        match networksetup(&["-setsecurewebproxystate", &t.service, "off"]) {
            Ok(_) => {
                // Put the user's own bypass list back; ours was only meaningful
                // while traffic was being proxied.
                if let Some(previous) = &t.previous_bypass {
                    if let Err(e) = write_bypass(&t.service, previous) {
                        errors.push(format!("{}: bypass list not restored: {e}", t.service));
                    }
                }
                changed.push(t.service.clone());
            }
            Err(e) => errors.push(format!("{}: {e}", t.service)),
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
        let err = set_proxy("127.0.0.1", 8899, &[], &[]).unwrap_err();
        assert!(err.contains("select at least one"), "{err}");
    }

    #[test]
    fn clear_proxy_with_no_targets_is_a_noop() {
        assert_eq!(clear_proxy(&[]).unwrap(), Vec::<String>::new());
    }

    #[test]
    fn unknown_prior_bypass_is_left_alone_on_rollback() {
        // No captured list means rollback must not write one, otherwise a
        // restart would erase entries the user set outside this app.
        let a = Applied::bypass_unknown("Wi-Fi".into());
        assert_eq!(a.previous_bypass, None);
    }

    #[test]
    fn parses_bypass_listing() {
        assert_eq!(
            parse_bypass("*.local\n169.254/16\n"),
            vec!["*.local", "169.254/16"]
        );
    }

    #[test]
    fn parses_empty_bypass_listing() {
        let out = "There aren't any bypass domains set on Wi-Fi.\n";
        assert_eq!(parse_bypass(out), Vec::<String>::new());
    }

    #[test]
    fn ignores_unparsable_lines() {
        let out = "some banner\nEnabled: yes\nPort: not-a-number\n";
        assert_eq!(parse_proxy_block(out), (true, String::new(), 0));
    }
}
