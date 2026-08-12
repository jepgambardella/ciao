# Ciao

> **Ship apps. Skip the ops.**

A tiny, fast, source-available deployment tool for running applications on your own Linux or macOS machines without Docker, Kubernetes, or a permanently running control plane.

Ciao should make this workflow almost trivial:

```bash
ciao host add home user@192.168.1.50
ciao deploy home --domain app.example.com
```

Expected result:

```text
✓ project detected: Bun
✓ release uploaded
✓ dependencies installed
✓ build completed
✓ service created
✓ restart-on-failure enabled
✓ start-on-boot enabled
✓ reverse proxy configured
✓ HTTPS enabled
✓ healthcheck passed

https://app.example.com
```

The philosophy:

> **Use the operating system. Hide the boring parts.**

Ciao must treat **Apple Silicon (`arm64`) as a first-class target**, not as an afterthought.

Ciao is not a PaaS, not a container orchestrator, and not a new server runtime.

It is a thin, opinionated deployment layer over mature Linux primitives.

---

# 1. Product thesis

Deploying a small application to a personal server should not require:

```text
Docker
Docker Compose
Kubernetes
a container registry
a hosted control plane
a CI/CD platform
manual systemd units
manual reverse-proxy configuration
manual TLS setup
manual log management
manual rollback scripts
```

For many projects, the real requirement is:

```text
copy application
build it
start it
keep it alive
restart it on failure
start it on boot
expose it safely
show logs
allow rollback
```

Linux already knows how to do almost all of this.

Ciao should combine those primitives behind one coherent UX.

---

# 2. Core promise

Ciao should feel closer to:

```bash
vercel deploy
```

than to a long SSH checklist.

The developer thinks about the application.

Ciao handles the operational glue.

The happy path must remain one or two commands:

```bash
ciao host add home user@server
ciao deploy home
```

Optional public exposure:

```bash
ciao deploy home --domain app.example.com
```

If ordinary deployments routinely need more than this, the UX is drifting.

---

# 3. What Ciao is not

This is the main anti-derailment constraint.

Do not slowly turn Ciao into:

```text
mini Kubernetes
mini Heroku
mini Coolify
mini Docker
mini GitHub Actions
mini Terraform
mini Nomad
```

Ciao should remain:

> **A very small deployment manager for individual Linux servers.**

No mandatory hosted account.

No proprietary control plane.

No mandatory remote dashboard.

No container registry.

No custom package format.

No cluster scheduler in v1.

No Docker dependency in the core architecture.

---

# 4. Architecture

The first version should integrate mature system components instead of replacing them.

```text
                    CIAO CLI
                         │
                         │ SSH
                         ▼
                    remote machine
                         │
                  OS auto-detection
                         │
             ┌───────────┴───────────┐
             │                       │
           Linux                   macOS
             │                       │
          systemd                 launchd
          journald          native logs/files
             │                       │
             └───────────┬───────────┘
                         │
                       Caddy
                         │
                    application
```

Optional:

```text
Cloudflare Tunnel
        │
        ▼
application on localhost
```

Ciao is the orchestration and UX layer.

---

# 5. Design principle: use boring infrastructure

Ciao should deliberately rely on boring, well-understood primitives.

## Process lifecycle

Use:

```text
systemd
```

for:

```text
start on boot
restart on failure
graceful stop
service dependencies
environment
process ownership
resource controls
```

Do not write a custom supervisor in v0.1.

## Logs

Use:

```text
journald
```

Application stdout/stderr naturally becomes:

```bash
ciao logs myapp
ciao logs myapp --follow
ciao logs myapp --since 10m
```

Do not build a logging database.

## Public HTTP/HTTPS

Use:

```text
Caddy
```

for:

```text
reverse proxy
automatic HTTPS
certificate renewal
HTTP → HTTPS
```

Do not implement TLS or a reverse proxy.

---


## OS abstraction

The core deployment engine must not hardcode Linux-specific behavior.

Use host abstractions such as:

```rust
trait ServiceManager {
    async fn install(&self, service: &ServiceSpec) -> Result<()>;
    async fn start(&self, name: &str) -> Result<()>;
    async fn stop(&self, name: &str) -> Result<()>;
    async fn restart(&self, name: &str) -> Result<()>;
    async fn status(&self, name: &str) -> Result<ServiceStatus>;
}

trait LogProvider {
    async fn read(&self, query: LogQuery) -> Result<LogStream>;
}
```

Initial backends:

