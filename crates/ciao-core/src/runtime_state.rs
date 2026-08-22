//! Runtime-state safeguards for immutable releases.

use super::*;

const FAILED_RELEASE_KEEP: usize = 3;

/// Runtime state must not be copied into an immutable service release. Ciao
/// cannot safely infer whether a database is a seed asset or a writable cache,
/// so fail before upload and make the migration to shared storage explicit.
pub(crate) fn reject_service_runtime_state(source: &Path) -> Result<()> {
    let paths = discover_runtime_state_paths(source)?;
    if paths.is_empty() {
        return Ok(());
    }
    Err(CiaoError::Config(format!(
        "runtime state files cannot be deployed in an immutable release: {}. Move the state under $CIAO_SHARED_DIR (for example $CIAO_SHARED_DIR/{}) and add the files to .ciaoignore after updating the application path",
        paths.join(", "),
        paths.first().expect("non-empty runtime state paths")
    )))
}

fn discover_runtime_state_paths(source: &Path) -> Result<Vec<String>> {
    let ignore = ignore_patterns(source);
    let mut paths = Vec::new();
    visit_runtime_state(source, source, &ignore, &mut paths)?;
    paths.sort();
    paths.dedup();
    Ok(paths)
}

fn visit_runtime_state(
    root: &Path,
    directory: &Path,
    ignore: &[String],
    paths: &mut Vec<String>,
) -> Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            if is_runtime_state_ignored_directory(&path) {
                continue;
            }
            visit_runtime_state(root, &path, ignore, paths)?;
            continue;
        }
        if !file_type.is_file()
            || !path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(is_runtime_state_name)
        {
            continue;
        }
        let relative = path
            .strip_prefix(root)
            .map_err(|error| CiaoError::Config(error.to_string()))?
            .to_string_lossy()
            .replace(std::path::MAIN_SEPARATOR, "/");
        if !matches_ignore_pattern(&relative, ignore) {
            paths.push(relative);
        }
    }
    Ok(())
}

fn is_runtime_state_ignored_directory(path: &Path) -> bool {
    matches!(
        path.file_name().and_then(|name| name.to_str()),
        Some(".git" | ".ciao" | "target" | "node_modules" | "dist" | "build" | ".venv" | "venv")
    )
}

fn is_runtime_state_name(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    name.ends_with(".db")
        || name.ends_with(".sqlite")
        || name.ends_with(".sqlite3")
        || name.ends_with("-wal")
        || name.ends_with("-shm")
        || name.ends_with("-journal")
}

fn matches_ignore_pattern(relative: &str, patterns: &[String]) -> bool {
    let basename = Path::new(relative)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(relative);
    patterns.iter().any(|pattern| {
        let pattern = pattern.trim_start_matches("./").trim_start_matches('/');
        pattern == relative
            || pattern == basename
            || (pattern.starts_with("*.") && basename.ends_with(&pattern[1..]))
            || (pattern.starts_with("**/") && relative.ends_with(&pattern[2..]))
    })
}

/// Preserve a failed candidate for inspection without making it eligible for
/// rollback. The quarantine is bounded to the latest three failed releases.
pub(crate) fn quarantine_failed_release(
    transport: &OpenSshTransport,
    os: &HostOs,
    app: &str,
    release: &str,
) -> Result<bool> {
    validate_identifier("app name", app)?;
    validate_identifier("release", release)?;
    let root = host_app_root(os);
    let current_path = format!("{root}/{app}/current");
    let release_path = format!("{root}/{app}/releases/{release}");
    let failed_root = format!("{root}/{app}/.failed");
    let failed_path = format!("{failed_root}/{release}");
    let staging_path = format!("/tmp/ciao-{app}-{release}");
    let script = format!(
        "set -eu\ncurrent=''\nif sudo -n test -L {current_path}; then current=$(sudo -n readlink {current_path}); fi\ncase \"$(basename \"$current\")\" in {release}) echo 'refusing to quarantine the release selected by current' >&2; exit 1;; esac\nexists=0\nif sudo -n test -e {release_path} || sudo -n test -L {release_path}; then exists=1; fi\nif [ \"$exists\" -eq 1 ]; then sudo -n install -d -m 0750 {failed_root}; sudo -n rm -rf {failed_path}; sudo -n mv {release_path} {failed_path}; printf 'failed_at=%s\\nrelease=%s\\n' \"$(date -u +%Y-%m-%dT%H:%M:%SZ)\" {release} | sudo -n tee {failed_path}/.ciao-failure >/dev/null; fi\nsudo -n rm -rf {staging_path}\ncount=0\nfor path in $(ls -1dt {failed_root}/* 2>/dev/null || true); do [ -d \"$path\" ] || continue; count=$((count + 1)); if [ \"$count\" -gt {keep} ]; then sudo -n rm -rf \"$path\"; fi; done\nprintf '%s\\n' \"$exists\"\n",
        current_path = shell_quote(&current_path),
        release = shell_quote(release),
        release_path = shell_quote(&release_path),
        failed_root = shell_quote(&failed_root),
        failed_path = shell_quote(&failed_path),
        staging_path = shell_quote(&staging_path),
        keep = FAILED_RELEASE_KEEP,
    );
    let output = remote_script(transport, "quarantine failed release", &script)?;
    Ok(output.stdout.trim() == "1")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_sqlite_state_and_sidecars() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("match.db"), b"sqlite").unwrap();
        fs::create_dir(directory.path().join("data")).unwrap();
        fs::write(directory.path().join("data/cache.sqlite3-wal"), b"wal").unwrap();
        let paths = discover_runtime_state_paths(directory.path()).unwrap();
        assert_eq!(paths, vec!["data/cache.sqlite3-wal", "match.db"]);
    }

    #[test]
    fn rejects_unignored_runtime_state_with_shared_path_guidance() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("match.db"), b"sqlite").unwrap();
        let error = reject_service_runtime_state(directory.path()).unwrap_err();
        let message = error.to_string();
        assert!(message.contains("match.db"));
        assert!(message.contains("CIAO_SHARED_DIR"));
        assert!(message.contains(".ciaoignore"));
    }

    #[test]
    fn explicit_ignore_suppresses_runtime_state_preflight() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("match.db"), b"sqlite").unwrap();
        fs::write(directory.path().join(".ciaoignore"), "*.db\n").unwrap();
        assert!(discover_runtime_state_paths(directory.path())
            .unwrap()
            .is_empty());
    }
}
