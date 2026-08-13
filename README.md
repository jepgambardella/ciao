# Ciao

> Ship apps. Skip the ops.

Ciao is a local CLI and stdio MCP server for deploying small Rust, Go,
Bun, Node and static applications to a Linux or macOS host over the installed
OpenSSH client. Service releases are immutable and run as ordinary systemd or
launchd services; Ciao does not leave a daemon on the host.

The intended workflow is:

```bash
cargo install --path crates/ciao
ciao host add home user@server
cd my-project
ciao inspect
ciao deploy home
ciao status home my-project
ciao logs home my-project
ciao rollback home my-project
ciao apps home
ciao releases home my-project
```

No Ciao daemon, management port, private-key database, container runtime or
remote database is installed. Applications run as a dedicated Unix user and
remain ordinary system services when the local binary is not running.

## Requirements

- Rust 1.80 or newer to build Ciao.
- An OpenSSH client locally and an SSH account on the target.
- `ciao host add` first reuses the normal OpenSSH configuration, agent and
  existing keys. If that login is not ready, run it from a terminal and Ciao
  offers a one-time guided bootstrap: it creates a standard Ed25519 identity
  under `~/.ssh/ciao/`, asks OpenSSH to authenticate once with the server
  password, installs only the public key, and verifies key-only access. The
  password is handled by OpenSSH and is never read or stored by Ciao. Use
  `--non-interactive` to fail instead of prompting, or `--setup-key` to force
  the guided identity for a host.
- Linux hosts need `systemd` and an SSH account allowed to use `sudo`. `ciao
  deploy home` opens one standard interactive SSH session when preparation is
  needed and runs the complete bootstrap under that remote sudo TTY; type the
  host password at that prompt. `ciao host init` uses the same path explicitly.
  Ciao never reads, stores or forwards that password. macOS hosts use
  `launchd` and the same flow. Later deploy/lifecycle commands use several
  non-interactive SSH sessions, so the SSH user must allow `sudo -n` (or an
  equivalent administrator policy). GitHub Actions and MCP always require
  passwordless `sudo -n`.
- `ciao deploy home` first performs a read-only host readiness check and, when
  needed, runs the same idempotent bootstrap as `ciao host init` before
  uploading the application. It installs Ciao's native prerequisites and Caddy
  on the target. On macOS it detects Homebrew in the standard Apple
  Silicon/Intel locations and installs it when missing. `ciao host init` remains
  available when you want to prepare a host explicitly.
- If deploy reports that passwordless sudo is missing, follow the commands
  printed by Ciao: open the sudo policy with `sudo visudo`, add the exact
  SSH-user rule shown there, validate it with `visudo -c`, then rerun the
  printed `ciao deploy ...` command. Ciao never asks for, stores or edits the
  sudo policy automatically.
- The detected Rust, Go, Node or Bun runtime is installed only when that
  runtime is needed by a deploy. Static deployments do not install a runtime.

## Configuration

Hosts are stored in `~/.config/ciao/config.toml`:

```toml
[hosts.home]
ssh = "user@server"
# Added only when the guided SSH bootstrap is used:
# identity_file = "/Users/me/.ssh/ciao/home_ed25519"

[mcp]
profile = "operator"
```

The project can opt into a small `ciao.toml` when detection is not enough:

```toml
[app]
name = "my-api"

[build]
install = "bun install --frozen-lockfile"
command = "bun run build"

[run]
command = "bun start"

[health]
path = "/health"
expected_status = 200
timeout = "10s"
```

For local development, the same file can customize only what is needed:

```toml
[dev]
name = "admin"
port = 41001
command = "bun run dev"
```

`[build]` and `[run]` commands are application commands. They are sent to the
host as stdin to a fixed `sh -s` invocation; Ciao never builds an SSH
command by concatenating host or application identifiers.

## Local development

Run this from a supported project:

```bash
ciao dev
```

Ciao installs and configures Homebrew (macOS when absent), dnsmasq and
Caddy on the first run. It creates one resolver for the whole namespace:
`*.ciao -> 127.0.0.1`. There is no per-project `/etc/hosts` edit. The project
name becomes a stable URL such as `http://my-api.ciao`; an internal port is
chosen and persisted in the local configuration. If a preferred port is
occupied, Ciao selects another free port automatically.
`ciao dev --name admin` overrides the local name without changing project
metadata.