```text
Linux
  ServiceManager → systemd
  LogProvider     → journald

macOS
  ServiceManager → launchd
  LogProvider     → macOS-compatible log/file backend
```

Everything above this layer remains shared:

```text
deploy
releases
health checks
rollback
ports
domains
MCP
dashboard
project detection
```

# 6. systemd integration

A Ciao-managed app should ultimately be a normal Linux service.

Conceptually:

```ini
[Unit]
Description=Ciao app myapp
After=network.target

[Service]
User=ciao-myapp
WorkingDirectory=/var/lib/ciao/apps/myapp/current
EnvironmentFile=/var/lib/ciao/apps/myapp/shared/env
ExecStart=/var/lib/ciao/apps/myapp/current/start
Restart=on-failure
RestartSec=2

[Install]
WantedBy=multi-user.target
```

Users should never need to write this manually.

Ciao must only manage its own units:

```text
/etc/systemd/system/ciao-*.service
```

Never modify unrelated services.

---


# macOS / launchd integration

macOS is an official deployment target.

For machines used as always-on servers, Ciao should prefer **LaunchDaemons** so applications can start at boot without requiring a logged-in desktop session.

Conceptually, Ciao generates a plist like:

```xml
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>dev.ciao.myapp</string>

    <key>ProgramArguments</key>
    <array>
        <string>/Library/Ciao/apps/myapp/current/start</string>
    </array>

    <key>WorkingDirectory</key>
    <string>/Library/Ciao/apps/myapp/current</string>

    <key>KeepAlive</key>
    <true/>

    <key>RunAtLoad</key>
    <true/>
</dict>
</plist>
```

Suggested managed paths:

```text
/Library/Ciao/apps/
/Library/LaunchDaemons/dev.ciao.*.plist
```

For user-scoped setups, a future mode may use:

```text
~/Library/LaunchAgents/
```

but the server-oriented default should prioritize boot persistence.

**Apple Silicon is mandatory compatibility:**

```text
macOS arm64
→ first-class target

macOS x86_64
→ supported where practical
```

The codebase must never assume x86_64 when generating paths, binaries, build commands, runtime metadata, or architecture checks.

---

# 7. Caddy integration

Ciao should manage only its own generated fragments.

Example:

```text
/etc/caddy/ciao/
  myapp.caddy
```

Main Caddy configuration can import:

```text
import /etc/caddy/ciao/*.caddy
```

Architecture:

```text
app.example.com
      ↓
    Caddy
      ↓
127.0.0.1:41827
      ↓
     app
```

Command:

```bash
ciao domain add home myapp app.example.com
```

DNS remains the user's responsibility unless a provider adapter is explicitly configured.

---

# 8. Cloudflare Tunnel

Cloudflare Tunnel should be a first-class optional exposure method for homeservers.

Example:

```bash
ciao expose home myapp --cloudflare app.example.com
```

Architecture:

```text
Internet
   ↓
Cloudflare
   ↓
outbound tunnel
   ↓
homeserver
   ↓
127.0.0.1:41827
```

The server does not need a publicly reachable IP.

Ciao should support both:

```text
normal public VPS → Caddy
homeserver/private network → Cloudflare Tunnel
```

Do not turn Ciao into a full Cloudflare administration client.

Keep the adapter narrow.

---

# 9. First server registration

```bash
ciao host add home luca@192.168.1.50
```

Expected output on Linux:

```text
✓ SSH connection
✓ OS: Linux
✓ service manager: systemd
✓ architecture: x86_64
✓ Ciao directories ready

Host `home` ready.
```

Expected output on Apple Silicon:

```text
✓ SSH connection
✓ OS: macOS
✓ service manager: launchd
✓ architecture: arm64 (Apple Silicon)
✓ Ciao directories ready

Host `studio` ready.
```

Remote layout should remain understandable:

```text
/var/lib/ciao/
/etc/ciao/
/etc/systemd/system/
```

No Ciao daemon is required on the remote machine in v0.1.

---

# 10. SSH-first control model

The default control path is:

```text
developer machine
      │
      │ SSH
      ▼
Linux server
```

Benefits:

```text
no exposed administration port
no remote daemon upgrades
reuse existing authentication
works with existing SSH config
small attack surface
easy debugging
```

Ciao should reuse normal OpenSSH configuration:

```text
~/.ssh/config
SSH keys
SSH agent
hardware keys
ProxyJump
custom ports
host aliases
```

Pragmatic v0.1 recommendation:

> Use the installed OpenSSH client instead of implementing SSH cryptography.

