<div align="center">
  <h1>Ciao</h1>
  <p><strong>Write apps. Ship it. Ciao.</strong></p>
  <p>
    <a href="https://www.rust-lang.org/"><img src="https://img.shields.io/badge/built_with-Rust-dea584?logo=rust&logoColor=white" alt="Built with Rust"></a>
    <a href="https://www.apple.com/macos/"><img src="https://img.shields.io/badge/macOS-supported-111111?logo=apple&logoColor=white" alt="macOS supported"></a>
    <a href="https://www.kernel.org/"><img src="https://img.shields.io/badge/Linux-supported-FCC624?logo=linux&logoColor=black" alt="Linux supported"></a>
    <a href="LICENSE"><img src="https://img.shields.io/badge/license-AGPLv3-5c6ac4" alt="AGPLv3 license"></a>
  </p>
</div>

Ciao deploys your app from your computer to your server.

One command:

```bash
cd my-app
ciao deploy home
```

Ciao uses SSH. It uses systemd on Linux and launchd on macOS. It installs the
needed host tools. It keeps the old release active until the new release is
ready.

## Install

The public installer will be:

```bash
curl -fsSL https://raw.githubusercontent.com/jepgambardella/ciao/main/install.sh | sh
```

The GitHub repository is private during development. From this checkout, use:

```bash
./install.sh --local
```

The installer puts Ciao in `~/.local/bin` and adds that directory to your
shell path. It works on macOS and Linux. It does not need Homebrew.

## First deploy

Add a host once:

```bash
ciao host add home user@server
```

Then deploy:

```bash
cd my-app
ciao deploy home
```

The first deploy checks the host. It installs missing tools, including Caddy.
It also connects the host to Tailscale and adds `my-app.ciao` on this computer.
That address reaches the app on the server. The app does not run on this
computer. If SSH needs a password, Ciao opens the normal SSH prompt. Ciao does
not read or save the password.

For a public hostname on a Cloudflare-managed domain, use the same command
with `--domain`:

```bash
ciao deploy home --domain app.example.com
```

Ciao installs `cloudflared` when needed, opens the Cloudflare sign-in page once,
creates or reuses a standard tunnel, creates the DNS route and starts one
shared `cloudflared` service on the host.

For a tunnel that belongs to the project, declare the hostname and existing
tunnel name in `ciao.toml`:

```toml
[tunnel]
hostname = "tv.example.com"
tunnel = "abcmovie"
```

Ciao owns only that app's ingress rule in the host-level
`/etc/cloudflared/config.yml`. Each deploy reads the shared file, upserts the
hostname and the active release port, preserves every other app and the final
`http_status:404` catch-all, then atomically writes and reloads the one
`cloudflared` service. DNS routing is idempotent per hostname. A deploy or
rollback never replaces another app's route; `ciao app remove` removes only
the marked rule for that app and keeps the shared service while other routes
remain. Ciao checks the public hostname with the configured `Host` header and
shows the hostname, tunnel name, real port and connector state in `ciao status`.
Only one `[tunnel]`
declaration is accepted in a full-stack deploy; use separate Ciao apps when
the backend and frontend need separate hostnames.

On a new host the shared tunnel name is deterministic: `ciao-<host-name>`.
Later apps reuse that same tunnel and credential file. If the host already has
a valid Cloudflare config, Ciao adopts it without replacing manual ingress
rules.

For an existing host-level Tunnel, Ciao uses the tunnel and credentials already
referenced by `/etc/cloudflared/config.yml`; a local Cloudflare login is only
needed when creating the first shared Tunnel. This keeps deploys from CI and
other non-interactive shells safe.

When the public hostname is already routed by Cloudflare (or another edge
proxy) and Caddy must not own the domain, use `--skip-caddy`:

```bash
ciao deploy home --skip-caddy
```

Projects with a `[tunnel]` declaration select this behavior automatically, so
a stale Caddy fragment cannot poison later deploys. For a flaky SSH/Tailscale
connection, retry only the safe initial host inspection with `--retry 3`.

For a temporary public URL without a Cloudflare domain, add `funnel` after the
host:

```bash
ciao deploy home funnel
```

