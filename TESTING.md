# Testing

`cargo test --workspace --all-targets` is the repository’s current automated
test suite. It runs locally and in Ubuntu CI without a remote host, covering
identifier and SSH validation, project detection and configuration overrides,
local `.ciao` name/port/proxy planning, release manifests, config
round-tripping, generated systemd/launchd definitions, and shell-script
quoting. Astro static detection and the temporary `ciao run` script are also
covered. The local setup script is inspected for native dependencies and for
the absence of per-project `/etc/hosts` edits; tests do not mutate the
developer's DNS or service manager.

CI adds a small vertical smoke test: it builds the `ciao` binary, detects a
temporary project through the CLI, then sends `initialize` and `tools/call`
(`inspect_app`) requests through the real MCP stdio process. This checks the
CLI/core/MCP wiring without mutating a host. CI runs these checks on Ubuntu and
runs the workspace tests natively on an arm64 macOS runner. The local UI is a
thin read-only view over `list_apps`; it is intentionally not a second control
plane.

## Live SSH tests

Live deployment tests are opt-in and require a separately provisioned,
disposable host. The fixture should provide an SSH account with passwordless
`sudo -n`, `systemd` or `launchd`, `tar`, `curl` or `wget`, and the toolchain
for the fixture application. Exercise first deploy, a second immutable
release, failed build and health-check preservation, status/logs, restart
after killing the process, rollback, and release pruning. Verify the host
state after every operation and clean up only the fixture application.
Also interrupt one deploy with a normal Ctrl-C during upload or install and
verify that local `ciao`/`tar`/`ssh` children, remote upload/build processes,
the deployment lock, and the candidate staging/release directory are gone.
Do not use SIGKILL for this check; a forced kill cannot execute cleanup and is
covered by the next-deploy lock recovery prompt.

For a temporary manual test, grant only the fixture account a short-lived
`NOPASSWD` rule, verify `sudo -n true`, run the vertical slice, then revoke the
rule. Do not paste passwords or private keys into the repository or test logs.

The guided `ciao host add` SSH bootstrap is tested separately from deployment:
it must use the installed OpenSSH client, keep the private identity local with
mode `0600`, install only the public key, and verify a subsequent
non-interactive connection. Tests must use a disposable account and must never
capture or print the password.

Live tests must not reboot, shut down, suspend, or otherwise disrupt the host.
Reboot persistence is intentionally not exercised by the repository’s live
test contract: the test must verify `systemctl enable`/LaunchDaemon
configuration, but must not reboot the host. Do not add a reboot step to CI or
a manual fixture.

If a required host or service manager is unavailable, report
`skipped: <reason>`; a green local or CI unit run is not evidence that remote
systemd, journald, launchd, or SSH behavior was exercised. The repository does
not start Docker or install a remote daemon as part of its tests. A Cloudflare
test is opt-in. It must use a disposable hostname and verify the guided login,
DNS route, target credentials mode `0600`, standard service status and HTTPS
response. Do not put Cloudflare credentials in test logs.
