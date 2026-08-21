use super::*;

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
