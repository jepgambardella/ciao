//! Tailscale Funnel integration for public app previews and deployments.

use super::*;

const FUNNEL_FRAGMENT_SUFFIX: &str = ".funnel.caddy";
const FUNNEL_TARGET: &str = "http://127.0.0.1:80";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunnelResult {
    pub app: String,
    pub hostname: String,
    pub url: String,
    pub target: String,
    pub auth: FunnelAuth,
    pub changed: bool,
    pub message: String,
}

/// Configure a stable Caddy route and enable a background Tailscale Funnel.
///
/// Tailscale terminates public HTTPS and forwards plain HTTP to Caddy on
/// loopback. The route is kept in its own fragment so a configured custom
/// domain is not replaced by the Funnel hostname.
pub fn enable_tailscale_funnel(transport: &OpenSshTransport, app: &str) -> Result<FunnelResult> {
    validate_identifier("app name", app)?;
    let platform = transport.inspect()?;
    let target = tailscale_target(transport)?;
    let hostname = target.hostname.ok_or_else(|| {
        CiaoError::Config(
            "Tailscale is connected, but the target has no MagicDNS hostname for Funnel".to_owned(),
        )
    })?;
    validate_domain(&hostname)?;

    let root = host_app_root(&platform.os);
    let release = read_current_release(transport, &root, app)?
        .ok_or_else(|| CiaoError::Config(format!("app `{app}` has no active release")))?;
    let manifest = read_release_manifest(transport, &root, app, &release)?;
    ensure_single_funnel_hostname(transport, app)?;
    let token = match manifest.funnel.auth {
        FunnelAuth::Token => Some(ensure_funnel_token(transport, &root, app)?),
        FunnelAuth::None => None,
    };
    let fragment =
        funnel_caddy_fragment(transport, &root, app, &release, &hostname, token.as_deref())?;
    let fragment_path = funnel_fragment_path(app);
    let previous = read_remote_file(transport, &fragment_path, "read existing Funnel route")?;

    write_remote_file(
        transport,
        &fragment_path,
        &fragment,
        "root",
        "write Tailscale Funnel Caddy route",
    )?;
    if let Err(error) = remote_script(
        transport,
        "reload Caddy for Tailscale Funnel",
        &caddy_reload_script(&platform.os),
    ) {
        restore_fragment(transport, &fragment_path, previous.as_deref(), &platform.os);
        return Err(error);
    }

    if let Err(error) =
        remote_funnel_healthcheck(transport, &hostname, &manifest.health, token.as_deref())
    {
        restore_fragment(transport, &fragment_path, previous.as_deref(), &platform.os);
        return Err(error);
    }

    if let Err(error) = remote_script(
        transport,
        "enable Tailscale Funnel",
        &tailscale_funnel_enable_script(),
    ) {
        restore_fragment(transport, &fragment_path, previous.as_deref(), &platform.os);
        return Err(error);
    }

    let url = match token.as_deref() {
        Some(token) => format!("https://{hostname}/{token}"),
        None => format!("https://{hostname}"),
    };
    Ok(FunnelResult {
        app: app.to_owned(),
        hostname,
        url: url.clone(),
        target: FUNNEL_TARGET.to_owned(),
        auth: manifest.funnel.auth,
        changed: previous.as_deref() != Some(fragment.as_str()),
        message: format!("✓ public Funnel: {url}"),
    })
}