When the first SSH login is not ready, `ciao host add` may guide the user
through a one-time key bootstrap. It creates a standard Ed25519 identity in
the user's local `~/.ssh/ciao/` directory, opens one normal OpenSSH session so
the user can approve the host key and enter the server password, installs only
the public key in `authorized_keys`, and verifies a subsequent key-only login.
The password is handled by OpenSSH; Ciao never receives or stores it. The
normal OpenSSH path remains the default, and `--non-interactive` disables the
prompt for automation.

Privileged CLI operations follow the same boundary. `ciao host init` opens one
`ssh -tt host sh -c ...` session, runs `sudo -v` and the complete bootstrap
under the same remote sudo TTY, so the password stays inside OpenSSH. Deploy and lifecycle
operations use independent non-interactive SSH commands and therefore require
`sudo -n`; CI and MCP require it as well because they cannot safely prompt.

A native SSH transport can be reconsidered later.

---

# 11. Deploy from a project

Inside a repository:

```bash
ciao deploy home
```

Ciao should infer:

```text
runtime
install command
build command
start command
artifact strategy
port behavior
healthcheck
```

Suggested detection:

```text
Cargo.toml
→ Rust

go.mod
→ Go

bun.lock / bun.lockb
→ Bun

pnpm-lock.yaml
→ Node/pnpm

package-lock.json
→ Node/npm

yarn.lock
→ Node/Yarn
```

Recommended v0.1 support:

```text
Rust
Go
Bun
Node
static sites
```

Python can follow later.

---

# 12. Configuration philosophy

Use zero config when detection is unambiguous.

Use a tiny config when necessary.

Example:

```toml
# ciao.toml

[app]
name = "my-api"

[run]
command = "bun start"
port = 3000
```

More advanced:

```toml
[build]
install = "bun install --frozen-lockfile"
command = "bun run build"

[run]
command = "bun start"
port = 3000

[health]
path = "/health"
timeout = "10s"
```

Do not create a giant YAML DSL.

---

# 13. Immutable releases

Deployments should be immutable releases.

Remote layout:

```text
/var/lib/ciao/apps/myapp/
  releases/
    20260812-103012-a81cd2/
    20260812-111411-c194ff/

  current -> releases/20260812-111411-c194ff

  shared/
    env
    data/
```

Never overwrite the active release in place.

This makes rollback simple and safe.

---

# 14. Deployment pipeline

Recommended flow:

```text
local project
     ↓
project detection
     ↓
deployment plan
     ↓
source package
     ↓
SSH upload
     ↓
new release directory
     ↓
install dependencies
     ↓
build
     ↓
allocate internal port
     ↓
start candidate release
     ↓
healthcheck
     ↓
activate candidate
     ↓
switch proxy
     ↓
stop previous release
     ↓
prune old releases
```

A failed candidate must not replace a working release.

---

# 15. Atomic activation

For web applications:

```text
old release → :41827
        │
        │ still serving
        ▼

new release → :41828
        ↓
healthcheck
        ↓
Caddy upstream switch
        ↓
graceful stop old release
```

This allows near-zero-downtime deployment for supported workloads.

Do not promise universal zero downtime.

---

# 16. Health checks

Default HTTP check:

```text
GET /
```

Optional config:

```toml
[health]
path = "/health"
expected_status = 200
timeout = "10s"
```

For background workers:

```text
process-alive check
```

A candidate does not become active until the health check passes.

---

# 17. Rollback

Rollback is a first-class operation:

```bash
ciao rollback home myapp
```

No rebuild.

Expected flow:

```text
previous release
      ↓
start
      ↓
healthcheck
      ↓
proxy switch
      ↓
stop current release
```

A deploy system without trivial rollback is incomplete.

---

# 18. Port management

Apps should bind to localhost by default.

Ciao assigns and tracks internal ports:

```text
myapp
→ 127.0.0.1:41827
```

Possible managed range:

```text
41000–49000
```

The user should not normally manage internal ports manually.

Public traffic arrives through:

```text
Caddy
or
Cloudflare Tunnel
```

---

# 19. Static applications

If output is fully static, avoid running a process.

Example:

```text
dist/
build/
public/
```

Ciao should serve the files directly through Caddy.

Result:

```text
application runtime RAM:
0
```

This is an important optimization.

---

# 20. Compiled applications

Rust:

```text
cargo build --release
→ native binary
```

Go:

```text
go build
→ native binary
```

The resulting process should be executed directly by systemd.

No wrapper runtime.

This is the ideal Ciao workload:

```text
binary
+
systemd
+
Caddy
```

