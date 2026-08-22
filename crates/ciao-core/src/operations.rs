//! Application status, logs, lifecycle and rollback operations.

use super::*;

pub fn app_status<T: RemoteHost + ?Sized>(transport: &T, app: &str) -> Result<StatusResult> {
    validate_identifier("app name", app)?;
    let platform = transport.inspect()?;
    let root = host_app_root(&platform.os);
    let current_release = read_current_release(transport, &root, app)?;
    let active_service = active_service(transport, &platform.os, &root, app)?;
    let release = active_service
        .as_ref()
        .and_then(|service| service.release.clone())
        .or_else(|| current_release.clone());
    let manifest = release
        .as_deref()
        .map(|release| read_release_manifest(transport, &root, app, release))
        .transpose()?;
    let status = match manifest.as_ref().map(|manifest| &manifest.app_type) {
        Some(AppType::Static) => "active".to_owned(),
        Some(AppType::Service) | None => match &platform.os {
            HostOs::Linux | HostOs::MacOs => {
                let unit = active_service
                    .as_ref()
                    .map(|service| service.unit.clone())
                    .or_else(|| {
                        read_active_slot(transport, &root, app)
                            .ok()
                            .flatten()
                            .and_then(|slot| slot_service_unit_name(app, slot).ok())
                    })
                    .unwrap_or_else(|| service_unit_name(app, false));
                service_state(transport, &platform.os, &unit)?
            }
            HostOs::Unknown(_) => "unsupported".to_owned(),
        },
    };
    let cloudflare = cloudflare_tunnel_status(transport, app)?;
    let configured_domain = read_existing_domain(transport, app)?;
    let mut message = match cloudflare.as_ref() {
        Some(tunnel) => format!(
            "{app}: {status}\n  Cloudflare: https://{} (tunnel {}, port {}, connector {})",
            tunnel.hostname, tunnel.tunnel, tunnel.port, tunnel.connector
        ),
        None => format!("{app}: {status}"),
    };
    if current_release.as_deref() != release.as_deref() {
        if let (Some(current), Some(active)) = (current_release.as_deref(), release.as_deref()) {
            message.push_str(&format!(
                "\n  release drift: current points to {current}, active service serves {active}"
            ));
        }
    }
    Ok(StatusResult {
        app: app.to_owned(),
        status: status.clone(),
        release,
        port: manifest.as_ref().and_then(|manifest| manifest.port),
        app_type: manifest.map(|manifest| manifest.app_type),
        service_manager: platform.os.service_manager_name().to_owned(),
        domain: cloudflare
            .as_ref()
            .map(|tunnel| tunnel.hostname.clone())
            .or(configured_domain),
        cloudflare,
        message,
    })
}

pub fn list_apps<T: RemoteHost + ?Sized>(transport: &T) -> Result<Vec<StatusResult>> {
    let platform = transport.inspect()?;
    let root = host_app_root(&platform.os);
    let output = remote_script(
        transport,
        "list applications",
        &format!(
            "set -eu\nif sudo -n test -d {}; then for path in {}/*; do if sudo -n test -d \"$path\"; then basename \"$path\"; fi; done; fi\n",
            shell_quote(&root),
            shell_quote(&root)
        ),
    )?;
    let mut apps = Vec::new();
    for app in output
        .stdout
        .lines()
        .map(str::trim)
        .filter(|app| !app.is_empty())
    {
        validate_identifier("app name", app)?;
        apps.push(app_status(transport, app)?);
    }
    apps.sort_by(|left, right| left.app.cmp(&right.app));
    Ok(apps)
}