Ciao installs Tailscale on the host when needed, opens the Tailscale sign-in or
Funnel approval page when required, routes the deployed app through Caddy and
prints a stable `https://<host>.<tailnet>.ts.net` URL. Funnel is public to the
whole Internet. To turn this route off, run `sudo tailscale funnel --https=443 off`
on the host.

The deploy does not require a TTY when the target is already connected to
Tailscale and Funnel is already approved. In CI or another non-interactive run,
an unauthenticated target returns the exact Tailscale login or approval URL;
complete it once, then rerun the deploy.

For service apps, declare the application port in `ciao.toml` before using
Funnel:

```toml
[run]
port = 3000
```

This is the app port. Funnel itself forwards through Caddy on `127.0.0.1:80`.
Static apps do not need a service port. During a Linux blue/green redeploy,
Ciao keeps this explicit port stable and restarts the service in place; apps
without `[run].port` use blue/green internal ports. External traffic should
always target the Caddy/Funnel endpoint, not a release port directly.

For a declared Funnel that is enabled after every deploy, use the secure
default token route:

```toml
[app]
public = false

[funnel]
enabled = true
auth = "token" # the default; Ciao prints the generated URL once
```

Only an explicitly public app may disable the token:

```toml
[app]
public = true

[funnel]
enabled = true
auth = "none"
```

Ciao keeps Funnel on the machine hostname and synchronizes its Caddy upstream
after every deploy and rollback. A host can expose one Ciao Funnel hostname at
a time; use separate hosts for separate public apps.

On Linux, status and release listings read the release from the active systemd
slot. If an interrupted deploy left `current` or `active-slot` stale, Ciao
reports the serving release and repairs the bookkeeping during the next deploy
or rollback. Activation verifies the final symlink target; proxy reload errors
are reported as warnings and restore the previous route, so a healthy service
is not rolled back only because Caddy or a tunnel is unavailable.

Ciao also prints an advisory when it finds an explicit `0.0.0.0` bind in a
service source file. `HOST=127.0.0.1` is exported to the generated start
script, but some frameworks ignore it; bind explicitly to loopback when Caddy
or Funnel must be the only entry point.

Use a normal SSH key for later commands. Ciao can create the key and install
the public key during the guided host setup.

## Daily commands

```bash
ciao status home my-app
ciao logs home my-app
ciao app remove home my-app --yes
ciao restart home my-app
ciao rollback home my-app
```

Deploys are immutable. A failed build or health check does not replace the
active release. If a deploy is interrupted, Ciao removes its temporary state
or offers a safe lock recovery on the next deploy.

Service releases cannot contain writable runtime state. Before upload, Ciao
rejects SQLite-like files (`*.db`, `*.sqlite`, `*.sqlite3`, `*-wal`, `*-shm`,
and `*-journal`). Store that state below `CIAO_SHARED_DIR`; Ciao exports the
variable in the generated start script and service unit. Add a pattern to
`.ciaoignore` only after the application has been migrated to that path.

## Supported apps

- Rust
- Go
- Bun
- Node
- Python / Flask
- Astro static sites
- Static sites

Ciao detects the project. It also recognizes the common full-stack layout:

```text
my-app/
  backend/   # Flask or another supported Python service
  frontend/  # Next, Astro or another supported Node app
```

Run `ciao inspect` to see both components and their detected commands. Deploy
the monorepo root to activate backend and frontend as one compensating
transaction. The backend is activated first; the frontend is health-checked
second. If the second step fails, Ciao restores the backend release too. The
`--domain` route is assigned to the frontend; each component still gets its
own local `.ciao` route.

Add `ciao.toml` only when you need custom build, run, health check, domain,
release retention, environment, lifecycle hooks or a project-owned Cloudflare
Tunnel. For example:

```toml
[releases]
keep = 8

[hooks]
pre_upload = "scripts/backup-db.sh"
pre_activate = "ciao run-remote bin/rails db:migrate"
post_activate = "scripts/notify-deploy.sh"
on_rollback = "scripts/notify-rollback.sh"

[tunnel]
hostname = "tv.example.com"
tunnel = "abcmovie"
```

The generated service environment includes:

```text
CIAO_SHARED_DIR=/var/lib/ciao/apps/<app>/shared
```