---

# 21. Bun / Node applications

For JS applications Ciao does not alter runtime semantics.

Example:

```text
bun install
bun run build
bun start
```

or:

```text
npm ci
npm run build
node server.js
```

Ciao manages:

```text
release
lifecycle
logs
health
domain
rollback
```

not application behavior.

---

# 22. Upload strategy

Do not require Git on the server.

Recommended v0.1:

```text
tar stream over SSH
```

Flow:

```text
local project
↓
apply ignore rules
↓
tar stream
↓
SSH
↓
extract into release
```

Support:

```text
.gitignore
.ciaoignore
```

Avoid requiring `rsync`.

---

# 23. Build location

Default:

> Build on the remote server.

Advantages:

```text
architecture matches target
native dependencies match target
no cross-compilation requirement
no artifact registry
simple mental model
```

Later:

```bash
ciao deploy --build-local
```

may be useful for CI or compiled binaries.

Do not add artifact registries in v0.1.

---

# 24. Secrets and environment

Commands:

```bash
ciao env set home myapp DATABASE_URL
ciao env set home myapp API_KEY
ciao env unset home myapp API_KEY
```

Never print secret values by default.

Remote file:

```text
/var/lib/ciao/apps/myapp/shared/env
```

Permissions:

```text
0600
```

Future optional integrations can include:

```text
1Password
Bitwarden
SOPS
systemd credentials
```

Not required initially.

---

# 25. Unix users

Do not run deployed applications as root.

Preferred design:

```text
one Unix user per application
```

Example:

```text
ciao-myapp
```

Benefits:

```text
filesystem isolation
process isolation
clear ownership
smaller blast radius
```

A shared `ciao` user may be acceptable in an early prototype, but per-app users are the better target.

---

# 26. State storage

Avoid a remote database service.

Use:

```text
SQLite
```

only when state becomes sufficiently complex.

Potential state:

```text
apps
releases
ports
domains
deployment events
configuration hashes
```

However, authoritative state should remain recoverable from:

```text
filesystem
release manifests
systemd units
proxy fragments
```

Avoid an opaque control-plane database.

---

# 27. Release manifests

Each release should contain generated metadata.

Example:

```toml
release = "c194ff"
app = "myapp"
runtime = "bun"
commit = "17ad32f"

[commands]
install = "bun install --frozen-lockfile"
build = "bun run build"
run = "bun start"

[network]
port = 41827
```

Useful for:

```text
rollback
recovery
debugging
dashboard
MCP
```

---

# 28. Git metadata

When available, record:

```text
commit
branch
dirty state
```

But do not require Git.

Ciao deploys the current project snapshot, not necessarily a remote repository.

---

# 29. Failure diagnostics

A failed deploy must answer:

```text
what failed?
which command?
exit status?
stdout?
stderr?
what remains active?
```

Example:

```text
✗ build failed

Command:
bun run build

Exit:
1

stderr:
src/index.ts:18:4 ...

Production remains on release:
a81cd2
```

Failures are part of the UX.

---

# 30. Dry run

Support:

```bash
ciao deploy home --dry-run
```

Output:

```text
Detected runtime: Bun

Would:
1. upload 142 files
2. create release c194ff
3. run bun install --frozen-lockfile
4. run bun run build
5. start bun
6. allocate localhost port 41827
7. healthcheck /
8. activate release
9. route app.example.com
```

Important for both humans and agents.

---

# 31. MCP is a first-class interface

MCP should be part of the official architecture.

But the MCP server should run **locally**.

```text
coding agent
    │
    │ MCP
    ▼
ciao mcp
on developer machine
    │
    │ SSH
    ▼
remote server
```

This avoids exposing a Ciao management API on the homeserver.

---

# 32. MCP tools

Suggested initial tools:

```text
list_hosts
inspect_host

list_apps
inspect_app
get_status
get_logs

deploy_app
restart_app
start_app
stop_app

list_releases
rollback_app

add_domain
remove_domain

set_environment_variable
remove_environment_variable

expose_cloudflare
remove_cloudflare_exposure
```

The agent should operate in terms of Ciao concepts.

Do not give MCP arbitrary shell execution by default.

---

# 33. MCP security

Bad MCP design:

```text
run_shell(host, command)
```

Good design:

```text
deploy_app(host, project)
restart_app(host, app)
rollback_app(host, app)
get_logs(host, app)
```

High-level operations provide:

```text
validation
idempotency
auditability
smaller attack surface
```

If arbitrary SSH execution is ever added, it should require an explicit opt-in.