The local process runs in the foreground. Caddy owns the HTTP entrypoint and
routes the Host header to the selected loopback port. Static projects keep
their Caddy route after the command exits; service routes are removed when the
process stops. The local setup uses native OS services and is safe to repeat.

## Safety model

- Host and application identifiers are validated before becoming paths or
  service names.
- SSH options are fixed. Ciao does not keep a private-key store or copy
  private keys to a target; an optional guided bootstrap creates a normal
  user-owned OpenSSH key locally and stores only its path in Ciao config.
- Deployments upload to a new staging directory, build there, health-check a
  candidate service, and only then replace `current`. If a domain is supplied,
  Ciao initializes Caddy before writing its own fragment.
- Failed builds and health checks remove only the new release and leave the
  previous service active.
- Secrets set with `env set` are sent over stdin, written with mode `0600`, and
  are not printed.
- MCP exposes deploy/status/log/restart/rollback concepts, never arbitrary
  shell execution.

## Current scope

The implemented path is project detection, SSH host inspection, automatic host
bootstrap, immutable release deployment, health checking, service lifecycle,
rollback, environment updates, Caddy domain fragments, app/release listing, a
local `.ciao` development resolver and proxy, a temporary localhost dashboard,
and the local MCP surface. Static deployments produce an immutable release and
manifest. Cloudflare Tunnel provisioning is not implemented in v0.1: point an
existing `cloudflared` tunnel configuration at the managed localhost port, or
add that narrow adapter separately. Ciao does not manage Cloudflare
accounts, DNS, tunnel credentials or the remote `cloudflared` service.

Run the local checks with `cargo fmt --all -- --check`, `cargo clippy
--workspace --all-targets -- -D warnings`, and `cargo test --workspace
--all-targets`. CI also builds the CLI, exercises project detection plus the
MCP stdio handshake against a temporary fixture, and tests natively on
Apple Silicon. SSH deployment tests require a separately provisioned
disposable host; see [TESTING.md](TESTING.md). Live tests must never reboot a
host.

See [docs/architecture.md](docs/architecture.md), [docs/security.md](docs/security.md),
[docs/local_ciao_domain.md](docs/local_ciao_domain.md),
[GitHub/Tailscale auto-deploy](docs/Ciao_GitHub_Tailscale_Autodeploy.md), and the
product source of truth in [Ciao.md](Ciao.md).

## GitHub auto-deploy

After a successful manual deploy, configure the optional GitHub Actions path
explicitly from the project directory:

```bash
ciao github setup --host home
```

The setup detects the GitHub repository, reuses Tailscale when it is already
installed, installs it when it is missing, and reads the target's Tailscale
address. If the local or target node needs sign-in, Ciao starts the login,
opens the Tailscale page in the browser, and waits for completion; the user
does not need to type `tailscale up`. It then installs a repository-specific
Ed25519 deploy key, copies the already trusted OpenSSH `known_hosts` entry,
creates a
repository-scoped Tailscale OIDC identity and writes
`.github/workflows/ciao-deploy.yml`. It never stores the Tailscale bootstrap
token or the private key in Ciao state. Use `ciao github status` to inspect the
link and `ciao github regenerate` after an explicit Ciao upgrade.

Because this repository is private, setup also needs a read-only GitHub token
with read access to `jepgambardella/ciao`, supplied through
`CIAO_GITHUB_TOKEN` (or `--ciao-github-token-stdin`). The token is stored only
as the target repository's Actions secret so the runner can checkout the pinned
Ciao source; it is never written to the generated workflow.

The generated workflow runs `ciao deploy --ci` with the same deploy engine as a
manual deployment. It uses GitHub OIDC plus Tailscale, strict host-key checking,
GitHub concurrency and Ciao's remote deployment lock. See
[the integration design](docs/Ciao_GitHub_Tailscale_Autodeploy.md) for the
security model and manual recovery paths.

## License

Copyright © 2026 Luca La Barbera. Ciao is source-available for
non-commercial use under the [PolyForm Noncommercial License 1.0.0](LICENSE)
(see [NOTICE](NOTICE) for copyright attribution).
Commercial use requires a separate written license from Luca La Barbera;
contact the author through the project repository.
