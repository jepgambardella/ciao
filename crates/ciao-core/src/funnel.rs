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
    let fragment = funnel_caddy_fragment(transport, &root, app, &release, &hostname)?;
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

    if let Err(error) = remote_script(
        transport,
        "enable Tailscale Funnel",
        &tailscale_funnel_enable_script(),
    ) {
        restore_fragment(transport, &fragment_path, previous.as_deref(), &platform.os);
        return Err(error);
    }

    let url = format!("https://{hostname}");
    Ok(FunnelResult {
        app: app.to_owned(),
        hostname,
        url: url.clone(),
        target: FUNNEL_TARGET.to_owned(),
        changed: previous.as_deref() != Some(fragment.as_str()),
        message: format!("✓ public Funnel: {url}"),
    })
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
}