---

# 34. MCP permission profiles

Local configuration:

```toml
[mcp]
profile = "operator"
```

Suggested profiles:

```text
read-only
operator
admin
```

Read-only:

```text
status
logs
releases
host inspection
```

Operator:

```text
deploy
restart
start
stop
rollback
```

Admin:

```text
domains
environment
host initialization
exposure settings
```

Keep permissions understandable.

---

# 35. Example MCP workflow

User:

> Deploy the current project to home and verify that it is healthy.

Agent:

```text
list_hosts()
→ home

deploy_app(
  host = "home",
  project = current_project
)

get_status(
  host = "home",
  app = "myapp"
)
```

Result:

```text
release: c194ff
status: running
health: healthy
domain: app.example.com
rss: 22 MB
```

The agent never needs to know systemd syntax.

---

# 36. Shared core API

CLI, MCP and dashboard must share the same Rust core.

Correct:

```text
             ciao_core
            /      |      \
           /       |       \
        CLI       MCP       UI
```

Wrong:

```text
MCP
→ launches CLI
→ parses stdout
```

Do not duplicate deployment logic.

---

# 37. Machine-readable output

Every important CLI command should support:

```bash
--json
```

Example:

```bash
ciao status home myapp --json
```

```json
{
  "app": "myapp",
  "status": "running",
  "release": "c194ff",
  "port": 41827,
  "rss_bytes": 14680064
}
```

This helps:

```text
MCP
CI
scripts
third-party integrations
```

---

# 38. Dashboard

The dashboard is useful but must not create a mandatory remote control plane.

Command:

```bash
ciao ui home
```

starts a temporary local interface:

```text
http://127.0.0.1:7843
```

Architecture:

```text
browser
   ↓
local Ciao UI
   ↓
Ciao core
   ↓
SSH
   ↓
server
```

When `ciao ui` exits, the dashboard is gone.

No always-on Ciao web service is required remotely.

---

# 39. Dashboard scope

Overview:

```text
HOME SERVER

my-site      ● running    14 MB    app.example.com
api          ● running     9 MB    api.example.com
worker       ● running    21 MB    private

CPU        7%
RAM        1.3 / 8 GB
Disk       34%
```

App detail:

```text
status
current release
deployment history
logs
restart
stop/start
rollback
domain
environment
RSS
CPU
uptime
```

Do not build a full observability platform.

---

# 40. Metrics

Use OS data:

```text
systemd
/proc
cgroups
```

Expose:

```text
RSS
CPU
uptime
PID
restart count
port
```

Do not introduce a time-series database.

---

# 41. Audit events

MCP makes auditability useful.

Record simple structured events:

```text
2026-08-12 10:32 deploy myapp c194ff source=cli
2026-08-12 10:41 restart myapp source=mcp
2026-08-12 10:44 rollback myapp a81cd2 source=mcp
```

This can live in SQLite or structured logs.

No enterprise audit subsystem is needed.

---

# 42. Rust implementation

Ciao should be implemented primarily in Rust.

Reasons:

```text
single binary
fast startup
low local memory
good CLI ecosystem
strong typing
safe systems programming
easy distribution
```

Suggested workspace:

```text
crates/
  ciao_cli/
  ciao_core/
  ciao_config/
  ciao_host/
  ciao_detect/
  ciao_transport/
  ciao_deploy/
  ciao_release/
  ciao_systemd/
  ciao_proxy/
  ciao_cloudflare/
  ciao_mcp/
  ciao_ui/
```

Keep boundaries explicit.

---

# 43. Core Rust model

Conceptual model:

```rust
struct Host {
    name: String,
    ssh_target: String,
}

struct App {
    name: String,
    runtime: Runtime,
    host: HostId,
}

struct Release {
    id: ReleaseId,
    app: AppId,
    status: ReleaseStatus,
}

enum Runtime {
    Rust,
    Go,
    Bun,
    Node,
    Static,
    Custom,
}
```

Host platform should be explicit:

```rust
enum HostOs {
    Linux,
    MacOs,
}

enum HostArch {
    X86_64,
    Arm64,
}

struct HostPlatform {
    os: HostOs,
    arch: HostArch,
}
```

`MacOs + Arm64` represents Apple Silicon and must be tested as a primary path.

Deployment state machine:

```text
Created
Uploading
Installing
Building
Starting
Checking
Activating
Active
```

Failure:

```text
Failed
```

Persist enough events to diagnose failures.

---

# 44. Remote execution abstraction

All remote operations should go through one interface.

Conceptually:

