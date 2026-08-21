//! Remote environment-file operations.
//!
//! Keeping these operations together gives the CLI a small, explicit API for
//! single-key updates while leaving room for the bulk pull/push/diff flows.

use super::*;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvEntry {
    pub key: String,
    pub value: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvDiff {
    pub added: Vec<String>,
    pub modified: Vec<String>,
    pub removed: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvGenerateResult {
    pub app: String,
    pub key: String,
    pub changed: bool,
    pub message: String,
}

/// Validate a POSIX environment variable name.
pub fn validate_env_key(key: &str) -> Result<()> {
    let valid = !key.is_empty()
        && key
            .as_bytes()
            .first()
            .is_some_and(|byte| byte.is_ascii_alphabetic() || *byte == b'_')
        && key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_');
    if !valid {
        return Err(CiaoError::InvalidIdentifier {
            field: "environment variable",
            value: key.to_owned(),
            reason: "must match [A-Za-z_][A-Za-z0-9_]*",
        });
    }
    Ok(())
}

pub(super) fn env_file_line(key: &str, value: &str) -> String {
    let escaped = value
        .chars()
        .flat_map(|character| {
            if matches!(character, '\\' | '"' | '$' | '`') {
                vec!['\\', character]
            } else {
                vec![character]
            }
        })
        .collect::<String>();
    format!(r#"{key}="{escaped}""#)
}

pub fn set_env(transport: &OpenSshTransport, app: &str, key: &str, value: &str) -> Result<()> {
    validate_identifier("app name", app)?;
    validate_env_key(key)?;
    if value.contains(['\n', '\r']) {
        return Err(CiaoError::Config(
            "environment values cannot contain newlines".to_owned(),
        ));
    }
    if value.len() > 64 * 1024 {
        return Err(CiaoError::Config(
            "environment value exceeds the 64 KiB limit".to_owned(),
        ));
    }
    let platform = transport.inspect()?;
    let root = host_app_root(&platform.os);
    let user = service_user(transport, &platform.os, app)?;
    let path = format!("{root}/{app}/shared/env");
    let line = env_file_line(key, value);
    let script = format!(
        "set -eu\nroot={}\nfile={}\nsudo -n install -d -m 0755 \"$root\"\nsudo -n touch \"$file\"\nsudo -n chmod 0600 \"$file\"\nsudo -n sed -i.bak '/^{}=/d' \"$file\"\nprintf '%s\\n' {} | sudo -n tee -a \"$file\" >/dev/null\nsudo -n rm -f \"$file.bak\"\nsudo -n chown {} \"$file\"\n",
        shell_quote(&format!("{root}/{app}/shared")),
        shell_quote(&path),
        regex_literal(key),
        shell_quote(&line),
        shell_quote(&user),
    );
    remote_script(transport, "set environment", &script)
        .map_err(|error| redact_error(error, &[value]))?;
    Ok(())
}

pub fn unset_env(transport: &OpenSshTransport, app: &str, key: &str) -> Result<()> {
    validate_identifier("app name", app)?;
    validate_env_key(key)?;
    let platform = transport.inspect()?;
    let root = host_app_root(&platform.os);
    let path = format!("{root}/{app}/shared/env");
    remote_script(
        transport,
        "unset environment",
        &format!(
            "set -eu\nif sudo -n test -f {}; then sudo -n sed -i.bak '/^{}=/d' {}; sudo -n rm -f {}.bak; fi\n",
            shell_quote(&path),
            regex_literal(key),
            shell_quote(&path),
            shell_quote(&path)
        ),
    )?;
    Ok(())
}

/// Download the remote environment file. Values are kept in memory and are
/// never included in the returned diff or status messages.
pub fn pull_env(
    transport: &OpenSshTransport,
    app: &str,
    destination: &Path,
    with_values: bool,
) -> Result<Vec<String>> {
    validate_identifier("app name", app)?;
    let entries = read_env_entries(transport, app)?;
    let contents = entries
        .iter()
        .map(|entry| {
            if with_values {
                env_file_line(&entry.key, &entry.value)
            } else {
                format!("{}=", entry.key)
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    let mut contents = contents;
    if !contents.is_empty() {
        contents.push('\n');
    }
    write_local_env_file(destination, contents.as_bytes())?;
    Ok(entries.into_iter().map(|entry| entry.key).collect())
}

fn write_local_env_file(destination: &Path, contents: &[u8]) -> Result<()> {
    let file_name = destination
        .file_name()
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_else(|| "env".to_owned());
    let temporary =
        destination.with_file_name(format!(".{file_name}.ciao-tmp-{}", std::process::id()));
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let result = (|| {
        let mut file = options.open(&temporary)?;
        file.write_all(contents)?;
        file.sync_all()?;
        fs::rename(&temporary, destination)?;
        #[cfg(unix)]
        fs::set_permissions(destination, fs::Permissions::from_mode(0o600))?;
        Ok::<(), io::Error>(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result.map_err(CiaoError::from)
}

pub fn diff_env(transport: &OpenSshTransport, app: &str, local_path: &Path) -> Result<EnvDiff> {
    validate_identifier("app name", app)?;
    let local = parse_env_contents(&fs::read_to_string(local_path)?)?;
    let remote = read_env_entries(transport, app)?;
    Ok(compare_env(&local, &remote))
}

pub fn push_env(transport: &OpenSshTransport, app: &str, local_path: &Path) -> Result<EnvDiff> {
    validate_identifier("app name", app)?;
    let local = parse_env_contents(&fs::read_to_string(local_path)?)?;
    let remote = read_env_entries(transport, app)?;
    let diff = compare_env(&local, &remote);
    write_env_entries(transport, app, &local)?;
    Ok(diff)
}

pub fn generate_env(
    transport: &OpenSshTransport,
    app: &str,
    key: &str,
) -> Result<EnvGenerateResult> {
    validate_identifier("app name", app)?;
    validate_env_key(key)?;
    let mut bytes = [0_u8; 32];
    let mut random = fs::File::open("/dev/urandom")?;
    random.read_exact(&mut bytes)?;
    let value = bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    set_env(transport, app, key, &value)?;
    Ok(EnvGenerateResult {
        app: app.to_owned(),
        key: key.to_owned(),
        changed: true,
        message: format!("✓ generated and stored {key} for {app}"),
    })
}

fn read_env_entries(transport: &OpenSshTransport, app: &str) -> Result<Vec<EnvEntry>> {
    let platform = transport.inspect()?;
    let path = format!("{}/{app}/shared/env", host_app_root(&platform.os));
    let command = CommandSpec::fixed("sh", &["-s"], "read environment")
        .with_stdin(
            format!(
                "set -eu\nif sudo -n test -f {}; then sudo -n cat {}; fi\n",
                shell_quote(&path),
                shell_quote(&path)
            )
            .into_bytes(),
        )
        .with_full_output();
    let output = transport
        .exec(command)?
        .ensure_success("read environment")?;
    parse_env_contents(&output.stdout)
}

fn write_env_entries(transport: &OpenSshTransport, app: &str, entries: &[EnvEntry]) -> Result<()> {
    let platform = transport.inspect()?;
    let user = service_user(transport, &platform.os, app)?;
    let path = format!("{}/{app}/shared/env", host_app_root(&platform.os));
    let mut contents = entries
        .iter()
        .map(|entry| env_file_line(&entry.key, &entry.value))
        .collect::<Vec<_>>()
        .join("\n");
    if !contents.is_empty() {
        contents.push('\n');
    }
    write_remote_file(transport, &path, &contents, &user, "write environment")?;
    remote_script(
        transport,
        "protect environment file",
        &format!("set -eu\nsudo -n chmod 0600 {}\n", shell_quote(&path)),
    )?;
    Ok(())
}

fn parse_env_contents(contents: &str) -> Result<Vec<EnvEntry>> {
    let mut entries = BTreeMap::new();
    for (line_number, raw) in contents.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let (key, value) = line.split_once('=').ok_or_else(|| {
            CiaoError::Config(format!("invalid environment line {}", line_number + 1))
        })?;
        validate_env_key(key.trim())?;
        let value = parse_env_value(value.trim());
        if value.contains(['\n', '\r']) || value.len() > 64 * 1024 {
            return Err(CiaoError::Config(format!(
                "environment value on line {} is invalid",
                line_number + 1
            )));
        }
        entries.insert(key.trim().to_owned(), value);
    }
    Ok(entries
        .into_iter()
        .map(|(key, value)| EnvEntry { key, value })
        .collect())
}

fn parse_env_value(value: &str) -> String {
    let value = value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .or_else(|| {
            value
                .strip_prefix('\'')
                .and_then(|value| value.strip_suffix('\''))
        })
        .unwrap_or(value);
    let mut output = String::with_capacity(value.len());
    let mut escaped = false;
    for character in value.chars() {
        if escaped {
            output.push(character);
            escaped = false;
        } else if character == '\\' {
            escaped = true;
        } else {
            output.push(character);
        }
    }
    if escaped {
        output.push('\\');
    }
    output
}

fn compare_env(local: &[EnvEntry], remote: &[EnvEntry]) -> EnvDiff {
    let local = local
        .iter()
        .map(|entry| (&entry.key, &entry.value))
        .collect::<BTreeMap<_, _>>();
    let remote = remote
        .iter()
        .map(|entry| (&entry.key, &entry.value))
        .collect::<BTreeMap<_, _>>();
    let mut diff = EnvDiff::default();
    for (key, value) in &local {
        match remote.get(key) {
            None => diff.added.push((*key).clone()),
            Some(remote_value) if remote_value != value => diff.modified.push((*key).clone()),
            Some(_) => {}
        }
    }
    for key in remote.keys() {
        if !local.contains_key(key) {
            diff.removed.push((*key).clone());
        }
    }
    diff
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_shell_quoted_environment_without_executing_it() {
        let entries = parse_env_contents(
            r#"A=plain
SECRET="a'b \$HOME \"quoted\" \\ slash"
export EMPTY=
"#,
        )
        .unwrap();
        assert_eq!(entries[0].key, "A");
        assert_eq!(entries[0].value, "plain");
        assert_eq!(entries[1].key, "EMPTY");
        assert_eq!(entries[1].value, "");
        assert_eq!(entries[2].key, "SECRET");
        assert_eq!(entries[2].value, "a'b $HOME \"quoted\" \\ slash");
    }

    #[test]
    fn environment_diff_contains_names_only() {
        let local = parse_env_contents("A=one\nB=two\n").unwrap();
        let remote = parse_env_contents("A=changed\nC=three\n").unwrap();
        let diff = compare_env(&local, &remote);
        assert_eq!(diff.added, vec!["B"]);
        assert_eq!(diff.modified, vec!["A"]);
        assert_eq!(diff.removed, vec!["C"]);
    }
}