/// Permanently remove a managed application, its services, Caddy routes and
/// immutable releases. This is intentionally explicit and cannot be inferred
/// from a failed deploy.
pub fn remove_app(transport: &OpenSshTransport, app: &str) -> Result<OperationResult> {
    validate_identifier("app name", app)?;
    let platform = transport.inspect()?;
    let root = host_app_root(&platform.os);
    let current = read_current_release(transport, &root, app)?;
    let manifest =
        effective_release_for_app(transport, &platform.os, &root, app, current.as_deref())?
            .map(|release| read_release_manifest(transport, &root, app, &release))
            .transpose()?;
    let serve_ports = list_releases(transport, app)?
        .into_iter()
        .filter_map(|release| release.port)
        .collect::<Vec<_>>();
    cleanup_tailscale_serve_for_ports(transport, app, &serve_ports)?;
    remove_cloudflare_tunnel_if_owned(transport, app)?;
    if !manifest
        .as_ref()
        .is_some_and(|manifest| manifest.app_type == AppType::Static)
    {
        let _ = service_action(
            transport,
            &platform.os,
            &service_unit_name(app, false),
            LifecycleAction::Stop,
        );
        let _ = remove_service(transport, &platform.os, &service_unit_name(app, false));
        if platform.os == HostOs::Linux {
            for slot in ['a', 'b'] {
                if let Ok(unit) = slot_service_unit_name(app, slot) {
                    let _ = remove_service(transport, &platform.os, &unit);
                }
            }
        }
    }
    disable_tailscale_funnel(transport, app)?;
    let caddy = format!(
        "set -eu\nsudo -n rm -f /etc/caddy/ciao/{app}.caddy /etc/caddy/ciao/{app}.local.caddy /etc/caddy/ciao/{app}.funnel.caddy\n",
        app = shell_quote(app)
    );
    remote_script(transport, "remove application Caddy routes", &caddy)?;
    remote_script(
        transport,
        "reload Caddy",
        &caddy_reload_script(&platform.os),
    )?;
    remote_script(
        transport,
        "remove application data",
        &format!(
            "set -eu\nsudo -n rm -rf {}\n",
            shell_quote(&format!("{root}/{app}"))
        ),
    )?;
    Ok(OperationResult {
        app: app.to_owned(),
        action: "remove".to_owned(),
        changed: true,
        message: format!("✓ removed {app}"),
    })
}

pub fn list_releases<T: RemoteHost + ?Sized>(transport: &T, app: &str) -> Result<Vec<ReleaseInfo>> {
    validate_identifier("app name", app)?;
    let platform = transport.inspect()?;
    let root = host_app_root(&platform.os);
    let current = read_current_release(transport, &root, app)?;
    let active_release =
        effective_release_for_app(transport, &platform.os, &root, app, current.as_deref())?;
    let output = remote_script(
        transport,
        "list releases",
        &format!(
            "set -eu\nif sudo -n test -d {root}/{app}/releases; then for path in {root}/{app}/releases/*; do if sudo -n test -d \"$path\"; then basename \"$path\"; fi; done; fi\n",
            root = shell_quote(&root),
            app = shell_quote(app)
        ),
    )?;
    let mut releases = Vec::new();
    for release in output
        .stdout
        .lines()
        .map(str::trim)
        .filter(|release| !release.is_empty())
    {
        validate_identifier("release", release)?;
        let manifest = read_release_manifest(transport, &root, app, release)?;
        releases.push(ReleaseInfo {
            app: app.to_owned(),
            release: release.to_owned(),
            active: active_release.as_deref() == Some(release),
            runtime: manifest.runtime,
            app_type: manifest.app_type,
            port: manifest.port,
            created_at_unix: manifest.created_at_unix,
        });
    }
    releases.sort_by(|left, right| right.release.cmp(&left.release));
    Ok(releases)
}