```rust
trait RemoteHost {
    async fn exec(&self, command: Command) -> Result<Output>;
    async fn upload(&self, payload: Payload, target: RemotePath) -> Result<()>;
}
```

Likewise:

```rust
trait RuntimeDetector
trait ProcessManager
trait ReverseProxy
trait ExposureProvider
```

Do not scatter shell invocation throughout the codebase.

---

# 45. Command safety

Never concatenate raw user input into shell commands.

Bad:

```rust
format!("rm -rf {}", user_input)
```

Use:

```text
validated identifiers
fixed templates
escaped arguments
internally generated paths
```

Application identifiers should be constrained, for example:

```text
[a-zA-Z0-9-_]+
```

MCP operations must use the same validation.

---

# 46. Idempotency

Commands should be safe to repeat.

Example:

```bash
ciao host init home
```

second run:

```text
✓ directories already exist
✓ systemd integration healthy
✓ Caddy integration healthy
```

This is especially important for agents.

---

# 47. Local config

Use:

```text
~/.config/ciao/config.toml
```

Example:

```toml
[hosts.home]
ssh = "luca@192.168.1.50"
# Optional; created by the guided `ciao host add` bootstrap:
# identity_file = "/Users/me/.ssh/ciao/home_ed25519"

[hosts.vps]
ssh = "root@vps.example.com"
```

Do not maintain a private-key database. Ciao normally reuses the user's
OpenSSH config, agent and keys. When the user explicitly approves the guided
bootstrap, Ciao may create a standard user-owned Ed25519 identity locally;
its config stores only the path, the private key never crosses to the host,
and the password remains inside OpenSSH.

---

# 48. Remote config

Only create global remote configuration when actually needed.

Possible:

```text
/etc/ciao/config.toml
```

Example:

```toml
[ports]
start = 41000
end = 49000

[releases]
keep = 5
```

Normal usage should not require editing it.

---

# 49. Cleanup

Default release policy:

```text
keep last 5 successful releases
never delete current
retain enough metadata for failures
prune stale build artifacts
```

Cleanup must never break rollback unexpectedly.

---

# 50. Recoverability

Ciao-managed servers must remain understandable without Ciao.

An app is still:

```text
files
systemd service
environment file
Caddy route
```

If Ciao local state disappears, recover as much as possible from the server.

This is a major product principle:

> **Ciao manages Linux. It does not replace Linux.**

---

# 51. Uninstallability

Removing Ciao locally must not stop applications.

Deployed apps remain standard system services.

A future uninstall command may remove:

```text
Ciao-managed units
proxy fragments
release directories
```

but only explicitly.

No hidden dependency on the Ciao executable should exist remotely for ordinary app runtime.

---

# 52. CI use

Ciao should work naturally from CI:

```bash
ciao deploy production --non-interactive
```

Use normal SSH credentials injected by the CI provider.

No Ciao cloud account.

No special CI architecture.

---

# 53. Database migrations

Do not invent automatic migration semantics.

Optional:

```toml
[deploy]
migrate = "bun run migrate"
```

Ciao can run it at a defined phase.

But database rollback semantics belong to the application.

Do not pretend otherwise.

---

# 54. Workers

After web apps, support non-HTTP workers.

Example:

```toml
[app]
type = "worker"

[run]
command = "./worker"
```

Ciao still provides:

```text
deploy
systemd
logs
restart
rollback
```

No proxy.

---

# 55. Multiple processes

Do not support complex multi-process projects in the first version.

Start with:

```text
one deployable app
one primary process
```

Later:

```toml
[[process]]
name = "web"
command = "./app web"

[[process]]
name = "worker"
command = "./app worker"
```

Only after the single-process model is excellent.

---

# 56. Databases are out of scope

Do not deploy:

```text
Postgres
Redis
MySQL
Kafka
```

in v0.1.

Ciao deploys applications.

Infrastructure provisioning is a separate problem and a fast route toward becoming a platform.

---

# 57. Supported hosts

Start narrow, but support Linux and macOS deliberately.

v0.1 host targets:

```text
Linux
  Ubuntu
  Debian

macOS
  Apple Silicon (arm64) — first-class / required
  Intel (x86_64) — supported where practical
```

Architectures:

```text
x86_64
arm64
```

**Apple Silicon is a core compatibility requirement.**

The release/test matrix must include `aarch64-apple-darwin`.

Windows is explicitly out of scope for the initial versions.

---

# 58. Installation

Ciao uses one Rust codebase but distributes platform-specific binaries.

