# Security notes

CiaoShip treats the local project and configured SSH account as trust
boundaries. It does not store SSH private keys and uses the installed OpenSSH
client with fixed options (`BatchMode`, bounded connect timeout and keepalives`).
Deploy/lifecycle operations require the SSH account to already have
non-interactive `sudo -n`; CiaoShip does not modify sudoers. The explicit
`ciaoship host init` operation uses that permission to install only the
documented native prerequisites and Caddy; a domain deploy invokes the same
idempotent operation before writing a CiaoShip-owned fragment.

Remote commands that contain paths or identifiers use values generated from
validated identifiers and a fixed CiaoShip layout. Build/install/start strings
come from the project because running the application is their purpose; they
are delivered as a script over stdin to `sh -s`, not interpolated into an SSH
destination or SSH option.

The MCP server is local stdio. Its profiles are `read-only`, `operator` and
`admin`; no profile exposes a generic shell tool. Secret values are accepted
only by the environment operation, transmitted through stdin, written to the
remote shared environment file with mode `0600`, and omitted from diagnostics.

Deployment uploads to a new staging path, builds and health-checks a candidate,
then switches the `current` symlink and service. Failures remove the candidate
and retain the previous release when one exists. Caddy is installed and
configured by the explicit host bootstrap when needed; CiaoShip never manages
certificates or exposes a service port publicly by default.

Local `.ciao` setup is similarly narrow: it writes one CiaoShip-owned resolver
configuration and one Caddy import, then stores only project name, source and
internal port in the local config. It does not edit `/etc/hosts` per project,
store secrets, or expose a generic command runner.

Cloudflare Tunnel is deliberately not provisioned implicitly. Tunnel IDs,
credentials, DNS changes and the `cloudflared` service remain under the
operator's control; an exposure integration must use an explicit existing
configuration and must never print or copy its credential file.

The test suite includes validation and generated-definition checks plus a local
CLI/MCP smoke path. SSH tests are opt-in and disposable; they must not reboot
or otherwise disrupt their host.