pub fn app_logs<T: RemoteHost + ?Sized>(
    transport: &T,
    app: &str,
    follow: bool,
    since: Option<&str>,
) -> Result<LogsResult> {
    validate_identifier("app name", app)?;
    let platform = transport.inspect()?;
    if follow {
        return Err(CiaoError::Config(
            "`logs --follow` is not available over synchronous SSH; omit it for a bounded snapshot"
                .to_owned(),
        ));
    }
    let root = host_app_root(&platform.os);
    let current = read_current_release(transport, &root, app)?;
    let active = active_service(transport, &platform.os, &root, app)?;
    let release = active
        .as_ref()
        .and_then(|service| service.release.clone())
        .or(effective_release_for_app(
            transport,
            &platform.os,
            &root,
            app,
            current.as_deref(),
        )?);
    let manifest = if let Some(release) = release.as_deref() {
        let manifest = read_release_manifest(transport, &root, app, release)?;
        if manifest.app_type == AppType::Static {
            return Err(CiaoError::Config(format!(
                "app `{app}` is static and has no service logs"
            )));
        }
        Some(manifest)
    } else {
        None
    };
    let mut pid = None;
    let mut last_event_timestamp = None;
    let result = match platform.os {
        HostOs::Linux => {
            let unit = if let Some(active) = active.as_ref() {
                active.unit.clone()
            } else if let Some(slot) = read_active_slot(transport, &root, app)? {
                slot_service_unit_name(app, slot)?
            } else {
                service_unit_name(app, false)
            };
            pid = read_systemd_pid(transport, &unit)?;
            last_event_timestamp = read_systemd_last_event(transport, &unit, since)?;
            let command = CommandSpec {
                program: "sudo".to_owned(),
                args: systemd_log_command_args(&unit, since)?,
                stdin: None,
                stage: "read logs".to_owned(),
                full_output: true,
            };
            transport.exec(command.clone())?
        }
        HostOs::MacOs => {
            if let Some(since) = since {
                validate_since(since)?;
                return Err(CiaoError::Config(
                    "`logs --since` is not available for macOS file-backed logs; omit it for a bounded snapshot".to_owned(),
                ));
            }
            let command = CommandSpec::fixed("sh", &["-s"], "read logs").with_stdin(
                format!(
                    "set -eu\nstdout=/Library/Ciao/logs/{app}.out\nstderr=/Library/Ciao/logs/{app}.err\nif test -f \"$stdout\"; then tail -n 200 \"$stdout\"; fi\nif test -f \"$stderr\"; then tail -n 200 \"$stderr\"; fi\n",
                    app = shell_quote(app),
                )
                .into_bytes(),
            );
            transport.exec(command.with_full_output())?
        }
        HostOs::Unknown(_) => {
            return Err(CiaoError::Config("unsupported host OS for logs".to_owned()))
        }
    };
    Ok(LogsResult {
        app: app.to_owned(),
        logs: result.stdout,
        release,
        pid,
        port: manifest.and_then(|manifest| manifest.port),
        last_event_timestamp,
        message: format!("logs for {app}"),
    })
}

fn systemd_log_command_args(unit: &str, since: Option<&str>) -> Result<Vec<String>> {
    validate_service_unit(unit)?;
    let mut args = vec![
        "-n".to_owned(),
        "journalctl".to_owned(),
        "-u".to_owned(),
        unit.to_owned(),
        "-n".to_owned(),
        "200".to_owned(),
        "--no-pager".to_owned(),
        "--output=short-iso".to_owned(),
    ];
    if let Some(since) = since {
        validate_since(since)?;
        args.extend(["--since".to_owned(), since.to_owned()]);
    }
    Ok(args)
}

fn read_systemd_pid<T: RemoteHost + ?Sized>(transport: &T, unit: &str) -> Result<Option<u32>> {
    validate_service_unit(unit)?;
    let output = remote_script(
        transport,
        "read service PID",
        &format!(
            "set -eu\nsudo -n systemctl show --property=MainPID --value {} 2>/dev/null || true\n",
            shell_quote(unit)
        ),
    )?;
    Ok(output
        .stdout
        .trim()
        .parse::<u32>()
        .ok()
        .filter(|pid| *pid > 0))
}

