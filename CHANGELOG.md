# Changelog

All notable Ciao changes are recorded here.

## [Unreleased]

No changes yet.

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
