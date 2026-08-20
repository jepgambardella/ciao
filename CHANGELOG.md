# Changelog

All notable Ciao changes are recorded here.

## [Unreleased]

No changes yet.

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