fn read_systemd_last_event<T: RemoteHost + ?Sized>(
    transport: &T,
    unit: &str,
    since: Option<&str>,
) -> Result<Option<String>> {
    let mut args = systemd_log_command_args(unit, since)?;
    let position = args
        .iter()
        .position(|arg| arg == "200")
        .ok_or_else(|| CiaoError::Config("invalid systemd log arguments".to_owned()))?;
    args[position] = "1".to_owned();
    let sudo_args = args;
    let output = transport.exec(CommandSpec {
        program: "sudo".to_owned(),
        args: sudo_args,
        stdin: None,
        stage: "read latest log timestamp".to_owned(),
        full_output: true,
    })?;
    Ok(output
        .stdout
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .and_then(|line| line.split_whitespace().next())
        .map(str::to_owned))
}

/// Follow logs through one interactive SSH session. The synchronous transport
/// intentionally rejects `--follow` because it cannot keep a live TTY stream.
pub fn follow_app_logs(transport: &OpenSshTransport, app: &str, since: Option<&str>) -> Result<()> {
    validate_identifier("app name", app)?;
    let platform = transport.inspect()?;
    let script = match platform.os {
        HostOs::Linux => {
            let since = since
                .map(|value| {
                    validate_since(value)?;
                    Ok::<_, CiaoError>(format!(" --since {}", shell_quote(value)))
                })
                .transpose()?
                .unwrap_or_default();
            let root = host_app_root(&platform.os);
            let unit = if let Some(active) = active_service(transport, &platform.os, &root, app)? {
                active.unit
            } else if let Some(slot) = read_active_slot(transport, &root, app)? {
                slot_service_unit_name(app, slot)?
            } else {
                service_unit_name(app, false)
            };
            format!(
                "set -eu\nexec sudo -n journalctl -u {} -n 200 -f --no-pager --output=short-iso{}\n",
                shell_quote(&unit),
                since
            )
        }
        HostOs::MacOs => {
            if since.is_some() {
                return Err(CiaoError::Config(
                    "`logs --since` is not available for macOS file-backed logs".to_owned(),
                ));
            }
            format!(
                "set -eu\nexec sudo -n sh -c 'touch /Library/Ciao/logs/{0}.out /Library/Ciao/logs/{0}.err; tail -F /Library/Ciao/logs/{0}.out /Library/Ciao/logs/{0}.err'\n",
                shell_quote(app)
            )
        }
        HostOs::Unknown(_) => {
            return Err(CiaoError::Config("unsupported host OS for logs".to_owned()))
        }
    };
    run_interactive_ssh_stream(transport, "follow logs", &script)
}

pub fn lifecycle_action<T: RemoteHost + ?Sized>(
    transport: &T,
    app: &str,
    action: LifecycleAction,
) -> Result<OperationResult> {
    validate_identifier("app name", app)?;
    let platform = transport.inspect()?;
    let root = host_app_root(&platform.os);
    let current = read_current_release(transport, &root, app)?;
    if let Some(release) =
        effective_release_for_app(transport, &platform.os, &root, app, current.as_deref())?
    {
        let manifest = read_release_manifest(transport, &root, app, &release)?;
        if manifest.app_type == AppType::Static {
            return Err(CiaoError::Config(format!(
                "app `{app}` is static and has no service lifecycle"
            )));
        }
    }
    let unit = if let Some(active) = active_service(transport, &platform.os, &root, app)? {
        active.unit
    } else if let Some(slot) = read_active_slot(transport, &root, app)? {
        slot_service_unit_name(app, slot)?
    } else {
        service_unit_name(app, false)
    };
    service_action(transport, &platform.os, &unit, action)?;
    Ok(OperationResult {
        app: app.to_owned(),
        action: action.as_str().to_owned(),
        changed: true,
        message: format!("✓ {action:?} {app}"),
    })
}

