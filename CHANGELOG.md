# Changelog

All notable Ciao changes are recorded here.

## [Unreleased]

No changes yet.

## [v0.1.32] - 2026-08-22

- Avoid contending for the shared Cloudflare lock when an app has no managed
  ingress route; unrelated deployments are no longer blocked by another app's
  exposure update.

## [v0.1.31] - 2026-08-22

- Reconcile existing Cloudflare ingress for projects with a declared
  `[tunnel]` before reporting the new release active; a locked or unavailable
  exposure now fails the deploy instead of being silently skipped.

## [v0.1.30] - 2026-08-22

- Make concurrent Cloudflare updates wait up to 30 seconds on the kernel lock
  instead of failing immediately.

## [v0.1.29] - 2026-08-22

- Migrate stale pre-0.1.28 directory locks safely when the kernel lock is
  first used, preventing old `.ciao-config-lock` state from bypassing the new
  lock protocol.

## [v0.1.28] - 2026-08-22

- Replace the Cloudflare directory lease with a kernel `flock` holder on
  `/etc/cloudflared/config.lock`; interrupted holders no longer leave a stale
  directory lock.
- Wait briefly for concurrent Cloudflare updates and fail the deployment when
  an existing exposure cannot be synchronized, allowing normal rollback.

## [v0.1.27] - 2026-08-22

- Warn on legacy `[tunnel].hostname` and normalize legacy tunnel keys to the
  `domain`/`name` schema during deployment.

## [v0.1.26] - 2026-08-22

- Extend `ciao host audit` with the local manifest versus remote public-domain
  coherence check when run from a project directory.

## [v0.1.25] - 2026-08-22

- Persist `ciao deploy --domain` in the project `ciao.toml`.
- Support `[tunnel].domain` and optional `[tunnel].name`, while accepting the
  legacy `hostname`/`tunnel` keys.
- Make `ciao domain add/remove` update the local project manifest.
- Expose the configured public domain in status and app listings.

## [v0.1.24] - 2026-08-22

- Remove ownerless Cloudflare lock directories robustly, including partially
  created directories where `rmdir` could not complete.

## [v0.1.23] - 2026-08-22

- Avoid acquiring the shared Cloudflare Tunnel lock twice during one deploy.
  Declared tunnels and explicit domains are configured once after activation,
  preventing false `another Ciao Cloudflare config update is already running`
  warnings from empty lock directories.

## [v0.1.22] - 2026-08-22

- Remove empty Cloudflare lock directories immediately after interrupted
  releases, avoiding false concurrent-update errors on the next deploy.

## [v0.1.21] - 2026-08-22

- Recover empty stale Cloudflare config locks left by interrupted SSH sessions.

## [v0.1.20] - 2026-08-21

- Preserve manual Cloudflare ingress routes when a legacy global ownership
  marker appears alongside already-marked Ciao routes.

## [v0.1.19] - 2026-08-21

- Make the Cloudflare Tunnel host-scoped with a deterministic `ciao-<host>`
  name; subsequent apps reuse the same tunnel, credentials and connector.
- Add a host-level lock around Cloudflare config read/merge/write/remove and
  recover locks left by an interrupted update after a bounded lease.
- Preserve manual ingress rules, mark the managed tunnel block and every Ciao
  route with ownership comments, and keep the catch-all rule last.
- Prefer existing `/etc/cloudflared/*.json` credentials on the target and
  avoid TTY-only login or installation paths in non-interactive deploys.
- Extend `ciao status` with tunnel name, real port and connector state; extend
  `ciao host audit` with DNS target, connector service and duplicate-process
  checks.

## [v0.1.18] - 2026-08-21

- Merge Cloudflare Tunnel ingress at host scope: every deploy upserts only the
  marked app route, preserves other apps and the final catch-all, writes the
  config through an atomic rename, and reloads the single shared service.
- Make Cloudflare DNS routing idempotent per hostname and keep app removal
  scoped to its own ingress rule while other apps remain online.
- Use the active release port for shared ingress updates, preserve remote
  tunnel credentials, and allow declared-Tunnel deploys from non-interactive
  shells when the host config is already usable.
- Extend `ciao host audit` to report every Cloudflare hostname, owner, upstream
  port, DNS status and dead-port or release-port drift.

## [v0.1.17] - 2026-08-21

- Fix Linux `current` activation: replace the symlink with `mv -fT`, verify the
  final target, and remove abandoned temporary pointers from app and release
  directories.
- Read the release and port served by the active systemd unit for `status`,
  `apps`, `releases`, logs, rollback, Funnel and Cloudflare synchronization;
  reconcile stale `current` and slot markers and report the real release after
  rollback recovery.
- Make Caddy, Funnel and Cloudflare route refreshes transactional. A failed
  proxy reload restores the previous fragment/config and no longer rolls back
  an otherwise healthy application deploy; the result includes a warning.
- Keep the previous service slot alive when a Caddy route cannot be reloaded,
  avoiding an unnecessary traffic black hole.
- Allow non-interactive Funnel setup to return the exact Tailscale login or
  approval URL. Native Cloudflare Tunnel setup can continue without a working
  local `:443` Caddy listener.
- Accept `ciao env set HOST=value` while preserving the stdin form for secrets;
  document both forms and add parser coverage.

## [v0.1.16] - 2026-08-21

- Do not require a TTY for Cloudflare Tunnel deploys when the local account is
  already authenticated. When the first login is missing, print the official
  login URL and the exact `cloudflared tunnel login` command instead of a
  generic terminal error.