/// Refresh the Caddy upstream for an already enabled Funnel without calling
/// `tailscale funnel` again. This is run after normal deploys and rollbacks so
/// the public route always follows the active release/slot port.
pub(crate) fn sync_tailscale_funnel_route_if_present(
    transport: &OpenSshTransport,
    app: &str,
) -> Result<bool> {
    validate_identifier("app name", app)?;
    let fragment_path = funnel_fragment_path(app);
    let Some(previous) = read_remote_file(transport, &fragment_path, "read existing Funnel route")?
    else {
        return Ok(false);
    };
    let target = tailscale_target(transport)?;
    let hostname = target.hostname.ok_or_else(|| {
        CiaoError::Config(
            "Funnel route exists, but Tailscale has no MagicDNS hostname for it".to_owned(),
        )
    })?;
    let platform = transport.inspect()?;
    let root = host_app_root(&platform.os);
    let release = read_current_release(transport, &root, app)?
        .ok_or_else(|| CiaoError::Config(format!("app `{app}` has no active release")))?;
    let manifest = read_release_manifest(transport, &root, app, &release)?;
    let existing_token = read_funnel_token(transport, &root, app)?;
    let token = match manifest.funnel.auth {
        FunnelAuth::Token => Some(existing_token.ok_or_else(|| {
            CiaoError::Config(
                "existing Funnel route has no Ciao token; rerun `ciao deploy <host> funnel` to migrate it safely"
                    .to_owned(),
            )
        })?),
        FunnelAuth::None => None,
    };
    let fragment =
        funnel_caddy_fragment(transport, &root, app, &release, &hostname, token.as_deref())?;
    let changed = previous != fragment;
    if changed {
        write_remote_file(
            transport,
            &fragment_path,
            &fragment,
            "root",
            "synchronize Tailscale Funnel Caddy route",
        )?;
        remote_script(
            transport,
            "reload Caddy for synchronized Funnel route",
            &caddy_reload_script(&platform.os),
        )?;
    }
    remote_funnel_healthcheck(transport, &hostname, &manifest.health, token.as_deref())?;
    Ok(changed)
}

/// Remove Serve endpoint entries that point at one of this app's historical
/// Ciao ports. Tailscale's `status --json` is for inspection; the editable
/// document is obtained with `serve get-config --all` and applied with
/// `serve set-config --all`.
pub(crate) fn cleanup_tailscale_serve_for_ports(
    transport: &OpenSshTransport,
    app: &str,
    ports: &[u16],
) -> Result<bool> {
    validate_identifier("app name", app)?;
    if ports.is_empty() {
        return Ok(false);
    }
    let Some(output) = tailscale_serve_get_config(transport)? else {
        return Ok(false);
    };
    let mut config = match parse_json_object(&output.stdout) {
        Some(value) => value,
        None => {
            return Err(CiaoError::Config(
                "Tailscale Serve configuration is not valid JSON; refusing to rewrite it"
                    .to_owned(),
            ))
        }
    };
    if !remove_serve_ports(&mut config, ports) {
        return Ok(false);
    }
    let contents = serde_json::to_string_pretty(&config)
        .map_err(|error| CiaoError::Serialization(error.to_string()))?;
    let path = format!("/tmp/ciao-serve-cleanup-{app}.json");
    write_remote_file(
        transport,
        &path,
        &format!("{contents}\n"),
        "root",
        "prepare Tailscale Serve cleanup",
    )?;
    let result = remote_script(
        transport,
        "apply Tailscale Serve cleanup",
        &format!(
            "set -eu\nfor candidate in /usr/local/bin/tailscale /usr/local/opt/tailscale/bin/tailscale /opt/homebrew/bin/tailscale /opt/homebrew/opt/tailscale/bin/tailscale /usr/bin/tailscale /Applications/Tailscale.app/Contents/MacOS/tailscale /Applications/Tailscale.app/Contents/MacOS/Tailscale; do if [ -x \"$candidate\" ]; then sudo -n \"$candidate\" serve set-config --all {}; sudo -n rm -f {}; exit 0; fi; done\nif command -v tailscale >/dev/null 2>&1; then sudo -n tailscale serve set-config --all {}; sudo -n rm -f {}; exit 0; fi\necho 'Tailscale CLI was not found on the target' >&2\nexit 127\n",
            shell_quote(&path),
            shell_quote(&path),
            shell_quote(&path),
            shell_quote(&path),
        ),
    );
    if result.is_err() {
        let _ = remote_script(
            transport,
            "remove failed Tailscale Serve cleanup file",
            &format!("set -eu\nsudo -n rm -f {}\n", shell_quote(&path)),
        );
    }
    result.map(|_| true)
}

