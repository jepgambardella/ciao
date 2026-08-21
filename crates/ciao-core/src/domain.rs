//! Caddy and domain management.

use super::*;

pub fn add_domain(transport: &OpenSshTransport, app: &str, domain: &str) -> Result<()> {
    validate_identifier("app name", app)?;
    validate_domain(domain)?;
    init_host(transport)?;
    configure_domain(transport, app, domain)
}

pub(super) fn configure_domain(
    transport: &OpenSshTransport,
    app: &str,
    domain: &str,
) -> Result<()> {
    validate_identifier("app name", app)?;
    validate_domain(domain)?;
    let platform = transport.inspect()?;
    let root = host_app_root(&platform.os);
    let release = read_current_release(transport, &root, app)?
        .ok_or_else(|| CiaoError::Config(format!("app `{app}` has no active release")))?;
    let fragment = caddy_fragment(transport, &root, app, &release, domain)?;
    let fragment_path = format!("/etc/caddy/ciao/{app}.caddy");
    remote_script(
        transport,
        "prepare Caddy directory",
        "set -eu\nsudo -n install -d -m 0755 /etc/caddy/ciao\n",
    )?;
    write_remote_file(
        transport,
        &fragment_path,
        &fragment,
        "root",
        "write Caddy fragment",
    )?;
    remote_script(
        transport,
        "reload Caddy",
        &caddy_reload_script(&platform.os),
    )?;
    Ok(())
}

/// Use plain HTTP between a Cloudflare Tunnel and Caddy. Cloudflare handles
/// the public TLS connection; keeping the origin local avoids a certificate
/// challenge loop through the tunnel.
pub fn configure_domain_for_cloudflare(
    transport: &OpenSshTransport,
    app: &str,
    domain: &str,
) -> Result<()> {
    validate_identifier("app name", app)?;
    validate_domain(domain)?;
    let platform = transport.inspect()?;
    let root = host_app_root(&platform.os);
    let release = read_current_release(transport, &root, app)?
        .ok_or_else(|| CiaoError::Config(format!("app `{app}` has no active release")))?;
    let fragment = caddy_fragment_with_scheme(transport, &root, app, &release, domain, true)?;
    let fragment_path = format!("/etc/caddy/ciao/{app}.caddy");
    write_remote_file(
        transport,
        &fragment_path,
        &fragment,
        "root",
        "write Cloudflare Caddy route",
    )?;
    remote_script(
        transport,
        "reload Caddy for Cloudflare",
        &caddy_reload_script(&platform.os),
    )?;
    Ok(())
}

/// Add the stable local `.ciao` host on the remote Caddy instance. It uses a
/// separate fragment, so a public domain configured for the same app remains
/// active at the same time.
pub fn configure_remote_ciao_domain(transport: &OpenSshTransport, app: &str) -> Result<()> {
    validate_identifier("app name", app)?;
    let domain = local_domain(app)?;
    let platform = transport.inspect()?;
    let root = host_app_root(&platform.os);
    let release = read_current_release(transport, &root, app)?
        .ok_or_else(|| CiaoError::Config(format!("app `{app}` has no active release")))?;
    let fragment = caddy_fragment_with_scheme(transport, &root, app, &release, &domain, true)?;
    let fragment_path = format!("/etc/caddy/ciao/{app}.local.caddy");
    remote_script(
        transport,
        "prepare local Ciao Caddy directory",
        "set -eu\nsudo -n install -d -m 0755 /etc/caddy/ciao\n",
    )?;
    write_remote_file(
        transport,
        &fragment_path,
        &fragment,
        "root",
        "write local Ciao Caddy route",
    )?;
    remote_script(
        transport,
        "reload Caddy for local Ciao domain",
        &caddy_reload_script(&platform.os),
    )?;
    Ok(())
}

pub(super) fn remove_domain_fragment(transport: &OpenSshTransport, app: &str) -> Result<()> {
    validate_identifier("app name", app)?;
    let path = format!("/etc/caddy/ciao/{app}.caddy");
    remote_script(
        transport,
        "cleanup Caddy fragment",
        &format!("set -eu\nsudo -n rm -f {}\n", shell_quote(&path)),
    )?;
    Ok(())
}

fn caddy_fragment(
    transport: &OpenSshTransport,
    root: &str,
    app: &str,
    release: &str,
    domain: &str,
) -> Result<String> {
    caddy_fragment_with_scheme(transport, root, app, release, domain, false)
}

pub(super) fn caddy_fragment_with_scheme(
    transport: &OpenSshTransport,
    root: &str,
    app: &str,
    release: &str,
    domain: &str,
    cloudflare_origin: bool,
) -> Result<String> {
    let site = if cloudflare_origin {
        format!("http://{domain}")
    } else {
        domain.to_owned()
    };
    if let Some(static_directory) = read_release_static_directory(transport, root, app, release)? {
        let static_root = format!("{root}/{app}/releases/{release}/{static_directory}");
        Ok(format!(
            "{site} {{\n    root * {}\n    file_server\n}}\n",
            static_root
        ))
    } else {
        let port = read_release_port(transport, root, app, release)?.unwrap_or(PORT_START);
        Ok(format!(
            "{site} {{\n    reverse_proxy 127.0.0.1:{port}\n}}\n"
        ))
    }
}