- Keep JSON deploy output machine-readable by sending Cloudflare setup notices
  to stderr.
- Use the release manifest's effective port as the single source for the
  generated service start script, candidate healthcheck and activation checks;
  add a regression test for `[run].port`.

## [v0.1.15] - 2026-08-21

- Add project-owned Cloudflare Tunnel declarations with `[tunnel] hostname`
  and `tunnel` settings. Ciao writes a marked ingress, targets the active
  release port, reloads `cloudflared` after deploy and rollback, and reports
  the public hostname and port in `ciao status`.
- Probe declared Cloudflare hostnames through the public HTTPS chain with the
  configured `Host` header and retry window instead of probing a candidate
  loopback port directly.
- Remove Ciao-owned Cloudflare configuration during `ciao app remove` while
  leaving unmarked user configuration untouched; adopt older Ciao routes on a
  matching Caddy hostname during the next deploy.
- Extend `ciao host audit` with Cloudflare ingress, hostname resolution, dead
  upstream port and active-release drift checks alongside Caddy and Funnel
  exposure findings.
- Keep Caddy's Cloudflare route and the generated release start script aligned
  through the active release manifest, including stable `[run].port` services.

## [v0.1.14] - 2026-08-21

- Treat `[run].port` as a stable service port and verify explicit-port services
  after activation instead of silently replacing it with an allocator port.
- Synchronize Funnel Caddy routes after deploy and rollback, including a
  loopback Host-header smoke check and a guard against conflicting Ciao Funnel
  hostnames.
- Add secure-by-default Funnel token paths, declarative `[funnel]` settings and
  read-only Tailscale Funnel/Serve findings to `ciao host audit`.
- Remove Ciao-owned historical Serve endpoints during application removal
  without rewriting unrelated Tailscale configuration; add explicit
  `ciao host cleanup <host> --yes` for rename cleanup.
- Warn when service source appears to bind `0.0.0.0`, while keeping the check
  advisory and leaving application code unchanged.

## [v0.1.13] - 2026-08-21

- Require service deployments using Funnel to declare `[run].port` in
  `ciao.toml`; static Funnel apps remain exempt.
- Retry loopback and domain healthchecks during the configured timeout so a
  service can finish its normal startup before Ciao decides it is unhealthy.
- Reactivate the target release through its correct Linux service slot during
  rollback, keeping `current`, the active slot and the serving process aligned.
- Store SSH ControlMaster sockets in the short, private
  `~/.cache/ciao/ssh/control-%C` path instead of `$TMPDIR`.

## [v0.1.12] - 2026-08-21

- Add `ciao deploy <host> funnel` to install Tailscale when needed, configure a
  dedicated Caddy route and print a public Funnel URL after approval.
- Deploy Node services without a `build` script: dependencies are installed and
  the detected `start` script runs directly.

## [v0.1.11] - 2026-08-21

- Add interactive `logs --follow`, explicit release-target rollback and
  destructive application removal with confirmation.
- Add Linux blue/green service slots with Caddy route switching after the
  candidate healthcheck.
- Reuse SSH connections and prefer rsync for incremental source uploads, with
  the existing tar stream as a fallback.
- Keep Linux service-slot state in a private internal module and allow status,
  logs, release listing and lifecycle orchestration to run against any
  `RemoteHost` implementation.
- Split project detection, Caddy/domain handling, environment operations,
  lifecycle hooks, host audit and multi-component transactions into focused
  internal modules.
- Add compensating full-stack deploys, fixed project hooks, bulk environment
  pull/push/diff/generate and read-only `ciao host audit`.

## [v0.1.10]

- Skip the redundant Astro production build during `ciao run`; Astro dev still
  compiles current source files and keeps HMR active.
- Reuse local JavaScript dependencies while package manifests and lockfiles
  are unchanged.

## [v0.1.9]

- Prevent concurrent `ciao run` sessions for the same project from removing
  each other’s local Caddy route.
- Remove stale local-run locks after an interrupted process.

## [v0.1.8]

- Remove two unused legacy local-run wrappers and their obsolete test path.

## [v0.1.7]

- Bound captured process output to 64 KiB while continuing to drain pipes.
- Stop remote command capture on Ctrl-C and wait for the child cleanly.
- Remove unused Unicode-width support from the terminal spinner dependency.

## [v0.1.6]

- Proxy `ciao run` Astro projects to the foreground Vite/Astro server instead
  of serving the previous `dist` build through Caddy.
- Keep static site deploys unchanged: remote releases still build and serve
  the immutable `dist` release as usual.

## [v0.1.5]

- Make `ciao run` print one canonical `.localhost` address.
- Show a completed start message and a clear `Ctrl-C` instruction.
- Keep Astro development servers in the foreground and enable CSS/source hot
  reload.
- Stop Astro child processes and remove the temporary Caddy route on exit.
- Hide service-manager PIDs and other local setup output from the normal CLI.

## [v0.1.4]

- Generate GitHub Actions workflows from the detected project runtime and app
  type.
- Install a pinned, checksum-verified Ciao release binary in GitHub Actions.
- Remove the unconditional Rust toolchain and Ciao source checkout from app
  deploy workflows.
- Document the commit and push step required after `ciao github setup`.
- Keep Astro static deployments on the static project path.
- Change the project license to GNU AGPLv3.

## Release process

Before publishing a release, move the relevant entries from `Unreleased` into
a version section and include that section in the GitHub release notes.