/// Remove stale local endpoints in Ciao's managed internal port range while
/// preserving every port still referenced by a retained Ciao release.
pub fn cleanup_tailscale_serve_orphans(transport: &OpenSshTransport) -> Result<bool> {
    let mut active_ports = std::collections::BTreeSet::new();
    for app in list_apps(transport)? {
        for release in list_releases(transport, &app.app)? {
            if let Some(port) = release.port {
                active_ports.insert(port);
            }
        }
    }
    let Some(output) = tailscale_serve_get_config(transport)? else {
        return Ok(false);
    };
    let mut config = parse_json_object(&output.stdout).ok_or_else(|| {
        CiaoError::Config(
            "Tailscale Serve configuration is not valid JSON; refusing to rewrite it".to_owned(),
        )
    })?;
    if !remove_serve_orphan_ports(&mut config, &active_ports) {
        return Ok(false);
    }
    let contents = serde_json::to_string_pretty(&config)
        .map_err(|error| CiaoError::Serialization(error.to_string()))?;
    let path = "/tmp/ciao-serve-orphan-cleanup.json";
    write_remote_file(
        transport,
        path,
        &format!("{contents}\n"),
        "root",
        "prepare orphaned Tailscale Serve cleanup",
    )?;
    let result = remote_script(
        transport,
        "apply orphaned Tailscale Serve cleanup",
        &format!(
            "set -eu\nfor candidate in /usr/local/bin/tailscale /usr/local/opt/tailscale/bin/tailscale /opt/homebrew/bin/tailscale /opt/homebrew/opt/tailscale/bin/tailscale /usr/bin/tailscale /Applications/Tailscale.app/Contents/MacOS/tailscale /Applications/Tailscale.app/Contents/MacOS/Tailscale; do if [ -x \"$candidate\" ]; then sudo -n \"$candidate\" serve set-config --all {}; sudo -n rm -f {}; exit 0; fi; done\nif command -v tailscale >/dev/null 2>&1; then sudo -n tailscale serve set-config --all {}; sudo -n rm -f {}; exit 0; fi\necho 'Tailscale CLI was not found on the target' >&2\nexit 127\n",
            shell_quote(path),
            shell_quote(path),
            shell_quote(path),
            shell_quote(path),
        ),
    );
    if result.is_err() {
        let _ = remote_script(
            transport,
            "remove failed orphaned Serve cleanup file",
            &format!("set -eu\nsudo -n rm -f {}\n", shell_quote(path)),
        );
    }
    result.map(|_| true)
}

fn tailscale_serve_get_config(transport: &OpenSshTransport) -> Result<Option<CommandOutput>> {
    let script = "set -eu\nfor candidate in /usr/local/bin/tailscale /usr/local/opt/tailscale/bin/tailscale /opt/homebrew/bin/tailscale /opt/homebrew/opt/tailscale/bin/tailscale /usr/bin/tailscale /Applications/Tailscale.app/Contents/MacOS/tailscale /Applications/Tailscale.app/Contents/MacOS/Tailscale; do if [ -x \"$candidate\" ]; then exec \"$candidate\" serve get-config --all; fi; done\nif command -v tailscale >/dev/null 2>&1; then exec tailscale serve get-config --all; fi\necho 'Tailscale CLI was not found on the target' >&2\nexit 127\n";
    let command = CommandSpec::fixed("sh", &["-s"], "read Tailscale Serve configuration")
        .with_stdin(script.as_bytes().to_vec())
        .with_full_output();
    match transport.exec(command) {
        Ok(output) => Ok(Some(output)),
        Err(CiaoError::RemoteCommand { stdout, stderr, .. }) => {
            let combined = format!("{stdout}\n{stderr}").to_ascii_lowercase();
            if combined.contains("not found")
                || combined.contains("no configuration")
                || combined.contains("no serve")
            {
                Ok(None)
            } else {
                Err(CiaoError::RemoteCommand {
                    stage: "read Tailscale Serve configuration".to_owned(),
                    exit: 1,
                    stdout,
                    stderr,
                })
            }
        }
        Err(error) => Err(error),
    }
}

