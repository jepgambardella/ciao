# Security notes

Ciao treats the local project and configured SSH account as trust
boundaries. It does not maintain an SSH private-key store and uses the
installed OpenSSH client with fixed options (`BatchMode`, bounded connect
timeout, keepalives, and an explicit identity only when configured).
`ciao host add` can optionally create a normal user-owned Ed25519 identity
locally. Ciao stores only its path, never the private material in Ciao state,
and never copies it to the target.
Deploy/lifecycle operations use several independent SSH commands and therefore
probe non-interactive `sudo -n`; they require the SSH user to allow it. The
automatic deploy bootstrap (and explicit `ciao host init` path) is different:
it opens one standard OpenSSH session with `ssh -tt host sh -c ...`, where
`sudo -v` and the complete bootstrap share the same remote sudo TTY while the
password never enters Ciao.
CI and MCP have no terminal by design, so they also require passwordless
`sudo -n`; when this policy is missing, Ciao prints a concrete `visudo`
remediation for the configured SSH user and retry guidance. Ciao does not
modify sudoers.
The automatic bootstrap installs only the documented native prerequisites and
Caddy; the explicit `ciao host init` command invokes the same idempotent
operation.

The first host link reuses `~/.ssh/config`, the agent and existing keys. When
those are not usable, the guided bootstrap asks OpenSSH to perform one normal
interactive login, so the user approves the host key and enters the server
password directly into OpenSSH. Ciao sends only the generated public key to
`authorized_keys`, verifies a subsequent `BatchMode` connection, and never
accepts a password as an argument or environment variable.

Remote commands that contain paths or identifiers use values generated from
validated identifiers and a fixed Ciao layout. Build/install/start strings
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
configured by the explicit host bootstrap when needed; Ciao never manages
certificates or exposes a service port publicly by default.

Local `.ciao` setup is similarly narrow: it writes one Ciao-owned resolver
configuration and one Caddy import, then stores only project name, source and
internal port in the local config. It does not edit `/etc/hosts` per project,
store secrets, or expose a generic command runner.

Cloudflare Tunnel is deliberately not provisioned implicitly. Tunnel IDs,
credentials, DNS changes and the `cloudflared` service remain under the
operator's control; an exposure integration must use an explicit existing
configuration and must never print or copy its credential file.

GitHub auto-deploy is opt-in. It stores only the dedicated CI private key and
the trusted known-hosts material in GitHub Actions secrets; Ciao's local integration state
contains no CI private key or Tailscale bootstrap token. The generated workflow
uses GitHub OIDC through the official Tailscale action, a repository/branch
scoped identity tagged `tag:ciao-ci`, strict SSH host verification and a
repository-specific remote deployment lock. Policy changes are previewed and
validated before an interactive apply; HuJSON that Ciao cannot preserve safely
is rejected with the exact narrow rule to add manually. During setup Ciao may
install the Tailscale client using the native package manager and starts only
the standard browser authentication flow; it never stores a Tailscale login
token or asks the user to paste one.

The test suite includes validation and generated-definition checks plus a local
CLI/MCP smoke path. SSH tests are opt-in and disposable; they must not reboot
or otherwise disrupt their host.