pub(super) fn configure_release_caddy_route(
    transport: &OpenSshTransport,
    os: &HostOs,
    root: &str,
    app: &str,
    release: &str,
    domain: &str,
    local: bool,
) -> Result<()> {
    let fragment = caddy_fragment_with_scheme(transport, root, app, release, domain, local)?;
    let suffix = if local { ".local.caddy" } else { ".caddy" };
    let path = format!("/etc/caddy/ciao/{app}{suffix}");
    remote_script(
        transport,
        "prepare Caddy directory",
        "set -eu\nsudo -n install -d -m 0755 /etc/caddy/ciao\n",
    )?;
    write_remote_file(transport, &path, &fragment, "root", "write Caddy route")?;
    remote_script(transport, "reload Caddy", &caddy_reload_script(os))?;
    Ok(())
}

pub(super) fn read_existing_domain(
    transport: &OpenSshTransport,
    app: &str,
) -> Result<Option<String>> {
    validate_identifier("app name", app)?;
    let path = format!("/etc/caddy/ciao/{app}.caddy");
    let output = remote_script(
        transport,
        "read existing domain",
        &format!(
            "set -eu\nif sudo -n test -f {}; then sudo -n awk 'NR == 1 {{print $1; exit}}' {}; fi\n",
            shell_quote(&path),
            shell_quote(&path)
        ),
    )?;
    let value = output.stdout.trim();
    if value.is_empty() {
        Ok(None)
    } else {
        let normalized = value
            .strip_prefix("http://")
            .or_else(|| value.strip_prefix("https://"))
            .unwrap_or(value)
            .trim_end_matches('/');
        validate_domain(normalized)?;
        Ok(Some(normalized.to_owned()))
    }
}

pub(super) fn existing_domain_is_plain_http(
    transport: &OpenSshTransport,
    app: &str,
) -> Result<bool> {
    validate_identifier("app name", app)?;
    let path = format!("/etc/caddy/ciao/{app}.caddy");
    let output = remote_script(
        transport,
        "read existing domain scheme",
        &format!(
            "set -eu\nif sudo -n test -f {}; then sudo -n awk 'NR == 1 {{print $1; exit}}' {}; fi\n",
            shell_quote(&path),
            shell_quote(&path)
        ),
    )?;
    Ok(output.stdout.trim_start().starts_with("http://"))
}

pub fn remove_domain(transport: &OpenSshTransport, app: &str, domain: &str) -> Result<()> {
    validate_identifier("app name", app)?;
    validate_domain(domain)?;
    let existing = read_existing_domain(transport, app)?;
    match existing.as_deref() {
        None => return Ok(()),
        Some(existing) if existing != domain => {
            return Err(CiaoError::Config(format!(
                "app `{app}` is configured for `{existing}`, not `{domain}`"
            )))
        }
        Some(_) => {}
    }
    let platform = transport.inspect()?;
    let path = format!("/etc/caddy/ciao/{app}.caddy");
    remote_script(
        transport,
        "remove Caddy fragment",
        &format!(
            "set -eu\nsudo -n rm -f {}\n{}",
            shell_quote(&path),
            caddy_reload_script(&platform.os)
        ),
    )?;
    Ok(())
}

pub(super) fn caddy_reload_script(os: &HostOs) -> String {
    let (setup, config, reload) = match os {
        HostOs::MacOs => (
            r#"caddy_config=''
brew_bin=''
for candidate in /opt/homebrew/bin/brew /usr/local/bin/brew /home/linuxbrew/.linuxbrew/bin/brew; do
    if [ -x "$candidate" ]; then brew_bin="$candidate"; break; fi
done
[ -n "$brew_bin" ] || { echo 'Homebrew is not available for the remote Caddy service' >&2; exit 1; }
brew_prefix=$("$brew_bin" --prefix)
caddy_config="$brew_prefix/etc/Caddyfile"
"#,
            "\"$caddy_config\"",
            "sudo -n \"$caddy_bin\" reload --config \"$caddy_config\"",
        ),
        _ => ("", "/etc/caddy/Caddyfile", "sudo -n systemctl reload caddy"),
    };
    format!(
        "set -eu\n{setup}sudo -n test -f {config}\nif ! sudo -n grep -Fq 'import /etc/caddy/ciao/*.caddy' {config}; then echo 'Caddyfile must import /etc/caddy/ciao/*.caddy' >&2; exit 1; fi\ncaddy_bin=$(command -v caddy || true)\nfor candidate in /opt/homebrew/bin/caddy /usr/local/bin/caddy /opt/homebrew/opt/caddy/bin/caddy /usr/bin/caddy; do if [ -z \"$caddy_bin\" ] && [ -x \"$candidate\" ]; then caddy_bin=\"$candidate\"; fi; done\nif [ -z \"$caddy_bin\" ]; then echo 'Caddy is not installed; run host initialization' >&2; exit 1; fi\nsudo -n \"$caddy_bin\" validate --config {config} && {reload}\n",
        setup = setup,
        config = config,
        reload = reload,
    )
}

pub fn validate_domain(domain: &str) -> Result<()> {
    if domain.len() > 253
        || domain.is_empty()
        || domain.starts_with('.')
        || domain.ends_with('.')
        || domain.split('.').any(|label| {
            label.is_empty()
                || label.len() > 63
                || label.starts_with('-')
                || label.ends_with('-')
                || !label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
    {
        return Err(CiaoError::InvalidIdentifier {
            field: "domain",
            value: domain.to_owned(),
            reason: "must contain DNS labels only",
        });
    }
    Ok(())
}