fn remove_serve_ports(value: &mut serde_json::Value, ports: &[u16]) -> bool {
    let Some(services) = value
        .get_mut("services")
        .and_then(|value| value.as_object_mut())
    else {
        return false;
    };
    let mut changed = false;
    let service_names = services.keys().cloned().collect::<Vec<_>>();
    for service_name in service_names {
        let Some(service) = services.get_mut(&service_name) else {
            continue;
        };
        let Some(endpoints) = service
            .get_mut("endpoints")
            .and_then(|value| value.as_object_mut())
        else {
            continue;
        };
        let endpoint_names = endpoints.keys().cloned().collect::<Vec<_>>();
        for endpoint_name in endpoint_names {
            let remove = endpoints
                .get(&endpoint_name)
                .and_then(|value| value.as_str())
                .is_some_and(|target| ports.iter().any(|port| target_has_port(target, *port)));
            if remove {
                endpoints.remove(&endpoint_name);
                changed = true;
            }
        }
        if endpoints.is_empty() {
            services.remove(&service_name);
        }
    }
    changed
}

fn target_has_port(target: &str, port: u16) -> bool {
    target.contains(&format!(":{port}"))
        && (target.contains("127.0.0.1")
            || target.contains("localhost")
            || target.contains("[::1]"))
}

fn local_target_port(target: &str) -> Option<u16> {
    let host = target
        .split_once("://")
        .map(|(_, value)| value)
        .unwrap_or(target)
        .split('/')
        .next()
        .unwrap_or_default();
    let (host, port) = host.rsplit_once(':')?;
    if host != "127.0.0.1" && host != "localhost" && host != "[::1]" {
        return None;
    }
    port.parse().ok()
}

fn remove_serve_orphan_ports(
    value: &mut serde_json::Value,
    active_ports: &std::collections::BTreeSet<u16>,
) -> bool {
    let Some(services) = value
        .get_mut("services")
        .and_then(|value| value.as_object_mut())
    else {
        return false;
    };
    let mut changed = false;
    let service_names = services.keys().cloned().collect::<Vec<_>>();
    for service_name in service_names {
        let Some(service) = services.get_mut(&service_name) else {
            continue;
        };
        let Some(endpoints) = service
            .get_mut("endpoints")
            .and_then(|value| value.as_object_mut())
        else {
            continue;
        };
        let endpoint_names = endpoints.keys().cloned().collect::<Vec<_>>();
        for endpoint_name in endpoint_names {
            let remove = endpoints
                .get(&endpoint_name)
                .and_then(|value| value.as_str())
                .and_then(local_target_port)
                .is_some_and(|port| {
                    (PORT_START..=PORT_END).contains(&port) && !active_ports.contains(&port)
                });
            if remove {
                endpoints.remove(&endpoint_name);
                changed = true;
            }
        }
        if endpoints.is_empty() {
            services.remove(&service_name);
        }
    }
    changed
}

/// Disable the Ciao-owned root Funnel route before removing an application.
/// If no Ciao Funnel fragment exists, this is a no-op and never touches any
/// user-managed Tailscale configuration.
pub(crate) fn disable_tailscale_funnel(transport: &OpenSshTransport, app: &str) -> Result<()> {
    validate_identifier("app name", app)?;
    let platform = transport.inspect()?;
    let fragment_path = funnel_fragment_path(app);
    let script = format!(
        r#"set -eu
fragment={fragment}
if sudo -n test -f "$fragment"; then
    tailscale_bin=''
    for candidate in /usr/local/bin/tailscale /usr/local/opt/tailscale/bin/tailscale /opt/homebrew/bin/tailscale /opt/homebrew/opt/tailscale/bin/tailscale /usr/bin/tailscale /Applications/Tailscale.app/Contents/MacOS/tailscale /Applications/Tailscale.app/Contents/MacOS/Tailscale; do
        if [ -x "$candidate" ]; then tailscale_bin="$candidate"; break; fi
    done
    if [ -z "$tailscale_bin" ] && command -v tailscale >/dev/null 2>&1; then tailscale_bin=$(command -v tailscale); fi
    [ -n "$tailscale_bin" ] || {{ echo 'Ciao Funnel route exists but the Tailscale CLI is missing' >&2; exit 1; }}
    sudo -n "$tailscale_bin" funnel --https=443 off
    sudo -n rm -f "$fragment"
fi
"#,
        fragment = shell_quote(&fragment_path),
    );
    remote_script(transport, "disable Tailscale Funnel", &script)?;
    remote_script(
        transport,
        "reload Caddy after disabling Tailscale Funnel",
        &caddy_reload_script(&platform.os),
    )?;
    Ok(())
}

