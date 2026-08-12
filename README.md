# CiaoShip

> Ship apps. Skip the ops.

CiaoShip is a local CLI and stdio MCP server for deploying small Rust, Go,
Bun, Node and static applications to a Linux or macOS host over the installed
OpenSSH client. Service releases are immutable and run as ordinary systemd or
launchd services; CiaoShip does not leave a daemon on the host.

The intended workflow is:

```bash
cargo install --path crates/ciaoship
ciaoship host add home user@server
ciaoship host init home
cd my-project
ciaoship inspect
ciaoship deploy home
ciaoship status home my-project
ciaoship logs home my-project
ciaoship rollback home my-project
ciaoship apps home
ciaoship releases home my-project
```

No CiaoShip daemon, management port, private-key store, container runtime or
remote database is installed. Applications run as a dedicated Unix user and
remain ordinary system services when the local binary is not running.

## Requirements

- Rust 1.80 or newer to build CiaoShip.
- An OpenSSH client locally and an SSH account on the target.
- Linux hosts need `systemd` and an SSH account allowed to use passwordless
  `sudo -n`. macOS hosts use `launchd` and the same server-side administrative
  permission.
- `ciaoship host init` is idempotent: it installs CiaoShip's small native
  prerequisites and Caddy on the target. On macOS it detects Homebrew in the
  standard Apple Silicon/Intel locations and installs it when missing. A deploy
  with `--domain` performs this initialization automatically.
- The detected Rust, Go, Node or Bun runtime is installed only when that
  runtime is needed by a deploy. Static deployments do not install a runtime. 

## Configuration

Hosts are stored in `~/.config/ciaoship/config.toml`:

```toml
[hosts.home]
ssh = "user@server"

[mcp]
profile = "operator"
```

The project can opt into a small `ciaoship.toml` when detection is not enough:

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
host as stdin to a fixed `sh -s` invocation; CiaoShip never builds an SSH
command by concatenating host or application identifiers.

## Local development

Run this from a supported project:

```bash
ciaoship dev
```

CiaoShip installs and configures Homebrew (macOS when absent), dnsmasq and
Caddy on the first run. It creates one resolver for the whole namespace:
`*.ciao -> 127.0.0.1`. There is no per-project `/etc/hosts` edit. The project
name becomes a stable URL such as `http://my-api.ciao`; an internal port is
chosen and persisted in the local configuration. If a preferred port is
occupied, CiaoShip selects another free port automatically.
`ciaoship dev --name admin` overrides the local name without changing project
metadata.

The local process runs in the foreground. Caddy owns the HTTP entrypoint and
routes the Host header to the selected loopback port. Static projects keep
their Caddy route after the command exits; service routes are removed when the
process stops. The local setup uses native OS services and is safe to repeat.

## Safety model

- Host and application identifiers are validated before becoming paths or
  service names.
- SSH options are fixed and private keys are never read or stored.
- Deployments upload to a new staging directory, build there, health-check a
  candidate service, and only then replace `current`. If a domain is supplied,
  CiaoShip initializes Caddy before writing its own fragment.
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
add that narrow adapter separately. CiaoShip does not manage Cloudflare
accounts, DNS, tunnel credentials or the remote `cloudflared` service.

Run the local checks with `cargo fmt --all -- --check`, `cargo clippy
--workspace --all-targets -- -D warnings`, and `cargo test --workspace
--all-targets`. CI also builds the CLI, exercises project detection plus the
MCP stdio handshake against a temporary fixture, and tests natively on
Apple Silicon. SSH deployment tests require a separately provisioned
disposable host; see [TESTING.md](TESTING.md). Live tests must never reboot a
host.

See [docs/architecture.md](docs/architecture.md), [docs/security.md](docs/security.md),
[docs/local_ciao_domain.md](docs/local_ciao_domain.md), and the original product
source of truth in [Ciaoship.md](Ciaoship.md).