pub fn rollback(transport: &OpenSshTransport, app: &str) -> Result<OperationResult> {
    rollback_to(transport, app, None)
}

pub fn rollback_to(
    transport: &OpenSshTransport,
    app: &str,
    target_release: Option<&str>,
) -> Result<OperationResult> {
    validate_identifier("app name", app)?;
    let platform = transport.inspect()?;
    let root = host_app_root(&platform.os);
    let current_symlink = read_current_release(transport, &root, app)?;
    let active_service_state = active_service(transport, &platform.os, &root, app)?;
    let current = active_service_state
        .as_ref()
        .and_then(|service| service.release.clone())
        .or(current_symlink)
        .ok_or_else(|| CiaoError::Config(format!("app `{app}` has no active release")))?;
    let previous = match target_release {
        Some(target) => {
            validate_identifier("release", target)?;
            if target == current {
                return Err(CiaoError::Config(
                    "cannot roll back to the active release".to_owned(),
                ));
            }
            let releases = list_releases(transport, app)?;
            if !releases.iter().any(|release| release.release == target) {
                return Err(CiaoError::Config(format!(
                    "release `{target}` does not exist for app `{app}`"
                )));
            }
            target.to_owned()
        }
        None => previous_release(transport, &root, app, &current)?.ok_or_else(|| {
            CiaoError::Config(format!(
                "app `{app}` has no previous release to roll back to"
            ))
        })?,
    };
    let current_manifest = read_release_manifest(transport, &root, app, &current)?;
    let manifest = read_release_manifest(transport, &root, app, &previous)?;
    let active_slot = if platform.os == HostOs::Linux {
        let service_slot = active_service_state
            .as_ref()
            .and_then(|service| active_slot_from_unit(&service.unit));
        match service_slot {
            Some(slot) => Some(slot),
            None => read_active_slot(transport, &root, app)?,
        }
    } else {
        None
    };
    let rollback_slot = if manifest.port_explicit {
        None
    } else {
        active_slot.map(opposite_slot).transpose()?
    };
    let previous_path = format!("{root}/{app}/releases/{previous}");
    let current_path = format!("{root}/{app}/releases/{current}");
    let retained_domain = read_existing_domain(transport, app)?;
    let activation = (|| {
        remote_script(
            transport,
            "rollback activation",
            &switch_current_script(&platform.os, &root, app, &previous_path),
        )?;
        if manifest.app_type == AppType::Service {
            if let Some(slot) = rollback_slot {
                let user = service_user(transport, &platform.os, app)?;
                let unit = slot_service_unit_name(app, slot)?;
                install_service(
                    transport,
                    &platform.os,
                    &unit,
                    &user,
                    &previous_path,
                    &format!("{root}/{app}/shared/env"),
                    false,
                    "./start",
                )?;
                enable_service(transport, &platform.os, &unit)?;
                service_action(transport, &platform.os, &unit, LifecycleAction::Start)?;
                remote_healthcheck(
                    transport,
                    manifest.port.ok_or_else(|| {
                        CiaoError::Config("rollback release has no port".to_owned())
                    })?,
                    &manifest.health,
                )?;
            } else {
                if manifest.port_explicit && platform.os == HostOs::Linux {
                    for slot in ['a', 'b'] {
                        if let Ok(slot_unit) = slot_service_unit_name(app, slot) {
                            let _ = service_action(
                                transport,
                                &platform.os,
                                &slot_unit,
                                LifecycleAction::Stop,
                            );
                            let _ = disable_service(transport, &platform.os, &slot_unit);
                        }
                    }
                    remote_script(
                        transport,
                        "clear active service slot",
                        &format!(
                            "set -eu\nsudo -n rm -f {}\n",
                            shell_quote(&active_slot_path(&root, app))
                        ),
                    )?;
                }
                let unit = service_unit_name(app, false);
                let user = service_user(transport, &platform.os, app)?;
                install_service(
                    transport,
                    &platform.os,
                    &unit,
                    &user,
                    &previous_path,
                    &format!("{root}/{app}/shared/env"),
                    false,
                    "./start",
                )?;
                enable_service(transport, &platform.os, &unit)?;
                service_action(transport, &platform.os, &unit, LifecycleAction::Restart)?;
                remote_healthcheck(
                    transport,
                    manifest.port.ok_or_else(|| {
                        CiaoError::Config("rollback release has no port".to_owned())
                    })?,
                    &manifest.health,
                )?;
            }
            if let Some(slot) = active_slot {
                if let Some(rollback_slot) = rollback_slot {
                    let old_unit = slot_service_unit_name(app, slot)?;
                    service_action(transport, &platform.os, &old_unit, LifecycleAction::Stop)?;
                    disable_service(transport, &platform.os, &old_unit)?;
                    write_active_slot(transport, &root, app, rollback_slot)?;
                }
            }
        } else if current_manifest.app_type == AppType::Service {
            let old_unit = match active_slot {
                Some(slot) => slot_service_unit_name(app, slot)?,
                None => service_unit_name(app, false),
            };
            let _ = service_action(transport, &platform.os, &old_unit, LifecycleAction::Stop);
            let _ = disable_service(transport, &platform.os, &old_unit);
            remote_script(
                transport,
                "clear active service slot",
                &format!(
                    "set -eu\nsudo -n rm -f {}\n",
                    shell_quote(&active_slot_path(&root, app))
                ),
            )?;
        }
        if let Some(domain) = retained_domain.as_deref() {
            if existing_domain_is_plain_http(transport, app)? {
                configure_domain_for_cloudflare(transport, app, domain)?;
            } else {
                add_domain(transport, app, domain)?;
            }
        }
        configure_remote_ciao_domain(transport, app)?;
        sync_tailscale_funnel_route_if_present(transport, app)?;
        sync_cloudflare_tunnel_if_present(transport, app)?;
        reconcile_current_to_active(transport, &platform.os, &root, app)?;
        Ok::<(), CiaoError>(())
    })();
    if let Err(error) = activation {
        let restore = (|| {
            remote_script(
                transport,
                "restore active release after rollback failure",
                &switch_current_script(&platform.os, &root, app, &current_path),
            )?;
            if current_manifest.app_type == AppType::Service {
                let unit = if current_manifest.port_explicit && platform.os == HostOs::Linux {
                    for slot in ['a', 'b'] {
                        if let Ok(slot_unit) = slot_service_unit_name(app, slot) {
                            let _ = service_action(
                                transport,
                                &platform.os,
                                &slot_unit,
                                LifecycleAction::Stop,
                            );
                            let _ = disable_service(transport, &platform.os, &slot_unit);
                        }
                    }
                    remote_script(
                        transport,
                        "clear active service slot",
                        &format!(
                            "set -eu\nsudo -n rm -f {}\n",
                            shell_quote(&active_slot_path(&root, app))
                        ),
                    )?;
                    let unit = service_unit_name(app, false);
                    let user = service_user(transport, &platform.os, app)?;
                    install_service(
                        transport,
                        &platform.os,
                        &unit,
                        &user,
                        &current_path,
                        &format!("{root}/{app}/shared/env"),
                        false,
                        "./start",
                    )?;
                    enable_service(transport, &platform.os, &unit)?;
                    unit
                } else if let Some(slot) = active_slot {
                    let user = service_user(transport, &platform.os, app)?;
                    let unit = slot_service_unit_name(app, slot)?;
                    install_service(
                        transport,
                        &platform.os,
                        &unit,
                        &user,
                        &current_path,
                        &format!("{root}/{app}/shared/env"),
                        false,
                        "./start",
                    )?;
                    enable_service(transport, &platform.os, &unit)?;
                    service_action(transport, &platform.os, &unit, LifecycleAction::Start)?;
                    if let Some(slot) = rollback_slot {
                        let failed_unit = slot_service_unit_name(app, slot)?;
                        let _ = service_action(
                            transport,
                            &platform.os,
                            &failed_unit,
                            LifecycleAction::Stop,
                        );
                        let _ = disable_service(transport, &platform.os, &failed_unit);
                    }
                    write_active_slot(transport, &root, app, slot)?;
                    unit
                } else {
                    let unit = service_unit_name(app, false);
                    let user = service_user(transport, &platform.os, app)?;
                    install_service(
                        transport,
                        &platform.os,
                        &unit,
                        &user,
                        &current_path,
                        &format!("{root}/{app}/shared/env"),
                        false,
                        "./start",
                    )?;
                    enable_service(transport, &platform.os, &unit)?;
                    unit
                };
                service_action(transport, &platform.os, &unit, LifecycleAction::Restart)?;
                remote_healthcheck(
                    transport,
                    current_manifest.port.ok_or_else(|| {
                        CiaoError::Config("active release has no port".to_owned())
                    })?,
                    &current_manifest.health,
                )?;
            }
            if let Some(domain) = retained_domain.as_deref() {
                if existing_domain_is_plain_http(transport, app)? {
                    configure_domain_for_cloudflare(transport, app, domain)?;
                } else {
                    add_domain(transport, app, domain)?;
                }
            }
            configure_remote_ciao_domain(transport, app)?;
            sync_tailscale_funnel_route_if_present(transport, app)?;
            sync_cloudflare_tunnel_if_present(transport, app)?;
            reconcile_current_to_active(transport, &platform.os, &root, app)?;
            Ok::<(), CiaoError>(())
        })();
        let recovery = match restore {
            Ok(()) => format!("active release `{current}` was restored"),
            Err(restore_error) => {
                format!("active release `{current}` may require manual recovery: {restore_error}")
            }
        };
        return Err(CiaoError::Deployment {
            stage: "rollback".to_owned(),
            message: format!("{error}; {recovery}"),
            previous_release: current,
        });
    }
    if let Some(command) = manifest.hooks.on_rollback.as_deref() {
        let user = service_user(transport, &platform.os, app)?;
        let build_home = build_cache_path(&platform.os, app)?;
        let env_file = format!("{root}/{app}/shared/env");
        run_remote_hook(
            transport,
            &user,
            command,
            &format!("{root}/{app}/current"),
            &build_home,
            &env_file,
            "run rollback hook",
        )?;
    }
    let active_after_rollback = active_service(transport, &platform.os, &root, app)
        .ok()
        .and_then(|service| service.and_then(|value| value.release))
        .or_else(|| read_current_release(transport, &root, app).ok().flatten())
        .unwrap_or_else(|| previous.clone());
    Ok(OperationResult {
        app: app.to_owned(),
        action: "rollback".to_owned(),
        changed: true,
        message: format!(
            "✓ rolled back {app} from {current} to {previous}; active release `{active_after_rollback}`"
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn systemd_log_query_is_tail_bounded_and_timestamped() {
        let args =
            systemd_log_command_args("ciao-demo-slot-b.service", Some("15 minutes ago")).unwrap();
        assert_eq!(args[0], "-n");
        assert_eq!(args[1], "journalctl");
        assert!(args
            .windows(2)
            .any(|pair| pair == ["-u", "ciao-demo-slot-b.service"]));
        assert!(args.windows(2).any(|pair| pair == ["-n", "200"]));
        assert!(args.contains(&"--output=short-iso".to_owned()));
        assert!(args
            .windows(2)
            .any(|pair| pair == ["--since", "15 minutes ago"]));
    }

    #[test]
    fn systemd_log_query_rejects_unmanaged_units() {
        assert!(systemd_log_command_args("ssh demo.service", None).is_err());
    }
}
