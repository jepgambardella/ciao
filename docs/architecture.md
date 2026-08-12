# Architecture

The implemented product has three layers:

```text
ciaoship CLI ─┐
              ├── ciaoship-core ── OpenSSH ── systemd/launchd/Caddy
ciaoship MCP ─┘
```

`ciaoship-core` owns configuration, validation, project detection, release
planning, OpenSSH transport, health checks, lifecycle and rollback. The CLI and
MCP call those functions directly; MCP does not execute the CLI or parse human
output. The MCP server is a local JSON-RPC-over-stdio process.

The local development path also uses the core: it detects the project, chooses
and persists the internal port, renders the Caddy fragment, and runs the local
process. The CLI only supplies the terminal-facing process loop. Resolver and
proxy setup use dnsmasq, Caddy and the native package/service facilities of the
host; CiaoShip does not become a DNS server or a reverse-proxy implementation.

The remote layout is intentionally ordinary:

```text
/var/lib/ciaoship/apps/<app>/
  releases/<release>/
  current -> releases/<release>
  shared/env
```

Linux uses a generated `ciaoship-<app>.service` and journald. macOS uses a
generated LaunchDaemon plist. The deploying SSH account must have
non-interactive `sudo -n`; CiaoShip never stores or asks for a private key
passphrase. There is no resident CiaoShip process on the remote host. Explicit
host initialization installs Caddy through the native package manager and
configures only CiaoShip's fragment import; a deploy with `--domain` invokes
that initialization automatically.

For local `.ciao` development, macOS uses Homebrew's Caddy/dnsmasq services
and `/etc/resolver/ciao`; Linux uses dnsmasq plus systemd-resolved's `~ciao`
routing domain. No per-project `/etc/hosts` entries are created.

`ciaoship apps`, `ciaoship releases` and the temporary `ciaoship ui` view read
this same filesystem/service state through the shared core. The UI binds only
to loopback and is read-only; it is not a remote dashboard or control plane.

Cloudflare Tunnel remains an optional external integration. CiaoShip does not
create accounts, store tunnel credentials, or replace `cloudflared` service
management. A future narrow adapter may operate on an explicitly supplied
existing tunnel configuration.

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