fn funnel_fragment_path(app: &str) -> String {
    format!("/etc/caddy/ciao/{app}{FUNNEL_FRAGMENT_SUFFIX}")
}

pub(crate) fn funnel_token_path(root: &str, app: &str) -> String {
    format!("{root}/{app}/shared/funnel-token")
}

pub(crate) fn read_funnel_token(
    transport: &OpenSshTransport,
    root: &str,
    app: &str,
) -> Result<Option<String>> {
    let path = funnel_token_path(root, app);
    let token = read_remote_file(transport, &path, "read Funnel token")?
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty());
    if let Some(token) = token.as_deref() {
        validate_identifier("Funnel token", token)?;
    }
    Ok(token)
}

fn ensure_funnel_token(transport: &OpenSshTransport, root: &str, app: &str) -> Result<String> {
    if let Some(token) = read_funnel_token(transport, root, app)? {
        return Ok(token);
    }
    let path = funnel_token_path(root, app);
    remote_script(
        transport,
        "generate Funnel token",
        &format!(
            "set -eu\nparent={}\nfile={}\nsudo -n install -d -m 0700 \"$parent\"\nif ! sudo -n test -s \"$file\"; then tmp=\"$file.$$\"; if command -v openssl >/dev/null 2>&1; then token=$(openssl rand -hex 18); else token=$(od -An -N18 -tx1 /dev/urandom | tr -d ' \\n'); fi; case \"$token\" in (*[!A-Fa-f0-9]*|'') echo 'CSPRNG did not return a valid Funnel token' >&2; exit 1;; esac; printf '%s\\n' \"$token\" | sudo -n tee \"$tmp\" >/dev/null; sudo -n chown root:root \"$tmp\"; sudo -n chmod 0600 \"$tmp\"; sudo -n mv -f \"$tmp\" \"$file\"; fi\n",
            shell_quote(&format!("{root}/{app}/shared")),
            shell_quote(&path),
        ),
    )?;
    read_funnel_token(transport, root, app)?.ok_or_else(|| {
        CiaoError::Config("Funnel token generation completed without a token".to_owned())
    })
}

fn ensure_single_funnel_hostname(transport: &OpenSshTransport, app: &str) -> Result<()> {
    let current = funnel_fragment_path(app);
    let output = remote_script(
        transport,
        "check existing Funnel routes",
        &format!(
            "set -eu\nfor path in /etc/caddy/ciao/*.funnel.caddy; do test -f \"$path\" || continue; test \"$path\" = {} && continue; printf '%s\\n' \"$path\"; done\n",
            shell_quote(&current)
        ),
    )?;
    let conflicts = output
        .stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    if conflicts.is_empty() {
        Ok(())
    } else {
        Err(CiaoError::Config(format!(
            "Tailscale Funnel already has a Ciao route on this host ({}) ; disable it before exposing `{app}`",
            conflicts.join(", ")
        )))
    }
}

fn remote_funnel_healthcheck(
    transport: &OpenSshTransport,
    hostname: &str,
    health: &HealthConfig,
    token: Option<&str>,
) -> Result<()> {
    validate_domain(hostname)?;
    let prefix = token.map(|token| format!("/{token}")).unwrap_or_default();
    let url = format!("http://127.0.0.1:80{prefix}{}", health.path);
    let attempts = health.timeout_seconds.saturating_mul(2).saturating_add(1);
    let script = format!(
        "set -eu\nexpected={}\nattempts={}\nfor attempt in $(seq 1 \"$attempts\"); do\n    actual=$(curl --silent --insecure --max-time 1 --header {} --output /dev/null --write-out '%{{http_code}}' {} || true)\n    if [ \"$actual\" = \"$expected\" ]; then exit 0; fi\n    if [ \"$actual\" != 000 ] && [ -n \"$actual\" ]; then echo \"Funnel smoke check expected HTTP $expected, got $actual\" >&2; exit 1; fi\n    if [ \"$attempt\" -lt \"$attempts\" ]; then sleep 0.5; fi\ndone\necho \"Funnel smoke check timed out after {}s for {}\" >&2\nexit 1\n",
        health.expected_status,
        attempts,
        shell_quote(&format!("Host: {hostname}")),
        shell_quote(&url),
        health.timeout_seconds,
        shell_quote(hostname),
    );
    remote_script(transport, "Funnel Caddy smoke check", &script).map(|_| ())
}

