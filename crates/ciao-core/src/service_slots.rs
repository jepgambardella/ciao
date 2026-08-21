use super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ActiveService {
    pub unit: String,
    pub release: Option<String>,
}

pub(super) fn slot_service_unit_name(app: &str, slot: char) -> Result<String> {
    validate_identifier("app name", app)?;
    if !matches!(slot, 'a' | 'b') {
        return Err(CiaoError::Config(
            "service slot must be `a` or `b`".to_owned(),
        ));
    }
    Ok(format!("ciao-{app}-slot-{slot}.service"))
}

pub(super) fn opposite_slot(slot: char) -> Result<char> {
    match slot {
        'a' => Ok('b'),
        'b' => Ok('a'),
        other => Err(CiaoError::Config(format!(
            "invalid active service slot `{other}`"
        ))),
    }
}

pub(super) fn active_slot_from_unit(unit: &str) -> Option<char> {
    if unit.ends_with("-slot-a.service") {
        Some('a')
    } else if unit.ends_with("-slot-b.service") {
        Some('b')
    } else {
        None
    }
}

pub(super) fn active_slot_path(root: &str, app: &str) -> String {
    format!("{root}/{app}/active-slot")
}

pub(super) fn read_active_slot<T: RemoteHost + ?Sized>(
    transport: &T,
    root: &str,
    app: &str,
) -> Result<Option<char>> {
    let path = active_slot_path(root, app);
    let output = remote_script(
        transport,
        "read active service slot",
        &format!(
            "set -eu\nif sudo -n test -f {}; then sudo -n cat {}; fi\n",
            shell_quote(&path),
            shell_quote(&path)
        ),
    )?;
    match output.stdout.trim() {
        "a" => Ok(Some('a')),
        "b" => Ok(Some('b')),
        "" => Ok(None),
        value => Err(CiaoError::Config(format!(
            "invalid active service slot `{value}`"
        ))),
    }
}

pub(super) fn write_active_slot<T: RemoteHost + ?Sized>(
    transport: &T,
    root: &str,
    app: &str,
    slot: char,
) -> Result<()> {
    let path = active_slot_path(root, app);
    let parent = format!("{root}/{app}");
    remote_script(
        transport,
        "record active service slot",
        &format!(
            "set -eu\nsudo -n install -d -m 0755 {}\nprintf '%s\\n' {} | sudo -n tee {} >/dev/null\nsudo -n chown root:root {}\nsudo -n chmod 0644 {}\n",
            shell_quote(&parent),
            shell_quote(&slot.to_string()),
            shell_quote(&path),
            shell_quote(&path),
            shell_quote(&path),
        ),
    )?;
    Ok(())
}

/// Inspect the service manager instead of trusting Ciao's bookkeeping files.
///
/// A deploy can be interrupted between the `current` symlink swap and the
/// active-slot marker write. On Linux, the unit's resolved WorkingDirectory is
/// the authoritative release that the running process was started from.
pub(super) fn active_service<T: RemoteHost + ?Sized>(
    transport: &T,
    os: &HostOs,
    root: &str,
    app: &str,
) -> Result<Option<ActiveService>> {
    if !matches!(os, HostOs::Linux) {
        return Ok(None);
    }
    let preferred_slot = read_active_slot(transport, root, app)?;
    let mut units = Vec::with_capacity(3);
    if let Some(slot) = preferred_slot {
        units.push(slot_service_unit_name(app, slot)?);
    }
    for slot in ['a', 'b'] {
        let unit = slot_service_unit_name(app, slot)?;
        if !units.iter().any(|candidate| candidate == &unit) {
            units.push(unit);
        }
    }
    let stable = service_unit_name(app, false);
    if !units.iter().any(|candidate| candidate == &stable) {
        units.push(stable);
    }

    for unit in units {
        if service_state(transport, os, &unit)? != "active" {
            continue;
        }
        let release = read_unit_release(transport, root, app, &unit)?;
        return Ok(Some(ActiveService { unit, release }));
    }
    Ok(None)
}

