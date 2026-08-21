//! Small, fixed lifecycle hooks for project-owned deploy actions.

use super::*;

pub(super) fn run_local_hook(source: &Path, command: &str) -> Result<()> {
    validate_hook_command(command)?;
    let source = source.canonicalize().map_err(|error| {
        CiaoError::Config(format!("cannot resolve hook working directory: {error}"))
    })?;
    if !source.is_dir() {
        return Err(CiaoError::Config(
            "local hook working directory must be a directory".to_owned(),
        ));
    }
    let script = format!(
        "set -eu\ncd -- {}\n{}\n",
        shell_quote(&source.display().to_string()),
        command
    );
    let output = run_local_script(script.as_bytes())?;
    if output.status == 0 {
        Ok(())
    } else {
        Err(redact_hook_error(CiaoError::LocalCommand {
            stage: "run pre-upload hook".to_owned(),
            exit: output.status,
            stdout: output.stdout,
            stderr: output.stderr,
        }))
    }
}

pub(super) fn run_remote_hook(
    transport: &OpenSshTransport,
    user: &str,
    command: &str,
    cwd: &str,
    home: &str,
    env_file: &str,
    stage: &str,
) -> Result<()> {
    validate_hook_command(command)?;
    let command = command
        .trim()
        .strip_prefix("ciao run-remote ")
        .unwrap_or(command.trim());
    let script = command_script_with_home(command, cwd, home, env_file)?;
    run_as_user_script(transport, user, stage, &script).map_err(redact_hook_error)
}

fn validate_hook_command(command: &str) -> Result<()> {
    if command.trim().is_empty() || command.contains('\0') {
        return Err(CiaoError::Config(
            "hook command cannot be empty or contain NUL".to_owned(),
        ));
    }
    Ok(())
}

fn redact_hook_error(error: CiaoError) -> CiaoError {
    let redact = |value: String| redact_hook_text(&value);
    match error {
        CiaoError::LocalCommand {
            stage,
            exit,
            stdout,
            stderr,
        } => CiaoError::LocalCommand {
            stage,
            exit,
            stdout: redact(stdout),
            stderr: redact(stderr),
        },
        CiaoError::RemoteCommand {
            stage,
            exit,
            stdout,
            stderr,
        } => CiaoError::RemoteCommand {
            stage,
            exit,
            stdout: redact(stdout),
            stderr: redact(stderr),
        },
        other => other,
    }
}

fn redact_hook_text(value: &str) -> String {
    value
        .lines()
        .map(|line| {
            let lower = line.to_ascii_lowercase();
            if !(lower.contains("secret")
                || lower.contains("token")
                || lower.contains("password")
                || lower.contains("api_key"))
            {
                return line.to_owned();
            }
            let separator = line.find('=').or_else(|| line.find(':'));
            separator
                .map(|index| format!("{}=[REDACTED]", &line[..index]))
                .unwrap_or_else(|| "[REDACTED]".to_owned())
        })
        .collect::<Vec<_>>()
        .join("\n")
}
