//! Read-only host drift inspection.

use super::*;
use std::collections::BTreeSet;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditItem {
    pub path: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actual: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostAuditResult {
    pub host: String,
    pub drift_count: usize,
    pub items: Vec<AuditItem>,
    pub message: String,
}

pub fn host_audit(transport: &OpenSshTransport) -> Result<HostAuditResult> {
    let platform = transport.inspect()?;
    let root = host_app_root(&platform.os);
    let apps = list_apps(transport)?;
    let mut items = Vec::new();

    let caddyfile = read_caddyfile(transport, &platform.os)?;
    let caddy_path = caddyfile
        .as_ref()
        .map(|(path, _)| path.clone())
        .unwrap_or_else(|| "/etc/caddy/Caddyfile".to_owned());
    let caddy_contents = caddyfile
        .as_ref()
        .map(|(_, contents)| contents.as_str())
        .unwrap_or_default();
    push_presence(
        &mut items,
        &caddy_path,
        caddy_contents.contains("import /etc/caddy/ciao/*.caddy"),
        "import /etc/caddy/ciao/*.caddy",
    );
    audit_tailscale_exposures(transport, &mut items)?;

    let app_names = apps
        .iter()
        .map(|app| app.app.clone())
        .collect::<BTreeSet<_>>();
    audit_cloudflare_exposure(transport, &app_names, &mut items)?;
    for app in &apps {
        let Some(release) = read_current_release(transport, &root, &app.app)? else {
            items.push(AuditItem {
                path: format!("{root}/{}/current", app.app),
                status: "missing".to_owned(),
                expected: Some("active release symlink".to_owned()),
                actual: None,
            });
            continue;
        };
        let manifest = read_release_manifest(transport, &root, &app.app, &release)?;
        let local_domain = local_domain(&app.app)?;
        let local_expected =
            caddy_fragment_with_scheme(transport, &root, &app.app, &release, &local_domain, true)?;
        audit_remote_file(
            transport,
            &mut items,
            &format!("/etc/caddy/ciao/{}.local.caddy", app.app),
            &local_expected,
        )?;
        if let Some(domain) = read_existing_domain(transport, &app.app)? {
            let expected = caddy_fragment_with_scheme(
                transport,
                &root,
                &app.app,
                &release,
                &domain,
                existing_domain_is_plain_http(transport, &app.app)?,
            )?;
            audit_remote_file(
                transport,
                &mut items,
                &format!("/etc/caddy/ciao/{}.caddy", app.app),
                &expected,
            )?;
            if let Some(actual) = read_remote_file(
                transport,
                &format!("/etc/caddy/ciao/{}.caddy", app.app),
                "audit public Caddy route",
            )? {
                if let Some(port) = caddy_upstream_port(&actual) {
                    let status = match remote_port_is_listening(transport, port)? {
                        Some(true) => "ok",
                        Some(false) => "dead-port",
                        None => "unknown-port",
                    };
                    items.push(AuditItem {
                        path: format!("caddy/{}/upstream", app.app),
                        status: status.to_owned(),
                        expected: Some("active release port".to_owned()),
                        actual: Some(port.to_string()),
                    });
                }
            }
        }
        let funnel_path = format!("/etc/caddy/ciao/{}.funnel.caddy", app.app);
        if let Some(actual) = read_remote_file(transport, &funnel_path, "audit Funnel route")? {
            if let Ok(target) = tailscale_target(transport) {
                if let Some(hostname) = target.hostname {
                    let token = match manifest.funnel.auth {
                        FunnelAuth::Token => read_funnel_token(transport, &root, &app.app)?,
                        FunnelAuth::None => None,
                    };
                    let expected = funnel_caddy_fragment(
                        transport,
                        &root,
                        &app.app,
                        &release,
                        &hostname,
                        token.as_deref(),
                    )?;
                    let status = if manifest.funnel.auth == FunnelAuth::Token && token.is_none() {
                        "missing-token"
                    } else if actual == expected {
                        "ok"
                    } else {
                        "drift"
                    };
                    items.push(AuditItem {
                        path: funnel_path,
                        status: status.to_owned(),
                        expected: Some("managed Funnel route (redacted)".to_owned()),
                        actual: Some("managed Funnel route (redacted)".to_owned()),
                    });
                    if let Some(port) = caddy_upstream_port(&actual) {
                        let port_status = match remote_port_is_listening(transport, port)? {
                            Some(true) => "ok",
                            Some(false) => "dead-port",
                            None => "unknown-port",
                        };
                        items.push(AuditItem {
                            path: format!("tailscale/funnel/{}/upstream", app.app),
                            status: port_status.to_owned(),
                            expected: Some("active Caddy upstream port".to_owned()),
                            actual: Some(port.to_string()),
                        });
                    }
                    if !manifest.public && manifest.funnel.auth == FunnelAuth::None {
                        items.push(AuditItem {
                            path: format!("tailscale/funnel/{}/auth", app.app),
                            status: "public-without-auth".to_owned(),
                            expected: Some("token auth or [app] public = true".to_owned()),
                            actual: Some("Funnel auth = none".to_owned()),
                        });
                    }
                } else {
                    items.push(AuditItem {
                        path: funnel_path,
                        status: "ok".to_owned(),
                        expected: Some("managed Tailscale Funnel route".to_owned()),
                        actual: Some("present".to_owned()),
                    });
                }
            } else {
                items.push(AuditItem {
                    path: funnel_path,
                    status: "ok".to_owned(),
                    expected: Some("managed Tailscale Funnel route".to_owned()),
                    actual: Some("present".to_owned()),
                });
            }
        }
        if manifest.app_type == AppType::Service {
            match platform.os {
                HostOs::Linux => {
                    let stable =
                        format!("/etc/systemd/system/{}", service_unit_name(&app.app, false));
                    audit_remote_presence(transport, &mut items, &stable)?;
                    for slot in ['a', 'b'] {
                        let path = format!(
                            "/etc/systemd/system/{}",
                            slot_service_unit_name(&app.app, slot)?
                        );
                        audit_remote_presence(transport, &mut items, &path)?;
                    }
                }
                HostOs::MacOs => {
                    let path = format!("/Library/LaunchDaemons/dev.ciao.{}.plist", app.app);
                    audit_remote_presence(transport, &mut items, &path)?;
                }
                HostOs::Unknown(_) => {}
            }
        }
    }

    let sudo_user = ssh_login_user(&transport.target).unwrap_or_default();
    let sudo_expected = if sudo_user.is_empty() {
        None
    } else {
        Some(format!("{sudo_user} ALL=(ALL) NOPASSWD: ALL"))
    };
    if let Some(expected) = sudo_expected {
        let fragment =
            read_remote_file(transport, "/etc/sudoers.d/ciao", "read Ciao sudoers policy")?
                .unwrap_or_default();
        let main =
            read_remote_file(transport, "/etc/sudoers", "read sudoers policy")?.unwrap_or_default();
        let actual = format!("{fragment}{main}");
        items.push(AuditItem {
            path: "/etc/sudoers.d/ciao".to_owned(),
            status: if actual.lines().any(|line| line.trim() == expected) {
                "ok".to_owned()
            } else {
                "drift".to_owned()
            },
            expected: Some(expected),
            actual: Some(actual),
        });
    }

    for path in remote_caddy_fragments(transport)? {
        let name = path
            .trim_end_matches(".local.caddy")
            .trim_end_matches(".funnel.caddy")
            .trim_end_matches(".caddy")
            .rsplit('/')
            .next()
            .unwrap_or_default();
        if !app_names.contains(name) {
            items.push(AuditItem {
                path,
                status: "orphan".to_owned(),
                expected: Some("managed application route".to_owned()),
                actual: Some("unmanaged Caddy fragment".to_owned()),
            });
        }
    }
    if platform.os == HostOs::Linux {
        for path in remote_systemd_units(transport)? {
            let name = path
                .rsplit('/')
                .next()
                .unwrap_or_default()
                .trim_end_matches(".service")
                .strip_prefix("ciao-")
                .unwrap_or_default()
                .trim_end_matches("-slot-a")
                .trim_end_matches("-slot-b")
                .trim_end_matches("-candidate");
            if !app_names.contains(name) {
                items.push(AuditItem {
                    path,
                    status: "orphan".to_owned(),
                    expected: Some("managed Ciao application unit".to_owned()),
                    actual: Some("unmanaged systemd unit".to_owned()),
                });
            }
        }
    }

    let drift_count = items.iter().filter(|item| item.status != "ok").count();
    Ok(HostAuditResult {
        host: transport.target.clone(),
        drift_count,
        items,
        message: if drift_count == 0 {
            "✓ host audit: no drift detected".to_owned()
        } else {
            format!("⚠ host audit: {drift_count} drift item(s)")
        },
    })
}

fn audit_cloudflare_exposure(
    transport: &OpenSshTransport,
    app_names: &BTreeSet<String>,
    items: &mut Vec<AuditItem>,
) -> Result<()> {
    let Some(contents) = read_cloudflare_config(transport)? else {
        return Ok(());
    };
    let Some(config) = parse_cloudflare_config(&contents) else {
        items.push(AuditItem {
            path: "/etc/cloudflared/config.yml".to_owned(),
            status: "invalid".to_owned(),
            expected: Some("Ciao-managed Cloudflare ingress".to_owned()),
            actual: Some("unparseable ingress configuration".to_owned()),
        });
        return Ok(());
    };
    let owner = config
        .app
        .clone()
        .or_else(|| {
            config
                .tunnel_name
                .as_deref()
                .and_then(|name| name.strip_prefix("ciao-"))
                .map(str::to_owned)
        })
        .or_else(|| config.tunnel.strip_prefix("ciao-").map(str::to_owned));
    let owner_status = match owner.as_deref() {
        Some(app) if app_names.contains(app) => "ok",
        Some(_) => "orphan",
        None => "unmanaged",
    };
    items.push(AuditItem {
        path: "/etc/cloudflared/config.yml".to_owned(),
        status: owner_status.to_owned(),
        expected: Some("active Ciao Cloudflare ingress".to_owned()),
        actual: Some(format!("{} → localhost:{}", config.hostname, config.port)),
    });
    let hostname_status = if validate_domain(&config.hostname).is_err() {
        "invalid-hostname"
    } else {
        match remote_hostname_resolves(transport, &config.hostname)? {
            Some(true) => "ok",
            Some(false) => "unresolved-hostname",
            None => "unknown-hostname",
        }
    };
    items.push(AuditItem {
        path: "cloudflared/hostname".to_owned(),
        status: hostname_status.to_owned(),
        expected: Some("resolvable public hostname".to_owned()),
        actual: Some(config.hostname.clone()),
    });
    let port_status = match remote_port_is_listening(transport, config.port)? {
        Some(true) => "ok",
        Some(false) => "dead-port",
        None => "unknown-port",
    };
    items.push(AuditItem {
        path: "cloudflared/upstream".to_owned(),
        status: port_status.to_owned(),
        expected: Some("active release port".to_owned()),
        actual: Some(config.port.to_string()),
    });
    if let Some(app) = owner.as_deref() {
        if !app_names.contains(app) {
            return Ok(());
        }
        let platform = transport.inspect()?;
        let root = host_app_root(&platform.os);
        if let Some(release) = read_current_release(transport, &root, app)? {
            let manifest = read_release_manifest(transport, &root, app, &release)?;
            let expected_port = manifest.port.unwrap_or(80);
            if expected_port != config.port {
                items.push(AuditItem {
                    path: format!("cloudflared/{app}/upstream"),
                    status: "drift".to_owned(),
                    expected: Some(expected_port.to_string()),
                    actual: Some(config.port.to_string()),
                });
            }
        }
    }
    Ok(())
}

fn audit_tailscale_exposures(
    transport: &OpenSshTransport,
    items: &mut Vec<AuditItem>,
) -> Result<()> {
    for kind in ["funnel", "serve"] {
        let status = match tailscale_exposure_status(transport, kind) {
            Ok(status) => status,
            Err(error) => {
                items.push(AuditItem {
                    path: format!("tailscale/{kind}"),
                    status: "unavailable".to_owned(),
                    expected: Some(format!("readable Tailscale {kind} status")),
                    actual: Some(error.to_string()),
                });
                continue;
            }
        };
        let Some(status) = status else {
            continue;
        };
        let mut targets = Vec::new();
        collect_exposure_targets(&status, &mut targets);
        targets.sort();
        targets.dedup();
        items.push(AuditItem {
            path: format!("tailscale/{kind}"),
            status: "ok".to_owned(),
            expected: Some(format!("managed or declared {kind} rules")),
            actual: Some(format!("{} rule target(s)", targets.len())),
        });
        for (index, target) in targets.iter().enumerate() {
            let status = if let Some(port) = local_target_port(target) {
                match remote_port_is_listening(transport, port)? {
                    Some(true) => "ok",
                    Some(false) => "dead-port",
                    None => "unknown-port",
                }
            } else if let Some(hostname) = target_hostname(target) {
                match remote_hostname_resolves(transport, &hostname)? {
                    Some(true) => "ok",
                    Some(false) => "unresolved-hostname",
                    None => "unknown-hostname",
                }
            } else {
                "ok"
            };
            items.push(AuditItem {
                path: format!("tailscale/{kind}/{index}"),
                status: status.to_owned(),
                expected: Some("active local target or resolvable hostname".to_owned()),
                actual: Some(target.clone()),
            });
        }
    }
    Ok(())
}

fn tailscale_exposure_status(
    transport: &OpenSshTransport,
    kind: &str,
) -> Result<Option<serde_json::Value>> {
    let command = CommandSpec::fixed("sh", &["-s"], format!("read Tailscale {kind} status"))
        .with_stdin(
            format!(
                "set -eu\nfor candidate in /usr/local/bin/tailscale /usr/local/opt/tailscale/bin/tailscale /opt/homebrew/bin/tailscale /opt/homebrew/opt/tailscale/bin/tailscale /usr/bin/tailscale /Applications/Tailscale.app/Contents/MacOS/tailscale /Applications/Tailscale.app/Contents/MacOS/Tailscale; do\n    if [ -x \"$candidate\" ]; then exec \"$candidate\" {kind} status --json; fi\ndone\nif command -v tailscale >/dev/null 2>&1; then exec tailscale {kind} status --json; fi\necho 'Tailscale CLI was not found on the target' >&2\nexit 127\n"
            )
            .into_bytes(),
        )
        .with_full_output();
    let output = match transport.exec(command) {
        Ok(output) => output,
        Err(CiaoError::RemoteCommand { stdout, stderr, .. }) => {
            if let Some(value) = tailscale_status_value(&stdout, &stderr) {
                return Ok(Some(value));
            }
            let combined = format!("{stdout}\n{stderr}").to_ascii_lowercase();
            if combined.contains("not found")
                || combined.contains("no configuration")
                || combined.contains("no serve")
                || combined.contains("no funnel")
            {
                return Ok(None);
            }
            return Err(CiaoError::RemoteCommand {
                stage: format!("read Tailscale {kind} status"),
                exit: 1,
                stdout,
                stderr,
            });
        }
        Err(error) => return Err(error),
    };
    Ok(tailscale_status_value(&output.stdout, &output.stderr))
}

fn collect_exposure_targets(value: &serde_json::Value, targets: &mut Vec<String>) {
    match value {
        serde_json::Value::String(value)
            if value.contains("://")
                || value.contains("127.0.0.1:")
                || value.contains("localhost:")
                || value.contains(".ts.net") =>
        {
            targets.push(value.clone());
        }
        serde_json::Value::Array(values) => {
            for value in values {
                collect_exposure_targets(value, targets);
            }
        }
        serde_json::Value::Object(values) => {
            for value in values.values() {
                collect_exposure_targets(value, targets);
            }
        }
        _ => {}
    }
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

fn target_hostname(target: &str) -> Option<String> {
    let host = target
        .split_once("://")
        .map(|(_, value)| value)
        .unwrap_or(target)
        .split('/')
        .next()
        .unwrap_or_default()
        .trim_matches('[');
    let hostname = host.split(':').next().unwrap_or_default();
    if hostname.is_empty() || hostname == "127.0.0.1" || hostname == "localhost" {
        None
    } else if hostname.contains('.') {
        Some(hostname.trim_end_matches(']').to_owned())
    } else {
        None
    }
}

fn caddy_upstream_port(fragment: &str) -> Option<u16> {
    let value = fragment.split("127.0.0.1:").nth(1)?;
    value
        .chars()
        .take_while(|character| character.is_ascii_digit())
        .collect::<String>()
        .parse()
        .ok()
}

fn remote_port_is_listening(transport: &OpenSshTransport, port: u16) -> Result<Option<bool>> {
    let output = remote_script(
        transport,
        "audit exposed port",
        &format!(
            "set -eu\nport={}\nif command -v ss >/dev/null 2>&1; then listeners=$(ss -ltnH 2>/dev/null | awk '{{print $4}}'); elif command -v lsof >/dev/null 2>&1; then listeners=$(lsof -nP -iTCP -sTCP:LISTEN 2>/dev/null | awk 'NR > 1 {{print $9}}'); elif command -v netstat >/dev/null 2>&1; then listeners=$(netstat -ltn 2>/dev/null | awk 'NR > 2 {{print $4}}'); else printf 'unknown\\n'; exit 0; fi\nif printf '%s\\n' \"$listeners\" | grep -Eq \"([.:])$port$\"; then printf 'yes\\n'; else printf 'no\\n'; fi\n",
            port
        ),
    )?;
    Ok(match output.stdout.trim() {
        "yes" => Some(true),
        "no" => Some(false),
        _ => None,
    })
}

fn remote_hostname_resolves(transport: &OpenSshTransport, hostname: &str) -> Result<Option<bool>> {
    validate_domain(hostname)?;
    let output = remote_script(
        transport,
        "audit exposed hostname",
        &format!(
            "set -eu\nname={}\nif command -v getent >/dev/null 2>&1; then getent ahosts \"$name\" >/dev/null 2>&1 && printf 'yes\\n' || printf 'no\\n'; elif command -v dscacheutil >/dev/null 2>&1; then dscacheutil -q host -a name \"$name\" | grep -q '^ip_address:' && printf 'yes\\n' || printf 'no\\n'; else printf 'unknown\\n'; fi\n",
            shell_quote(hostname)
        ),
    )?;
    Ok(match output.stdout.trim() {
        "yes" => Some(true),
        "no" => Some(false),
        _ => None,
    })
}

fn push_presence(items: &mut Vec<AuditItem>, path: &str, present: bool, expected: &str) {
    items.push(AuditItem {
        path: path.to_owned(),
        status: if present { "ok" } else { "drift" }.to_owned(),
        expected: Some(expected.to_owned()),
        actual: Some(if present { expected } else { "missing" }.to_owned()),
    });
}

fn audit_remote_presence(
    transport: &OpenSshTransport,
    items: &mut Vec<AuditItem>,
    path: &str,
) -> Result<()> {
    let actual = read_remote_file(transport, path, "audit remote file")?;
    items.push(AuditItem {
        path: path.to_owned(),
        status: if actual.is_some() { "ok" } else { "missing" }.to_owned(),
        expected: Some("generated Ciao service definition".to_owned()),
        actual: actual.map(|_| "present".to_owned()),
    });
    Ok(())
}

fn audit_remote_file(
    transport: &OpenSshTransport,
    items: &mut Vec<AuditItem>,
    path: &str,
    expected: &str,
) -> Result<()> {
    let actual = read_remote_file(transport, path, "audit remote file")?;
    let status = match actual.as_deref() {
        None => "missing",
        Some(actual) if actual == expected => "ok",
        Some(_) => "drift",
    };
    items.push(AuditItem {
        path: path.to_owned(),
        status: status.to_owned(),
        expected: Some(expected.to_owned()),
        actual,
    });
    Ok(())
}

fn read_remote_file(
    transport: &OpenSshTransport,
    path: &str,
    stage: &str,
) -> Result<Option<String>> {
    let command = CommandSpec::fixed("sh", &["-s"], stage)
        .with_stdin(
            format!(
                "set -eu\nif sudo -n test -f {}; then sudo -n cat {}; else printf '__CIAO_MISSING__'; fi\n",
                shell_quote(path),
                shell_quote(path)
            )
            .into_bytes(),
        )
        .with_full_output();
    let output = transport.exec(command)?.ensure_success(stage)?;
    if output.stdout == "__CIAO_MISSING__" {
        Ok(None)
    } else {
        Ok(Some(output.stdout))
    }
}

fn read_caddyfile(transport: &OpenSshTransport, os: &HostOs) -> Result<Option<(String, String)>> {
    let script = match os {
        HostOs::MacOs => {
            "set -eu\nfor path in /opt/homebrew/etc/Caddyfile /usr/local/etc/Caddyfile; do if test -f \"$path\"; then printf '__CIAO_PATH__%s\\n' \"$path\"; cat \"$path\"; exit 0; fi; done\nprintf '__CIAO_MISSING__'\n"
        }
        _ => "set -eu\npath=/etc/caddy/Caddyfile\nif sudo -n test -f \"$path\"; then printf '__CIAO_PATH__%s\\n' \"$path\"; sudo -n cat \"$path\"; else printf '__CIAO_MISSING__'; fi\n",
    };
    let output = transport
        .exec(
            CommandSpec::fixed("sh", &["-s"], "audit Caddyfile")
                .with_stdin(script.as_bytes().to_vec())
                .with_full_output(),
        )?
        .ensure_success("audit Caddyfile")?;
    let mut lines = output.stdout.splitn(2, '\n');
    let first = lines.next().unwrap_or_default();
    if first == "__CIAO_MISSING__" {
        return Ok(None);
    }
    let path = first.strip_prefix("__CIAO_PATH__").unwrap_or(first);
    Ok(Some((
        path.to_owned(),
        lines.next().unwrap_or_default().to_owned(),
    )))
}

fn remote_caddy_fragments(transport: &OpenSshTransport) -> Result<Vec<String>> {
    let output = remote_script(
        transport,
        "list Caddy fragments",
        "set -eu\nfor path in /etc/caddy/ciao/*.caddy; do test -f \"$path\" && printf '%s\\n' \"$path\"; done\n",
    )?;
    Ok(output
        .stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect())
}

fn remote_systemd_units(transport: &OpenSshTransport) -> Result<Vec<String>> {
    let output = remote_script(
        transport,
        "list Ciao systemd units",
        "set -eu\nfor path in /etc/systemd/system/ciao-*.service; do test -f \"$path\" && printf '%s\\n' \"$path\"; done\n",
    )?;
    Ok(output
        .stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect())
}