/// Repair the bookkeeping symlink after an interrupted activation. This is
/// deliberately driven by the active service unit, never by the stale marker
/// or by the release that the caller expected to be active.
pub(super) fn reconcile_current_to_active<T: RemoteHost + ?Sized>(
    transport: &T,
    os: &HostOs,
    root: &str,
    app: &str,
) -> Result<bool> {
    let current = read_current_release(transport, root, app)?;
    let Some(active) = active_service(transport, os, root, app)? else {
        return Ok(false);
    };
    let Some(release) = active.release else {
        return Ok(false);
    };
    let mut changed = false;
    if current.as_deref() != Some(release.as_str()) {
        remote_script(
            transport,
            "reconcile current release with active service",
            &switch_current_script(os, root, app, &format!("{root}/{app}/releases/{release}")),
        )?;
        changed = true;
    }
    if matches!(os, HostOs::Linux) {
        if let Some(slot) = active_slot_from_unit(&active.unit) {
            if read_active_slot(transport, root, app)? != Some(slot) {
                write_active_slot(transport, root, app, slot)?;
                changed = true;
            }
        } else if read_active_slot(transport, root, app)?.is_some() {
            remote_script(
                transport,
                "clear inactive service slot marker",
                &format!(
                    "set -eu\nsudo -n rm -f {}\n",
                    shell_quote(&active_slot_path(root, app))
                ),
            )?;
            changed = true;
        }
    }
    Ok(changed)
}

fn read_unit_release<T: RemoteHost + ?Sized>(
    transport: &T,
    root: &str,
    app: &str,
    unit: &str,
) -> Result<Option<String>> {
    let output = remote_script(
        transport,
        "read active service release",
        &format!(
            "set -eu\npid=$(sudo -n systemctl show --property=MainPID --value {} 2>/dev/null || true)\nworking=''\ncase \"$pid\" in\n    ''|0|*[!0-9]*) ;;\n    *) working=$(sudo -n readlink -f -- \"/proc/$pid/cwd\" 2>/dev/null || true) ;;\nesac\nif [ -z \"$working\" ]; then working=$(sudo -n systemctl show --property=WorkingDirectory --value {} 2>/dev/null || true); fi\ncase \"$working\" in\n    ''|-) ;;\n    *) readlink -f -- \"$working\" 2>/dev/null || true ;;\nesac\n",
            shell_quote(unit),
            shell_quote(unit)
        ),
    )?;
    let path = output.stdout.trim();
    Ok(release_from_working_directory(root, app, path))
}

fn release_from_working_directory(root: &str, app: &str, path: &str) -> Option<String> {
    let prefix = format!("{root}/{app}/releases/");
    let release = path.strip_prefix(&prefix)?;
    let release = release.trim_end_matches('/');
    if release.is_empty()
        || release.contains('/')
        || validate_identifier("release", release).is_err()
    {
        return None;
    }
    Some(release.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_release_parser_accepts_a_resolved_release_directory() {
        assert_eq!(
            release_from_working_directory(
                "/var/lib/ciao/apps",
                "demo",
                "/var/lib/ciao/apps/demo/releases/20260821-120000-123"
            ),
            Some("20260821-120000-123".to_owned())
        );
    }

    #[test]
    fn active_release_parser_rejects_unmanaged_or_nested_paths() {
        assert_eq!(
            release_from_working_directory(
                "/var/lib/ciao/apps",
                "demo",
                "/var/lib/ciao/apps/demo/current"
            ),
            None
        );
        assert_eq!(
            release_from_working_directory(
                "/var/lib/ciao/apps",
                "demo",
                "/var/lib/ciao/apps/demo/releases/../other"
            ),
            None
        );
    }

    #[test]
    fn active_slot_parser_only_accepts_ciao_slot_units() {
        assert_eq!(active_slot_from_unit("ciao-demo-slot-a.service"), Some('a'));
        assert_eq!(active_slot_from_unit("ciao-demo-slot-b.service"), Some('b'));
        assert_eq!(active_slot_from_unit("ciao-demo.service"), None);
    }
}
