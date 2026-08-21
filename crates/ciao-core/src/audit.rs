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

    let app_names = apps
        .iter()
        .map(|app| app.app.clone())
        .collect::<BTreeSet<_>>();
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
        }
        let funnel_path = format!("/etc/caddy/ciao/{}.funnel.caddy", app.app);
        if let Some(actual) = read_remote_file(transport, &funnel_path, "audit Funnel route")? {
            if let Ok(target) = tailscale_target(transport) {
                if let Some(hostname) = target.hostname {
                    let expected =
                        funnel_caddy_fragment(transport, &root, &app.app, &release, &hostname)?;
                    let status = if actual == expected { "ok" } else { "drift" };
                    items.push(AuditItem {
                        path: funnel_path,
                        status: status.to_owned(),
                        expected: Some(expected),
                        actual: Some(actual),
                    });
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