Use it for SQLite files, uploads, indexes and other writable application data.

`pre_upload` runs on the local computer. The other hooks run remotely as the
application user, in the candidate or active release directory. Hooks are
project commands, like build and start commands, and their output is bounded.

Bulk environment management avoids copying secrets through the terminal:

```bash
printf '%s' 'secret-value' | ciao env set home my-app API_TOKEN
ciao env set home my-app API_TOKEN=secret-value # convenient for non-secret values
ciao env pull home my-app              # names only, writes .env.ciao
ciao env pull home my-app --with-values
ciao env diff home my-app
ciao env push home my-app --yes
ciao env generate home my-app JWT_SECRET
```

`env push` shows key-only additions, changes and removals and requires
confirmation. `env generate` uses the operating system CSPRNG and never prints
the generated value. `env set` accepts either `KEY=value` or a bare `KEY`; the
bare form reads the value from stdin and is the safer choice for secrets.

Inspect host drift without changing anything:

```bash
ciao host audit home
ciao host audit home --diff
```

The audit checks Caddy imports/routes, managed service definitions, sudoers and
orphaned Ciao files. It also inspects Tailscale Funnel/Serve rules and flags
dead local ports, hostnames that no longer resolve, and public Funnel routes
without declared authentication. It is read-only.

After renaming a machine, remove only stale local Ciao endpoints from the
editable Tailscale Serve configuration with an explicit confirmation:

```bash
ciao host cleanup home --yes
```

The cleanup is limited to Ciao's managed port range and preserves ports still
referenced by retained releases. It never rewrites Tailscale configuration
without `--yes`.

## Local development

For a temporary test on this computer, use:

```bash
ciao run
```

Ciao detects the project, installs dependencies, runs the detected build when
the project defines one, then starts it on
`http://<project>.localhost`. Ciao keeps the app on an internal loopback port
and uses the local Caddy proxy to hide it. Ciao prints one address and tells you
how to stop the server. For Astro, Ciao starts the Astro development server
after the first build, so CSS and source changes reload automatically.

On Linux service deployments, Ciao keeps two service slots and flips Caddy to
the healthy slot before stopping the old one. Source uploads use `rsync` when
available on both ends and fall back to the portable tar stream otherwise.

Run an app locally with a stable `.ciao` address:

```bash
ciao dev
```

Ciao configures the local DNS and Caddy setup. A project named `my-app` uses
`http://my-app.ciao`. This mode is for a process running on this computer. The
remote route created by `ciao deploy` does not use local Caddy.

## GitHub and Tailscale

After a manual deploy, enable optional private-network auto-deploy:

```bash
ciao github setup --host home
```

Ciao reuses Tailscale when it exists. Otherwise it installs it and guides the
sign-in in a browser. GitHub Actions then runs the same deploy engine over
Tailscale and SSH.

The generated workflow is specific to the detected project. Ciao records the
runtime and app type (for example, `Astro` + `static`, or `Go` + `service`),
downloads one pinned Ciao release binary, verifies its checksum, and deploys.
It does not install Rust in the GitHub runner for an Astro, Node, static, Go or
other non-Rust project.

After setup, commit and push the generated workflow:

```bash
git add .github/workflows/ciao-deploy.yml
git commit -m "enable Ciao auto-deploy"
git push
```

Every push to the configured branch then runs the normal Ciao deploy path over
the private Tailscale network.

See [the GitHub and Tailscale guide](docs/Ciao_GitHub_Tailscale_Autodeploy.md).

## Safety

Ciao does not install a remote daemon. It does not store private SSH keys. It
does not expose app ports by default. It does not offer a generic remote shell
through MCP. Secrets are sent over SSH and are not printed.

## Documentation

- [Architecture](docs/architecture.md)
- [Security](docs/security.md)
- [Local `.ciao` domains](docs/local_ciao_domain.md)
- [Testing](TESTING.md)
- [Changelog](CHANGELOG.md)
- [Product source of truth](Ciao.md)

Run the checks:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets
```

## License

Copyright © 2026 Luca La Barbera.

Ciao is free software licensed under the
[GNU Affero General Public License, version 3 (AGPLv3)](LICENSE).
See [NOTICE](NOTICE).
