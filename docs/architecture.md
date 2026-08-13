# Architecture

The implemented product has three layers:

```text
ciao CLI ─┐
              ├── ciao-core ── OpenSSH ── systemd/launchd/Caddy
ciao MCP ─┘
```

`ciao-core` owns configuration, validation, project detection, release
planning, OpenSSH transport, health checks, lifecycle and rollback. The CLI and
MCP call those functions directly; MCP does not execute the CLI or parse human
output. The MCP server is a local JSON-RPC-over-stdio process.

The local development path also uses the core: it detects the project, chooses
and persists the internal port, renders the Caddy fragment, and runs the local
process. The CLI only supplies the terminal-facing process loop. Resolver and
proxy setup use dnsmasq, Caddy and the native package/service facilities of the
host; Ciao does not become a DNS server or a reverse-proxy implementation.

The remote layout is intentionally ordinary:

```text
/var/lib/ciao/apps/<app>/
  releases/<release>/
  current -> releases/<release>
  shared/env

/var/cache/ciao/<app>/
  build and package-manager cache (owned by the app user)
```

On macOS the corresponding cache is `/Library/Caches/Ciao/<app>/`. Build and
install commands use that directory as `HOME`; release contents remain
immutable and the user's personal home directory is never used.

Linux uses a generated `ciao-<app>.service` and journald. macOS uses a
generated LaunchDaemon plist. `ciao deploy` runs the complete native bootstrap
when the readiness check finds missing prerequisites, through one
`ssh -tt host sh -c ...` session; the password stays
inside OpenSSH and the SSH user is preserved for Homebrew. Deploy/lifecycle, CI
and MCP use independent non-interactive SSH commands and therefore require
`sudo -n`. Ciao never stores or asks for a private key
passphrase in its state. If the user opts into guided host linking, Ciao
creates a standard local OpenSSH identity and stores only its path. There is
no resident Ciao process on the remote host. Host initialization installs Caddy
through the native package manager and configures only Ciao's fragment import;
the normal deploy path invokes that same idempotent initialization automatically
when the host is not ready.

For local `.ciao` development, macOS uses Homebrew's Caddy/dnsmasq services,
`/etc/resolver/ciao`, and a tiny launchd job to restore its loopback alias;
Linux uses dnsmasq plus systemd-resolved's `~ciao` routing domain. No
per-project `/etc/hosts` entries are created.

`ciao apps`, `ciao releases` and the temporary `ciao ui` view read
this same filesystem/service state through the shared core. The UI binds only
to loopback and is read-only; it is not a remote dashboard or control plane.

Cloudflare Tunnel remains an optional external integration. Ciao does not
create accounts, store tunnel credentials, or replace `cloudflared` service
management. A future narrow adapter may operate on an explicitly supplied
existing tunnel configuration.

GitHub auto-deploy is another thin client of the same core. GitHub Actions
checks out the application and a pinned Ciao revision, connects to the target
through the official Tailscale action, configures ordinary OpenSSH and invokes
`ciao deploy --ci`. It adds no Ciao daemon, webhook receiver, scheduler or
remote control plane. `ciao github setup` detects or installs the Tailscale
client on the local device and target; when either node is not authenticated it
starts the normal Tailscale login flow, opens the browser and waits for the
node to become connected.

## Deployment states

```text
detect → upload → build → candidate → healthcheck → activate → prune
```

Only activation changes `current` or the stable service. Build and health
failures clean the candidate and preserve the active release. A Linux service
candidate is managed temporarily for its health check; the current macOS
candidate path runs the candidate script directly and checks its HTTP endpoint
before stable activation.

Automated coverage is currently in the core unit tests and a CLI/MCP vertical
smoke test in CI. Remote deployment coverage is a separate opt-in fixture and
must not reboot the host.