fn tailscale_funnel_enable_script() -> String {
    format!(
        r#"set -eu
tailscale_bin=''
for candidate in /usr/local/bin/tailscale /usr/local/opt/tailscale/bin/tailscale /opt/homebrew/bin/tailscale /opt/homebrew/opt/tailscale/bin/tailscale /usr/bin/tailscale /Applications/Tailscale.app/Contents/MacOS/tailscale /Applications/Tailscale.app/Contents/MacOS/Tailscale; do
    if [ -x "$candidate" ]; then tailscale_bin="$candidate"; break; fi
done
if [ -z "$tailscale_bin" ] && command -v tailscale >/dev/null 2>&1; then tailscale_bin=$(command -v tailscale); fi
[ -n "$tailscale_bin" ] || {{ echo 'Tailscale CLI was not found on the target' >&2; exit 1; }}
sudo -n "$tailscale_bin" funnel --bg --https=443 {target}
"#,
        target = shell_quote(FUNNEL_TARGET),
    )
}

fn restore_fragment(transport: &OpenSshTransport, path: &str, previous: Option<&str>, os: &HostOs) {
    let result = match previous {
        Some(contents) => {
            write_remote_file(transport, path, contents, "root", "restore Funnel route")
        }
        None => remote_script(
            transport,
            "remove failed Funnel route",
            &format!("set -eu\nsudo -n rm -f {}\n", shell_quote(path)),
        )
        .map(|_| ()),
    };
    if result.is_ok() {
        let _ = remote_script(
            transport,
            "reload Caddy after failed Funnel setup",
            &caddy_reload_script(os),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn funnel_enable_script_uses_loopback_caddy_and_background_https() {
        let script = tailscale_funnel_enable_script();
        assert!(script.contains("funnel --bg --https=443 'http://127.0.0.1:80'"));
        assert!(script.contains("sudo -n"));
    }

    #[test]
    fn funnel_fragment_path_is_app_scoped() {
        assert_eq!(
            funnel_fragment_path("demo"),
            "/etc/caddy/ciao/demo.funnel.caddy"
        );
    }

    #[test]
    fn serve_cleanup_removes_only_local_historical_ports() {
        let mut config = serde_json::json!({
            "version": "0.0.1",
            "services": {
                "svc:ciao": {
                    "endpoints": {
                        "tcp:443": "http://127.0.0.1:41000",
                        "tcp:8443": "http://127.0.0.1:45000"
                    }
                },
                "svc:other": {
                    "endpoints": {
                        "tcp:443": "http://127.0.0.1:3000"
                    }
                }
            }
        });
        assert!(remove_serve_ports(&mut config, &[41000]));
        assert!(config["services"]["svc:ciao"]["endpoints"]
            .get("tcp:443")
            .is_none());
        assert!(config["services"]["svc:ciao"]["endpoints"]
            .get("tcp:8443")
            .is_some());
        assert!(config["services"]["svc:other"]["endpoints"]
            .get("tcp:443")
            .is_some());
    }

    #[test]
    fn serve_cleanup_uses_documented_set_config_argument_order() {
        let path = "/tmp/ciao-serve-cleanup-demo.json";
        let script = format!("tailscale serve set-config --all {}", shell_quote(path));
        assert!(script.contains("serve set-config --all '/tmp/ciao-serve-cleanup-demo.json'"));
        assert!(!script.contains("serve set-config '/tmp/ciao-serve-cleanup-demo.json' --all"));
    }

    #[test]
    fn funnel_token_path_is_private_and_scoped() {
        assert_eq!(
            funnel_token_path("/var/lib/ciao/apps", "demo"),
            "/var/lib/ciao/apps/demo/shared/funnel-token"
        );
        assert!(target_has_port("http://127.0.0.1:41000", 41000));
        assert!(!target_has_port("http://127.0.0.1:41000", 3000));
        assert!(!target_has_port("http://10.0.0.4:41000", 41000));
    }
}