Initial release artifacts:

```text
ciao-linux-x86_64
ciao-linux-arm64
ciao-macos-x86_64
ciao-macos-arm64
```

Rust targets:

```text
x86_64-unknown-linux-gnu
aarch64-unknown-linux-gnu
x86_64-apple-darwin
aarch64-apple-darwin
```

An installer detects OS and architecture automatically, for example via:

```bash
uname -s
uname -m
```

and downloads the correct artifact.

For the user there is still only:

```bash
ciao
```

Apple Silicon (`aarch64-apple-darwin`) must be published and tested from the first public release.

Goal:

```text
one local binary
```

Distribution later:

```text
GitHub Releases
Homebrew
cargo-binstall
AUR
deb/rpm
```

Do not require Node, Python or Docker to run Ciao itself.

---

# 59. Server dependencies

Minimum depends on the host OS.

Linux:

```text
systemd
OpenSSH server
basic Unix tools
```

macOS:

```text
launchd
Remote Login / OpenSSH server
standard macOS Unix tools
```

Optional:

```text
Caddy
cloudflared
application runtime
```

The normal deploy path installs only the native dependencies it needs. Optional
integrations remain opt-in, and Ciao asks before changing external account
policy or other operator-owned configuration.

---

# 60. Testing

Unit tests:

```text
runtime detection
config parsing
release naming
manifest generation
command validation
deployment state transitions
```

Integration tests should exercise disposable Linux machines.

Test scenarios:

```text
first deploy
second deploy
failed build
failed healthcheck
rollback
server reboot
restart on failure
logs
domain
static site
Rust app
Go app
Bun app
Node app

Linux + systemd
macOS + launchd
Apple Silicon arm64
```

Containers are perfectly acceptable for Ciao's own CI tests.

The product simply does not require containers for user deployments.

---

# 61. Performance goals

Ciao itself should be almost invisible.

Measure:

```text
CLI startup
CLI RSS
deploy preparation overhead
dashboard idle RAM
MCP idle RAM
remote deployment overhead
```

The most important property:

> **When Ciao is not actively deploying, remote Ciao overhead should be effectively zero.**

No remote daemon makes this naturally achievable.

---

# 62. Security requirements

Defaults:

```text
no Ciao management port
no SSH private-key database or remote private-key copy
no application running as root
no public app port by default
no secret logging
no raw shell MCP tool
```

Use SSH as the trust boundary.

Use application-specific Unix users where possible.

---

# 63. Potential future remote agent

A remote agent may eventually enable:

```text
real-time metrics
push events
multi-user access
remote scheduling
```

But it must remain optional.

Do not require it for the core product.

---

# 64. Potential future localhost mode

The separate “localhost done well” idea can later become:

```bash
ciao dev
```

with:

```text
api.project.localhost
admin.project.localhost
```

But do not build this first.

Deployment is Ciao's initial identity.

---

# 65. Potential future preview environments

Later:

```bash
ciao deploy home --preview
```

could produce:

```text
pr-142.myapp.example.com
```

with expiration.

Interesting, but not core MVP.

---

# 66. CLI surface

Recommended:

```bash
ciao host add
ciao host list
ciao host inspect

ciao inspect
ciao deploy
ciao apps
ciao status
ciao logs
ciao restart
ciao start
ciao stop
ciao rollback

ciao domain add
ciao domain remove

ciao expose

ciao env set
ciao env unset

ciao mcp
ciao ui
```

Avoid command explosion.

---

# 67. MVP

The first public MVP should make this work reliably:

```bash
ciao host add home user@server

cd my-project

ciao deploy home
```

Supported:

```text
Rust HTTP server
Go HTTP server
Bun web server
Node web server
static site
```

Core features:

```text
SSH
project detection
source upload
remote build
immutable releases
systemd
journald
restart-on-failure
start-on-boot
managed localhost port
healthcheck
rollback
Caddy domain
```

MCP should be part of the first serious release immediately after the deploy core is stable.

---

# 68. Implementation order

## Phase 1 — Host transport

Build:

```text
CLI
local config
SSH abstraction
remote execution abstraction
host inspection
```

Goal:

```bash
ciao host add
ciao host inspect
```

---

## Phase 2 — Project detection

Build deterministic detection for:

```text
Rust
Go
Bun
Node
static
```

Goal:

```bash
ciao inspect
```

returns a deployment plan.

---

## Phase 3 — Release upload

Build:

```text
ignore rules
tar streaming
release directories
release manifests
```

Goal:

```text
local project
→ immutable remote release
```

