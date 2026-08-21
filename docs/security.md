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
remediation for the configured SSH user. A human terminal deploy can apply the
same policy automatically only after explicit confirmation; CI and MCP never
modify sudoers.
The automatic bootstrap installs only the documented native prerequisites and
Caddy; the explicit `ciao host init` command invokes the same idempotent
operation.
If a terminal deploy is interrupted, Ciao may find its deployment marker on
the next run. It asks before removing only that marker; it never kills remote
processes and never removes a marker based only on its age. An actually active
deployment must be allowed to finish.

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
Lifecycle hooks use the same project trust boundary. `pre_upload` runs locally;
the remote hooks run as the application user from the release directory. Ciao
supports only the four fixed hook points and does not add a plugin loader or a
general-purpose remote shell. Hook diagnostics are bounded and sensitive
environment values are not included in normal output.

The MCP server is local stdio. Its profiles are `read-only`, `operator` and
`admin`; no profile exposes a generic shell tool. Secret values are accepted
only by the environment operations, transmitted through SSH, written to the
remote shared environment file with mode `0600`, and omitted from diagnostics.
Bulk environment pull downloads names by default; values require an explicit
flag. Push displays key names only and requires confirmation. Secret generation
uses the operating system CSPRNG and never prints the generated value.

Deployment uploads to a new staging path, builds and health-checks a candidate,
then switches the `current` symlink and service. Failures remove the candidate
and retain the previous release when one exists. Caddy is installed and
configured by the explicit host bootstrap when needed; Ciao never manages
certificates or exposes a service port publicly by default.

Local `.ciao` setup is similarly narrow: it writes one Ciao-owned resolver
configuration and one Caddy import, then stores only project name, source and
internal port in the local config. It does not edit `/etc/hosts` per project,
store secrets, or expose a generic command runner.

When `ciao deploy` receives `--domain`, Ciao uses the official `cloudflared`
CLI. It installs the client only when needed, opens `cloudflared tunnel login`
in the browser, creates or reuses a tunnel named `ciao-<app>`, creates the DNS
route and installs the standard `cloudflared` service on the target. The tunnel
credential file is read locally and sent over SSH only to the root-owned target
path with mode `0600`; it is never printed or stored in Ciao state. The public
TLS connection ends at Cloudflare. Caddy serves the tunnel origin on loopback.

`ciao deploy <host> funnel` is a separate, explicit public-exposure action. It
installs Tailscale only on the target when needed, requires the normal
Tailscale sign-in and Funnel approval flow, and routes only the selected Ciao
app through a dedicated Caddy fragment. Funnel terminates HTTPS at Tailscale
and forwards to Caddy on `127.0.0.1:80`; Ciao does not open the application
port in the host firewall. Removing the app disables the Ciao-owned Funnel
route before deleting its Caddy fragment. Funnel is public to anyone on the
Internet, so it is never enabled by a normal `ciao deploy`.

GitHub auto-deploy is opt-in. It stores only the dedicated CI private key and
the trusted known-hosts material in GitHub Actions secrets; Ciao's local integration state
contains no CI private key or Tailscale bootstrap token. The generated workflow
uses GitHub OIDC through the official Tailscale action, a repository/branch
scoped identity tagged `tag:ciao-ci`, strict SSH host verification and a
repository-specific remote deployment lock. Policy changes are previewed and
validated before an interactive apply; HuJSON that Ciao cannot preserve safely
is rejected with the exact narrow rule to add manually. During setup Ciao may
install the Tailscale client using the native package manager and starts only
the standard browser authentication flow. If the tailnet API needs admin
permission, Ciao asks for one temporary token in a normal terminal prompt,
uses it for setup, and never stores or logs it.

Ciao tracks every detached browser-authentication process in a Ciao-owned
temporary state directory. It stops that process after sign-in, on timeout, or
when the user interrupts the command. Local temporary servers and upload
children are also waited for and stopped on normal cancellation; Ciao does not
leave a managed child process behind after `ciao run`, `ciao dev`, or an
interrupted upload.

`ciao host audit` is read-only. It reads generated Caddy routes, service
definitions, sudoers policy and Ciao-owned file names over SSH and reports
missing, changed or orphaned entries without repairing them.

The test suite includes validation and generated-definition checks plus a local
CLI/MCP smoke path. SSH tests are opt-in and disposable; they must not reboot
or otherwise disrupt their host.
