# Changelog

All notable Ciao changes are recorded here.

## [Unreleased]

No changes yet.

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