---

## Phase 4 — Build and process lifecycle

Build:

```text
remote install
remote build
port allocation
ServiceManager abstraction
systemd backend
launchd backend
service lifecycle
journald/macOS log backend
```

Goal:

> App survives SSH disconnect and server reboot.

---

## Phase 5 — Safe activation

Build:

```text
candidate release
healthcheck
activation
rollback
release pruning
```

Goal:

> A broken deployment never replaces working production.

---

## Phase 6 — Exposure

Build:

```text
Caddy integration
domains
HTTPS
```

Then:

```text
Cloudflare Tunnel adapter
```

---

## Phase 7 — MCP

Build:

```text
ciao mcp
shared core API
structured tools
permission profiles
audit events
```

Goal:

> An agent can safely deploy, inspect logs, restart and roll back.

---

## Phase 8 — Dashboard

Build:

```text
ciao ui
small local Rust HTTP server
embedded frontend
SSH-backed state
```

Only after CLI and MCP are solid.

---

# 69. Repository structure

Suggested monorepo:

```text
ciao/
  Cargo.toml

  crates/
    ciao_cli/
    ciao_core/
    ciao_config/
    ciao_host/
    ciao_detect/
    ciao_transport/
    ciao_deploy/
    ciao_release/
    ciao_systemd/
    ciao_proxy/
    ciao_cloudflare/
    ciao_mcp/
    ciao_ui/

  examples/
    rust-app/
    go-app/
    bun-app/
    node-app/
    static-site/

  tests/
    integration/

  docs/
    architecture.md
    security.md
    mcp.md
```

Keep one repository initially.

---

# 70. Priorities for an implementation agent

Use this order:

```text
1. safety
2. correctness
3. reliability
4. simple architecture
5. developer experience
6. performance
7. feature breadth
```

Ciao touches real servers.

A clever deployment system that occasionally destroys state is useless.

Work in vertical slices:

```text
implement
test
exercise on real Linux
document
continue
```

Avoid speculative subsystems.

---

# 71. Anti-derailment checklist

Before adding a feature:

### Does it directly make application deployment easier?

If not, probably skip it.

### Can Linux already solve this?

Integrate rather than replace.

### Does it require a permanent Ciao daemon?

Require strong justification.

### Does it move Ciao toward container orchestration?

Stop and reconsider.

### Can it be an optional adapter?

Prefer an adapter.

### Does the happy path remain one or two commands?

If not, simplify.

### Is it useful without the dashboard?

It should be.

### Is it useful without MCP?

It should be.

The CLI deployment core must stand on its own.

---

# 72. Explicit non-goals

Do not initially build:

```text
container runtime
container registry
Kubernetes integration
custom reverse proxy
custom init system
custom logging database
custom TLS implementation
custom SSH crypto implementation
database provisioning
distributed scheduler
hosted control plane
team account system
billing
CI platform
Git hosting
full observability stack
```

The project should stay small enough that a developer can understand the architecture.

---

# 73. Example README opening

```text
Ciao
========

Ship apps. Skip the ops.

Ciao deploys applications to your own Linux servers without Docker,
Kubernetes, or a permanent control plane.

$ ciao host add home user@192.168.1.50
$ ciao deploy home --domain app.example.com

That's it.

Ciao uploads your app, builds it, runs it under systemd, keeps it alive,
stores logs in journald, configures HTTPS through Caddy, performs health
checks, and keeps previous releases ready for rollback.

Your server remains a normal Linux server.
```

---

# 74. Product positioning

Do not pitch Ciao as:

```text
Docker replacement
Kubernetes replacement
another PaaS
DevOps platform
```

Better:

> **Ciao is the fastest way to turn a Linux server into a place where your apps just run.**

Alternative:

> **Deploy to your own server without becoming your own DevOps team.**

Main claim:

> **Ship apps. Skip the ops.**

---

# 75. Final thesis

Ciao should make a plain Linux server or Mac feel modern without hiding it behind another platform.

The infrastructure is intentionally boring:

```text
SSH
systemd / launchd
journald / native macOS logging
Caddy
filesystem
```

The product value is making these pieces feel like one tool.

Human workflow:

```bash
ciao host add home user@server
ciao deploy home --domain app.example.com
```

Agent workflow:

```text
coding agent
    ↓ MCP
Ciao local
    ↓ SSH
homeserver
```

When Ciao is idle:

```text
no remote daemon
no control plane
no container runtime
no platform tax
```

Just normal applications running as normal Linux services.

> **Ship apps. Skip the ops.**
