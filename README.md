<div align="center">
  <h1>Ciao</h1>
  <p><strong>Write apps. Ship it. Ciao.</strong></p>
  <p>
    <a href="https://www.rust-lang.org/"><img src="https://img.shields.io/badge/built_with-Rust-dea584?logo=rust&logoColor=white" alt="Built with Rust"></a>
    <a href="https://www.apple.com/macos/"><img src="https://img.shields.io/badge/macOS-supported-111111?logo=apple&logoColor=white" alt="macOS supported"></a>
    <a href="https://www.kernel.org/"><img src="https://img.shields.io/badge/Linux-supported-FCC624?logo=linux&logoColor=black" alt="Linux supported"></a>
    <a href="LICENSE"><img src="https://img.shields.io/badge/license-personal_use_only-5c6ac4" alt="Personal use only"></a>
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
If SSH needs a password, Ciao opens the normal SSH prompt. Ciao does not read
or save the password.

Use a normal SSH key for later commands. Ciao can create the key and install
the public key during the guided host setup.

## Daily commands

```bash
ciao status home my-app
ciao logs home my-app
ciao restart home my-app
ciao rollback home my-app
```

Deploys are immutable. A failed build or health check does not replace the
active release. If a deploy is interrupted, Ciao removes its temporary state
or offers a safe lock recovery on the next deploy.

## Supported apps

- Rust
- Go
- Bun
- Node
- Static sites

Ciao detects the project. Add `ciao.toml` only when you need custom build,
run, health check, domain or environment settings.

## Local development

Run an app locally with a stable `.ciao` address:

```bash
ciao dev
```

Ciao configures the local DNS and Caddy setup. A project named `my-app` uses
`http://my-app.ciao`. The setup is automatic and can be customized later.

## GitHub and Tailscale

After a manual deploy, enable optional private-network auto-deploy:

```bash
ciao github setup --host home
```

Ciao reuses Tailscale when it exists. Otherwise it installs it and guides the
sign-in in a browser. GitHub Actions then runs the same deploy engine over
Tailscale and SSH.

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
- [Product source of truth](Ciao.md)

Run the checks:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets
```

## License

Copyright © 2026 Luca La Barbera.

Ciao is available for personal, non-commercial use under the
[PolyForm Noncommercial License 1.0.0](LICENSE). Commercial use needs a
separate written license from Luca La Barbera. See [NOTICE](NOTICE).
