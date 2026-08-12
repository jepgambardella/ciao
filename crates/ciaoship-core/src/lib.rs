//! Shared CiaoShip domain, detection and deployment primitives.
//!
//! The CLI and MCP layers deliberately depend on this crate instead of
//! invoking one another. Remote work is performed through OpenSSH, while the
//! target machine remains a normal systemd/launchd host.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt::{Display, Formatter};
use std::fs;
use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use thiserror::Error;

pub const APP_ROOT: &str = "/var/lib/ciaoship/apps";
pub const CONFIG_ENV: &str = "CIAOSHIP_CONFIG";
pub const PORT_START: u16 = 41_000;
pub const PORT_END: u16 = 49_000;
pub const LOCAL_PORT_START: u16 = 41_000;
pub const LOCAL_PORT_END: u16 = 49_999;

#[derive(Debug, Error)]
pub enum CiaoError {
    #[error("invalid {field} `{value}`: {reason}")]
    InvalidIdentifier {
        field: &'static str,
        value: String,
        reason: &'static str,
    },
    #[error("configuration error: {0}")]
    Config(String),
    #[error("project detection failed: {0}")]
    Detection(String),
    #[error("transport failed during {stage}: {message}{details}")]
    Transport {
        stage: String,
        message: String,
        details: String,
    },
    #[error(
        "remote command failed during {stage}: exit {exit}\nstdout:\n{stdout}\nstderr:\n{stderr}"
    )]
    RemoteCommand {
        stage: String,
        exit: i32,
        stdout: String,
        stderr: String,
    },
    #[error(
        "local command failed during {stage}: exit {exit}\nstdout:\n{stdout}\nstderr:\n{stderr}"
    )]
    LocalCommand {
        stage: String,
        exit: i32,
        stdout: String,
        stderr: String,
    },
    #[error("deployment failed during {stage}: {message}; previous release: {previous_release}")]
    Deployment {
        stage: String,
        message: String,
        previous_release: String,
    },
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("serialization error: {0}")]
    Serialization(String),
}

pub type Result<T> = std::result::Result<T, CiaoError>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Host {
    #[serde(skip)]
    pub name: String,
    pub ssh: String,
}

impl Host {
    pub fn new(name: impl Into<String>, ssh: impl Into<String>) -> Result<Self> {
        let name = name.into();
        validate_identifier("host name", &name)?;
        let ssh = ssh.into();
        validate_ssh_target(&ssh)?;
        Ok(Self { name, ssh })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum HostOs {
    Linux,
    MacOs,
    Unknown(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum HostArch {
    X86_64,
    Arm64,
    Unknown(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostPlatform {
    pub os: HostOs,
    pub arch: HostArch,
    pub service_manager: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostInitResult {
    pub platform: HostPlatform,
    pub dependencies: Vec<String>,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Runtime {
    Rust,
    Go,
    Bun,
    Node,
    Static,
}

impl Display for Runtime {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Rust => "Rust",
            Self::Go => "Go",
            Self::Bun => "Bun",
            Self::Node => "Node",
            Self::Static => "Static",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AppType {
    Service,
    Static,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthConfig {
    pub path: String,
    pub expected_status: u16,
    pub timeout_seconds: u64,
}

impl Default for HealthConfig {
    fn default() -> Self {
        Self {
            path: "/".to_owned(),
            expected_status: 200,
            timeout_seconds: 10,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectPlan {
    pub name: String,
    pub runtime: Runtime,
    pub app_type: AppType,
    pub install_command: Option<String>,
    pub build_command: Option<String>,
    pub run_command: Option<String>,
    pub port: Option<u16>,
    pub health: HealthConfig,
    pub static_directory: Option<String>,
    pub port_explicit: bool,
    pub local_name: Option<String>,
    pub local_port: Option<u16>,
    pub local_command: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct ProjectConfig {
    app: Option<AppConfig>,
    build: Option<BuildConfig>,
    run: Option<RunConfig>,
    health: Option<HealthConfigFile>,
    dev: Option<DevConfig>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct AppConfig {
    name: Option<String>,
    #[serde(rename = "type")]
    app_type: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct BuildConfig {
    install: Option<String>,
    command: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct RunConfig {
    command: Option<String>,
    port: Option<u16>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct HealthConfigFile {
    path: Option<String>,
    expected_status: Option<u16>,
    timeout: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct DevConfig {
    name: Option<String>,
    port: Option<u16>,
    command: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub hosts: BTreeMap<String, Host>,
    #[serde(default)]
    pub mcp: McpConfig,
    #[serde(default)]
    pub local: LocalConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpConfig {
    pub profile: String,
}

impl Default for McpConfig {
    fn default() -> Self {
        Self {
            profile: "read-only".to_owned(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalConfig {
    #[serde(default = "default_local_port_start")]
    pub port_start: u16,
    #[serde(default = "default_local_port_end")]
    pub port_end: u16,
    #[serde(default)]
    pub projects: BTreeMap<String, LocalProject>,
}

impl Default for LocalConfig {
    fn default() -> Self {
        Self {
            port_start: LOCAL_PORT_START,
            port_end: LOCAL_PORT_END,
            projects: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalProject {
    pub domain: String,
    pub port: u16,
    pub source: String,
    #[serde(default)]
    pub app_type: Option<AppType>,
    #[serde(default)]
    pub static_root: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalDevPlan {
    pub name: String,
    pub domain: String,
    pub port: u16,
    pub source: PathBuf,
    pub runtime: Runtime,
    pub app_type: AppType,
    pub install_command: Option<String>,
    pub build_command: Option<String>,
    pub run_command: Option<String>,
    pub static_root: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalSetupResult {
    pub resolver: String,
    pub proxy: String,
    pub dependencies: Vec<String>,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalProxyPaths {
    pub caddy_bin: PathBuf,
    pub caddyfile: PathBuf,
    pub fragment_dir: PathBuf,
}

fn default_local_port_start() -> u16 {
    LOCAL_PORT_START
}

fn default_local_port_end() -> u16 {
    LOCAL_PORT_END
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let contents = fs::read_to_string(path)?;
        let mut config: Self =
            toml::from_str(&contents).map_err(|error| CiaoError::Config(error.to_string()))?;
        for (name, host) in &mut config.hosts {
            validate_identifier("host name", name)?;
            host.name = name.clone();
            validate_ssh_target(&host.ssh)?;
        }
        validate_profile(&config.mcp.profile)?;
        if config.local.port_start < 1024 || config.local.port_end < config.local.port_start {
            return Err(CiaoError::Config(
                "local port range must be ordered and start at 1024 or above".to_owned(),
            ));
        }
        for (name, project) in &config.local.projects {
            validate_local_name(name)?;
            if project.domain != local_domain(name)? {
                return Err(CiaoError::Config(format!(
                    "local project {} has domain {}, expected {}.ciao",
                    name, project.domain, name
                )));
            }
            if !(config.local.port_start..=config.local.port_end).contains(&project.port) {
                return Err(CiaoError::Config(format!(
                    "local project {} has port {} outside the configured range",
                    name, project.port
                )));
            }
            if project.source.trim().is_empty() {
                return Err(CiaoError::Config(format!(
                    "local project {} has an empty source path",
                    name
                )));
            }
        }
        Ok(config)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let contents = toml::to_string_pretty(self)
            .map_err(|error| CiaoError::Serialization(error.to_string()))?;
        fs::write(path, contents)?;
        Ok(())
    }

    pub fn add_host(&mut self, host: Host) {
        self.hosts.insert(host.name.clone(), host);
    }
}

pub fn default_config_path() -> PathBuf {
    if let Ok(path) = std::env::var(CONFIG_ENV) {
        return PathBuf::from(path);
    }
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|path| path.join(".config/ciaoship/config.toml"))
        .unwrap_or_else(|| PathBuf::from(".ciaoship/config.toml"))
}

pub fn validate_identifier(field: &'static str, value: &str) -> Result<()> {
    if value.is_empty() || value.len() > 64 {
        return Err(CiaoError::InvalidIdentifier {
            field,
            value: value.to_owned(),
            reason: "must be 1-64 characters",
        });
    }
    if !value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
        || value.starts_with('-')
    {
        return Err(CiaoError::InvalidIdentifier {
            field,
            value: value.to_owned(),
            reason: "only letters, numbers, `_` and `-` are allowed and it cannot start with `-`",
        });
    }
    Ok(())
}

pub fn validate_env_key(key: &str) -> Result<()> {
    let valid = !key.is_empty()
        && key
            .as_bytes()
            .first()
            .is_some_and(|byte| byte.is_ascii_alphabetic() || *byte == b'_')
        && key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_');
    if !valid {
        return Err(CiaoError::InvalidIdentifier {
            field: "environment variable",
            value: key.to_owned(),
            reason: "must match [A-Za-z_][A-Za-z0-9_]*",
        });
    }
    Ok(())
}

pub fn validate_ssh_target(value: &str) -> Result<()> {
    if value.is_empty()
        || value.starts_with('-')
        || value
            .chars()
            .any(|char| char.is_whitespace() || ";|&$`()<>\n\r~*?![]\\%\"'".contains(char))
        || value.matches('@').count() > 1
        || value.contains('/')
        || value.contains(':')
    {
        return Err(CiaoError::InvalidIdentifier {
            field: "SSH target",
            value: value.to_owned(),
            reason: "must be a single OpenSSH destination without shell metacharacters or options",
        });
    }
    let host = value.rsplit('@').next().unwrap_or(value);
    if host.is_empty() || host == "." || host == ".." {
        return Err(CiaoError::InvalidIdentifier {
            field: "SSH target",
            value: value.to_owned(),
            reason: "host portion is empty or invalid",
        });
    }
    let (user, hostname) = value.rsplit_once('@').unwrap_or(("", value));
    for (field, part) in [("SSH user", user), ("SSH host", hostname)] {
        if !part.is_empty()
            && !part
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
        {
            return Err(CiaoError::InvalidIdentifier {
                field,
                value: part.to_owned(),
                reason: "only letters, numbers, `.`, `_` and `-` are allowed",
            });
        }
    }
    Ok(())
}

pub fn validate_profile(profile: &str) -> Result<()> {
    if matches!(profile, "read-only" | "operator" | "admin") {
        Ok(())
    } else {
        Err(CiaoError::Config(format!(
            "unknown MCP profile `{profile}`; expected read-only, operator or admin"
        )))
    }
}

pub fn detect_project(root: &Path) -> Result<ProjectPlan> {
    let config_path = root.join("ciaoship.toml");
    let config: ProjectConfig = if config_path.exists() {
        toml::from_str(&fs::read_to_string(&config_path)?)
            .map_err(|error| CiaoError::Config(error.to_string()))?
    } else {
        ProjectConfig::default()
    };
    let name = config
        .app
        .as_ref()
        .and_then(|app| app.name.clone())
        .or_else(|| {
            root.file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .ok_or_else(|| CiaoError::Detection("project has no usable name".to_owned()))?;
    validate_identifier("app name", &name)?;

    let (runtime, app_type, install, build, run, port, static_directory) = if root
        .join("Cargo.toml")
        .exists()
    {
        let binary = cargo_package_name(root).unwrap_or_else(|| name.clone());
        (
            Runtime::Rust,
            AppType::Service,
            None,
            Some("cargo build --release".to_owned()),
            config
                .run
                .as_ref()
                .and_then(|run| run.command.clone())
                .or_else(|| Some(format!("./target/release/{binary}"))),
            config.run.as_ref().and_then(|run| run.port).or(Some(3000)),
            None,
        )
    } else if root.join("go.mod").exists() {
        (
            Runtime::Go,
            AppType::Service,
            None,
            Some("go build -o app .".to_owned()),
            config
                .run
                .as_ref()
                .and_then(|run| run.command.clone())
                .or_else(|| Some("./app".to_owned())),
            config.run.as_ref().and_then(|run| run.port).or(Some(3000)),
            None,
        )
    } else if root.join("bun.lock").exists() || root.join("bun.lockb").exists() {
        (
            Runtime::Bun,
            AppType::Service,
            Some("bun install --frozen-lockfile".to_owned()),
            Some("bun run build".to_owned()),
            Some("bun start".to_owned()),
            Some(3000),
            None,
        )
    } else if root.join("package.json").exists() {
        let (install, runner) = if root.join("pnpm-lock.yaml").exists() {
            ("pnpm install --frozen-lockfile", "pnpm")
        } else if root.join("yarn.lock").exists() {
            ("yarn install --frozen-lockfile", "yarn")
        } else if root.join("package-lock.json").exists() {
            ("npm ci", "npm")
        } else {
            ("npm install", "npm")
        };
        (
            Runtime::Node,
            AppType::Service,
            Some(install.to_owned()),
            Some(format!("{runner} run build")),
            Some(format!("{runner} start")),
            Some(3000),
            None,
        )
    } else if let Some(directory) = ["dist", "build", "public"]
        .into_iter()
        .find(|directory| root.join(directory).is_dir())
    {
        (
            Runtime::Static,
            AppType::Static,
            None,
            None,
            None,
            None,
            Some(directory.to_owned()),
        )
    } else {
        return Err(CiaoError::Detection(
                "no supported project marker found (Cargo.toml, go.mod, Bun/Node lockfile or dist/build/public)".to_owned(),
            ));
    };

    let port_explicit = config.run.as_ref().and_then(|run| run.port).is_some();
    let mut plan = ProjectPlan {
        name,
        runtime,
        app_type,
        install_command: install,
        build_command: build,
        run_command: run,
        port,
        health: HealthConfig::default(),
        static_directory,
        port_explicit,
        local_name: None,
        local_port: None,
        local_command: None,
    };
    if let Some(app) = config.app.as_ref().and_then(|app| app.app_type.as_deref()) {
        plan.app_type = match app {
            "service" => AppType::Service,
            "static" => AppType::Static,
            other => {
                return Err(CiaoError::Config(format!(
                    "unsupported app.type `{other}`; use service or static"
                )))
            }
        };
        if plan.app_type == AppType::Static {
            plan.install_command = None;
            plan.build_command = None;
            plan.run_command = None;
            plan.port = None;
            plan.static_directory = ["dist", "build", "public"]
                .into_iter()
                .find(|directory| root.join(directory).is_dir())
                .map(str::to_owned);
            if plan.static_directory.is_none() {
                return Err(CiaoError::Detection(
                    "static app.type requires dist, build or public".to_owned(),
                ));
            }
        }
    }
    if let Some(build) = config.build {
        plan.install_command = build.install.or(plan.install_command);
        plan.build_command = build.command.or(plan.build_command);
    }
    if let Some(run) = config.run {
        plan.run_command = run.command.or(plan.run_command);
        plan.port = run.port.or(plan.port);
    }
    if let Some(health) = config.health {
        plan.health.path = health.path.unwrap_or(plan.health.path);
        plan.health.expected_status = health
            .expected_status
            .unwrap_or(plan.health.expected_status);
        if let Some(timeout) = health.timeout {
            plan.health.timeout_seconds = parse_duration_seconds(&timeout)?;
        }
    }
    let mut local_name = None;
    let mut local_port = None;
    let mut local_command = None;
    if let Some(dev) = config.dev {
        if let Some(name) = dev.name {
            validate_local_name(&name)?;
            local_name = Some(name);
        }
        if let Some(command) = dev.command {
            if command.trim().is_empty() {
                return Err(CiaoError::Config("dev.command cannot be empty".to_owned()));
            }
            local_command = Some(command);
        }
        if let Some(port) = dev.port {
            local_port = Some(port);
            plan.port_explicit = true;
        }
    }
    plan.local_name = local_name;
    plan.local_port = local_port;
    plan.local_command = local_command;
    if plan.app_type == AppType::Static {
        plan.install_command = None;
        plan.build_command = None;
        plan.run_command = None;
        plan.port = None;
    }
    if !plan.health.path.starts_with('/') || plan.health.path.contains(['\n', '\r', ' ']) {
        return Err(CiaoError::Config(
            "health.path must be an absolute URL path without whitespace".to_owned(),
        ));
    }
    if plan.health.path.contains(['#', '?']) || plan.health.path.contains("..") {
        return Err(CiaoError::Config(
            "health.path must not contain query, fragment or parent-path segments".to_owned(),
        ));
    }
    Ok(plan)
}

pub fn local_domain(name: &str) -> Result<String> {
    validate_local_name(name)?;
    Ok(format!("{name}.ciao"))
}

pub fn validate_local_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 63
        || name.starts_with('-')
        || name.ends_with('-')
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return Err(CiaoError::InvalidIdentifier {
            field: "local project name",
            value: name.to_owned(),
            reason: "must be a DNS-safe label",
        });
    }
    Ok(())
}

pub fn local_dev_plan(
    source: &Path,
    detected: &ProjectPlan,
    override_name: Option<&str>,
    local: &LocalConfig,
) -> Result<LocalDevPlan> {
    let source = source
        .canonicalize()
        .map_err(|error| CiaoError::Detection(format!("cannot resolve project path: {error}")))?;
    if !source.is_dir() {
        return Err(CiaoError::Detection(
            "local dev project path must be a directory".to_owned(),
        ));
    }
    let name = override_name
        .or(detected.local_name.as_deref())
        .unwrap_or(&detected.name)
        .to_owned();
    validate_local_name(&name)?;
    let domain = local_domain(&name)?;
    let persisted = local.projects.get(&name);
    let source_string = source.display().to_string();
    if let Some(project) = persisted {
        if project.source != source_string {
            return Err(CiaoError::Config(format!(
                "local name {} is already mapped to {}; use --name to choose another name",
                name, project.source
            )));
        }
    }
    let preferred = detected
        .local_port
        .or_else(|| detected.port_explicit.then_some(detected.port).flatten());
    let port_is_reserved = |port: &u16| {
        local
            .projects
            .iter()
            .any(|(project_name, project)| project_name != &name && project.port == *port)
    };
    let port = persisted
        .filter(|project| {
            project.source == source_string
                && (local.port_start..=local.port_end).contains(&project.port)
                && !port_is_reserved(&project.port)
                && TcpStream::connect(("127.0.0.1", project.port)).is_err()
        })
        .map(|project| project.port)
        .or_else(|| {
            preferred.filter(|port| {
                (local.port_start..=local.port_end).contains(port)
                    && !port_is_reserved(port)
                    && TcpStream::connect(("127.0.0.1", *port)).is_err()
            })
        })
        .or_else(|| {
            (local.port_start..=local.port_end).find(|port| {
                TcpStream::connect(("127.0.0.1", *port)).is_err() && !port_is_reserved(port)
            })
        })
        .ok_or_else(|| CiaoError::Config("no free local development port remains".to_owned()))?;
    let static_root = detected
        .static_directory
        .as_ref()
        .map(|directory| source.join(directory));
    let run_command = detected
        .local_command
        .clone()
        .or_else(|| detected.run_command.clone());
    Ok(LocalDevPlan {
        name,
        domain,
        port,
        source,
        runtime: detected.runtime.clone(),
        app_type: detected.app_type.clone(),
        install_command: detected.install_command.clone(),
        build_command: detected.build_command.clone(),
        run_command,
        static_root,
    })
}

fn cargo_package_name(root: &Path) -> Option<String> {
    let contents = fs::read_to_string(root.join("Cargo.toml")).ok()?;
    let parsed: toml::Value = toml::from_str(&contents).ok()?;
    parsed
        .get("package")
        .and_then(|package| package.get("name"))
        .and_then(toml::Value::as_str)
        .map(|name| name.replace('-', "_"))
}

fn parse_duration_seconds(value: &str) -> Result<u64> {
    let trimmed = value.trim();
    let number = trimmed
        .strip_suffix('s')
        .or_else(|| trimmed.strip_suffix("sec"))
        .unwrap_or(trimmed)
        .parse::<u64>()
        .map_err(|_| CiaoError::Config(format!("invalid duration `{value}`")))?;
    if number == 0 || number > 3600 {
        return Err(CiaoError::Config(
            "health timeout must be 1-3600 seconds".to_owned(),
        ));
    }
    Ok(number)
}

pub fn release_id() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let seconds = now.as_secs();
    let days = seconds / 86_400;
    let (year, month, day) = civil_from_days(days as i64);
    let hour = (seconds % 86_400) / 3_600;
    let minute = (seconds % 3_600) / 60;
    let second = seconds % 60;
    let nanos = now.subsec_nanos();
    format!("{year:04}{month:02}{day:02}-{hour:02}{minute:02}{second:02}-{nanos:09}")
}

// Howard Hinnant's public-domain civil date conversion, kept local to avoid a
// date dependency for release directory names.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = mp + if mp < 10 { 3 } else { -9 };
    (y + if m <= 2 { 1 } else { 0 }, m, d)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReleaseManifest {
    pub release: String,
    pub app: String,
    pub runtime: Runtime,
    pub app_type: AppType,
    pub port: Option<u16>,
    pub source_path: String,
    pub install_command: Option<String>,
    pub build_command: Option<String>,
    pub run_command: Option<String>,
    pub static_directory: Option<String>,
    pub health: HealthConfig,
    pub created_at_unix: u64,
}

fn release_start_command(plan: &ProjectPlan) -> Result<&str> {
    plan.run_command
        .as_deref()
        .ok_or_else(|| CiaoError::Config("service deployment requires a run command".to_owned()))
}

impl ReleaseManifest {
    pub fn from_plan(release: String, source_path: &Path, plan: &ProjectPlan) -> Self {
        Self {
            release,
            app: plan.name.clone(),
            runtime: plan.runtime.clone(),
            app_type: plan.app_type.clone(),
            port: plan.port,
            source_path: source_path.display().to_string(),
            install_command: plan.install_command.clone(),
            build_command: plan.build_command.clone(),
            run_command: plan.run_command.clone(),
            static_directory: plan.static_directory.clone(),
            health: plan.health.clone(),
            created_at_unix: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        }
    }

    pub fn to_toml(&self) -> Result<String> {
        toml::to_string_pretty(self).map_err(|error| CiaoError::Serialization(error.to_string()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    pub program: String,
    pub args: Vec<String>,
    pub stdin: Option<Vec<u8>>,
    pub stage: String,
}

impl CommandSpec {
    pub fn fixed(program: impl Into<String>, args: &[&str], stage: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            args: args.iter().map(|arg| (*arg).to_owned()).collect(),
            stdin: None,
            stage: stage.into(),
        }
    }

    pub fn with_stdin(mut self, stdin: impl Into<Vec<u8>>) -> Self {
        self.stdin = Some(stdin.into());
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandOutput {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

impl CommandOutput {
    fn from_output(output: std::process::Output) -> Self {
        Self {
            status: output.status.code().unwrap_or(128),
            stdout: truncate(&String::from_utf8_lossy(&output.stdout)),
            stderr: truncate(&String::from_utf8_lossy(&output.stderr)),
        }
    }

    pub fn ensure_success(self, stage: &str) -> Result<Self> {
        if self.status == 0 {
            Ok(self)
        } else {
            Err(CiaoError::RemoteCommand {
                stage: stage.to_owned(),
                exit: self.status,
                stdout: self.stdout,
                stderr: self.stderr,
            })
        }
    }
}

pub trait RemoteHost {
    fn exec(&self, command: CommandSpec) -> Result<CommandOutput>;
    fn inspect(&self) -> Result<HostPlatform>;
}

#[derive(Debug, Clone)]
pub struct OpenSshTransport {
    pub target: String,
    pub connect_timeout_seconds: u64,
}

impl OpenSshTransport {
    pub fn new(target: impl Into<String>) -> Result<Self> {
        let target = target.into();
        validate_ssh_target(&target)?;
        Ok(Self {
            target,
            connect_timeout_seconds: 10,
        })
    }

    fn ssh_command(&self, command: &CommandSpec) -> Command {
        let mut process = Command::new("ssh");
        process
            .arg("-o")
            .arg("BatchMode=yes")
            .arg("-o")
            .arg(format!("ConnectTimeout={}", self.connect_timeout_seconds))
            .arg("-o")
            .arg("ServerAliveInterval=15")
            .arg("-o")
            .arg("ServerAliveCountMax=2")
            .arg(&self.target)
            .arg(&command.program);
        for arg in &command.args {
            process.arg(arg);
        }
        process
    }

    /// Upload a filtered source snapshot to a new, caller-generated directory.
    /// The local tar and remote extractor are separate processes; no shell
    /// pipeline is constructed from user input.
    pub fn upload_tar(&self, source: &Path, destination: &str) -> Result<()> {
        if !source.is_dir() {
            return Err(CiaoError::Config(format!(
                "project path `{}` is not a directory",
                source.display()
            )));
        }
        validate_upload_destination(destination)?;
        let mut tar_command = Command::new("tar");
        tar_command
            .arg("--create")
            .arg("--file=-")
            .arg("--exclude=.git")
            .arg("--exclude=.ciaoship")
            .arg("--exclude=target")
            .arg("--exclude=node_modules")
            .arg("--exclude=.env")
            .arg("--exclude=.env.*")
            .args(
                ignore_patterns(source)
                    .iter()
                    .map(|pattern| format!("--exclude={pattern}")),
            )
            .arg("--directory")
            .arg(source)
            .arg(".");
        let mut tar = tar_command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| CiaoError::Transport {
                stage: "upload".to_owned(),
                message: error.to_string(),
                details: String::new(),
            })?;

        let mut remote = Command::new("ssh")
            .arg("-o")
            .arg("BatchMode=yes")
            .arg("-o")
            .arg(format!("ConnectTimeout={}", self.connect_timeout_seconds))
            .arg("-o")
            .arg("ServerAliveInterval=15")
            .arg("-o")
            .arg("ServerAliveCountMax=2")
            .arg(&self.target)
            .arg("sudo")
            .arg("-n")
            .arg("tar")
            .arg("--extract")
            .arg("--file=-")
            .arg("--no-same-owner")
            .arg("--no-same-permissions")
            .arg("--no-absolute-names")
            .arg("--directory")
            .arg(destination)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| CiaoError::Transport {
                stage: "upload".to_owned(),
                message: error.to_string(),
                details: String::new(),
            })?;
        let mut tar_stdout = tar.stdout.take().ok_or_else(|| CiaoError::Transport {
            stage: "upload".to_owned(),
            message: "tar stdout was not available".to_owned(),
            details: String::new(),
        })?;
        let mut remote_stdin = remote.stdin.take().ok_or_else(|| CiaoError::Transport {
            stage: "upload".to_owned(),
            message: "SSH stdin was not available".to_owned(),
            details: String::new(),
        })?;
        let copy_result = io::copy(&mut tar_stdout, &mut remote_stdin);
        drop(remote_stdin);
        let tar_output = tar.wait_with_output()?;
        let remote_output = remote.wait_with_output()?;
        if !remote_output.status.success() {
            return Err(CiaoError::RemoteCommand {
                stage: "remote source extraction".to_owned(),
                exit: exit_code(remote_output.status),
                stdout: truncate(&String::from_utf8_lossy(&remote_output.stdout)),
                stderr: truncate(&String::from_utf8_lossy(&remote_output.stderr)),
            });
        }
        copy_result?;
        if !tar_output.status.success() {
            return Err(CiaoError::RemoteCommand {
                stage: "local source archive".to_owned(),
                exit: exit_code(tar_output.status),
                stdout: String::new(),
                stderr: truncate(&String::from_utf8_lossy(&tar_output.stderr)),
            });
        }
        Ok(())
    }
}

fn ignore_patterns(source: &Path) -> Vec<String> {
    [".gitignore", ".ciaoshipignore"]
        .into_iter()
        .flat_map(|name| fs::read_to_string(source.join(name)).ok())
        .flat_map(|contents| {
            contents
                .lines()
                .map(str::trim)
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .filter(|line| !line.is_empty() && !line.starts_with('#') && !line.starts_with('!'))
        .map(|line| line.trim_start_matches('/').to_owned())
        .collect()
}

impl RemoteHost for OpenSshTransport {
    fn exec(&self, command: CommandSpec) -> Result<CommandOutput> {
        let mut process = self.ssh_command(&command);
        process.stdin(if command.stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        });
        let mut child = process.spawn().map_err(|error| CiaoError::Transport {
            stage: command.stage.clone(),
            message: error.to_string(),
            details: String::new(),
        })?;
        if let Some(stdin) = command.stdin {
            child
                .stdin
                .take()
                .ok_or_else(|| CiaoError::Transport {
                    stage: command.stage.clone(),
                    message: "SSH stdin was not available".to_owned(),
                    details: String::new(),
                })?
                .write_all(&stdin)?;
        }
        let output = child.wait_with_output()?;
        let result = CommandOutput::from_output(output);
        result.ensure_success(&command.stage)
    }

    fn inspect(&self) -> Result<HostPlatform> {
        let command = CommandSpec::fixed("sh", &["-s"], "host inspection").with_stdin(
            b"set -eu\nprintf 'os=%s\\n' \"$(uname -s)\"\nprintf 'arch=%s\\n' \"$(uname -m)\"\nif command -v systemctl >/dev/null 2>&1; then printf 'service_manager=systemd\\n'; elif command -v launchctl >/dev/null 2>&1; then printf 'service_manager=launchd\\n'; else printf 'service_manager=unknown\\n'; fi\n".to_vec(),
        );
        let output = self.exec(command)?;
        let mut values = BTreeMap::new();
        for line in output.stdout.lines() {
            if let Some((key, value)) = line.split_once('=') {
                values.insert(key, value);
            }
        }
        let os = match values.get("os").copied().unwrap_or_default() {
            "Linux" => HostOs::Linux,
            "Darwin" => HostOs::MacOs,
            other => HostOs::Unknown(other.to_owned()),
        };
        let arch = match values.get("arch").copied().unwrap_or_default() {
            "x86_64" | "amd64" => HostArch::X86_64,
            "arm64" | "aarch64" => HostArch::Arm64,
            other => HostArch::Unknown(other.to_owned()),
        };
        Ok(HostPlatform {
            os,
            arch,
            service_manager: values
                .get("service_manager")
                .copied()
                .unwrap_or("unknown")
                .to_owned(),
        })
    }
}

pub fn command_script(command: &str, cwd: &str) -> Result<Vec<u8>> {
    if command.trim().is_empty() {
        return Err(CiaoError::Config(
            "application command cannot be empty".to_owned(),
        ));
    }
    if cwd.is_empty() || cwd.contains(['\n', '\r']) || !cwd.starts_with('/') {
        return Err(CiaoError::Config(
            "internal working directory is invalid".to_owned(),
        ));
    }
    Ok(format!("set -eu\ncd -- {}\nexec {}\n", shell_quote(cwd), command).into_bytes())
}

pub fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

pub fn run_local_script(script: &[u8]) -> Result<CommandOutput> {
    let output = Command::new("sh")
        .arg("-s")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            child.stdin.take().expect("piped stdin").write_all(script)?;
            child.wait_with_output()
        })?;
    Ok(CommandOutput::from_output(output))
}

fn run_local_command(
    program: &str,
    args: &[String],
    stdin: Option<&[u8]>,
    stage: &str,
) -> Result<CommandOutput> {
    let mut command = Command::new(program);
    command.args(args);
    if stdin.is_some() {
        command.stdin(Stdio::piped());
    }
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let output = command.spawn().and_then(|mut child| {
        if let Some(stdin) = stdin {
            child.stdin.take().expect("piped stdin").write_all(stdin)?;
        }
        child.wait_with_output()
    })?;
    let result = CommandOutput::from_output(output);
    if result.status == 0 {
        Ok(result)
    } else {
        Err(CiaoError::LocalCommand {
            stage: stage.to_owned(),
            exit: result.status,
            stdout: result.stdout,
            stderr: result.stderr,
        })
    }
}

fn local_admin_session() -> Result<()> {
    let status = Command::new("sudo").arg("-v").status()?;
    if status.success() {
        Ok(())
    } else {
        Err(CiaoError::LocalCommand {
            stage: "request local administrator privileges".to_owned(),
            exit: status.code().unwrap_or(128),
            stdout: String::new(),
            stderr: "sudo did not grant administrator privileges".to_owned(),
        })
    }
}

fn local_brew_prefix() -> PathBuf {
    let candidates = [
        PathBuf::from("/opt/homebrew/bin/brew"),
        PathBuf::from("/usr/local/bin/brew"),
        PathBuf::from("/home/linuxbrew/.linuxbrew/bin/brew"),
    ];
    for candidate in candidates {
        if candidate.is_file() {
            if let Ok(output) = Command::new(&candidate).arg("--prefix").output() {
                if output.status.success() {
                    let value = String::from_utf8_lossy(&output.stdout).trim().to_owned();
                    if !value.is_empty() {
                        return PathBuf::from(value);
                    }
                }
            }
        }
    }
    if let Some(path) = find_executable("brew") {
        if let Ok(output) = Command::new(&path).arg("--prefix").output() {
            if output.status.success() {
                let value = String::from_utf8_lossy(&output.stdout).trim().to_owned();
                if !value.is_empty() {
                    return PathBuf::from(value);
                }
            }
        }
    }
    if cfg!(target_os = "macos") && std::env::consts::ARCH == "aarch64" {
        PathBuf::from("/opt/homebrew")
    } else {
        PathBuf::from("/usr/local")
    }
}

fn find_executable(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|directory| directory.join(name))
            .find(|candidate| candidate.is_file())
    })
}

pub fn local_proxy_paths() -> Result<LocalProxyPaths> {
    if cfg!(target_os = "macos") {
        let prefix = local_brew_prefix();
        Ok(LocalProxyPaths {
            caddy_bin: prefix.join("bin/caddy"),
            caddyfile: prefix.join("etc/Caddyfile"),
            fragment_dir: prefix.join("etc/ciaoship"),
        })
    } else if cfg!(target_os = "linux") {
        let caddy_bin = find_executable("caddy")
            .or_else(|| {
                ["/usr/bin/caddy", "/usr/local/bin/caddy"]
                    .iter()
                    .map(PathBuf::from)
                    .find(|path| path.is_file())
            })
            .unwrap_or_else(|| PathBuf::from("/usr/bin/caddy"));
        Ok(LocalProxyPaths {
            caddy_bin,
            caddyfile: PathBuf::from("/etc/caddy/Caddyfile"),
            fragment_dir: PathBuf::from("/var/lib/ciaoship/local"),
        })
    } else {
        Err(CiaoError::Config(
            "local .ciao development is supported on macOS and Linux".to_owned(),
        ))
    }
}

fn caddyfile_quote(path: &Path) -> Result<String> {
    let value = path
        .to_str()
        .ok_or_else(|| CiaoError::Config("local Caddy path is not valid UTF-8".to_owned()))?;
    if value.contains(['\n', '\r']) {
        return Err(CiaoError::Config(
            "local Caddy path cannot contain newlines".to_owned(),
        ));
    }
    Ok(format!(
        "\"{}\"",
        value.replace('\\', "\\\\").replace('"', "\\\"")
    ))
}

pub fn local_caddy_fragment(plan: &LocalDevPlan) -> Result<String> {
    validate_local_name(&plan.name)?;
    if plan.domain != local_domain(&plan.name)? {
        return Err(CiaoError::Config(
            "local proxy domain does not match the project name".to_owned(),
        ));
    }
    if plan.port < 1024 {
        return Err(CiaoError::Config(
            "local development port must be at least 1024".to_owned(),
        ));
    }
    let site = format!("http://{}", plan.domain);
    match (&plan.app_type, &plan.static_root) {
        (AppType::Static, Some(root)) => Ok(format!(
            "{site} {{\n    root * {}\n    file_server\n}}\n",
            caddyfile_quote(root)?
        )),
        (AppType::Static, None) => Err(CiaoError::Config(
            "static local project has no directory to serve".to_owned(),
        )),
        (AppType::Service, _) => Ok(format!(
            "{site} {{\n    reverse_proxy 127.0.0.1:{}\n}}\n",
            plan.port
        )),
    }
}

pub fn local_setup_script() -> Result<String> {
    if cfg!(target_os = "macos") {
        Ok(r#"set -eu
command -v curl >/dev/null 2>&1 || { echo 'macOS curl is required' >&2; exit 1; }
brew_bin=''
for candidate in /opt/homebrew/bin/brew /usr/local/bin/brew /home/linuxbrew/.linuxbrew/bin/brew; do
    if [ -x "$candidate" ]; then brew_bin="$candidate"; break; fi
done
if [ -z "$brew_bin" ] && command -v brew >/dev/null 2>&1; then brew_bin=$(command -v brew); fi
if [ -z "$brew_bin" ]; then
    brew_install_script=$(mktemp -t ciaoship-homebrew)
    trap 'rm -f "$brew_install_script"' EXIT
    curl -fsSL 'https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh' -o "$brew_install_script"
    NONINTERACTIVE=1 /bin/bash "$brew_install_script"
    for candidate in /opt/homebrew/bin/brew /usr/local/bin/brew /home/linuxbrew/.linuxbrew/bin/brew; do
        if [ -x "$candidate" ]; then brew_bin="$candidate"; break; fi
    done
fi
[ -n "$brew_bin" ] || { echo 'Homebrew installation finished without a usable brew executable' >&2; exit 1; }
brew_prefix=$("$brew_bin" --prefix)
export PATH="$brew_prefix/bin:$brew_prefix/sbin:$PATH"
if ! "$brew_bin" list --formula caddy >/dev/null 2>&1; then "$brew_bin" install caddy; fi
if ! "$brew_bin" list --formula dnsmasq >/dev/null 2>&1; then "$brew_bin" install dnsmasq; fi
if ! ifconfig lo0 2>/dev/null | grep -q '10.0.0.1'; then sudo -n ifconfig lo0 alias 10.0.0.1; fi
dnsmasq_conf="$brew_prefix/etc/dnsmasq.conf"
sudo -n install -d -m 0755 "$brew_prefix/etc"
if ! sudo -n test -f "$dnsmasq_conf"; then
    printf '%s\n' '# CiaoShip .ciao resolver' 'listen-address=10.0.0.1' 'bind-interfaces' 'port=53' 'address=/.ciao/127.0.0.1' | sudo -n tee "$dnsmasq_conf" >/dev/null
elif ! sudo -n grep -Fq '# CiaoShip .ciao resolver' "$dnsmasq_conf"; then
    printf '\n%s\n' '# CiaoShip .ciao resolver' 'listen-address=10.0.0.1' 'bind-interfaces' 'port=53' 'address=/.ciao/127.0.0.1' | sudo -n tee -a "$dnsmasq_conf" >/dev/null
fi
sudo -n install -d -m 0755 /etc/resolver
printf '%s\n' 'nameserver 10.0.0.1' 'port 53' | sudo -n tee /etc/resolver/ciao >/dev/null
fragment_dir="$brew_prefix/etc/ciaoship"
caddyfile="$brew_prefix/etc/Caddyfile"
sudo -n install -d -m 0755 "$fragment_dir" "$brew_prefix/etc"
sudo -n chown "$(id -un)" "$fragment_dir"
import_line="import $fragment_dir/*.caddy"
if ! sudo -n test -f "$caddyfile"; then
    printf '%s\n' "$import_line" | sudo -n tee "$caddyfile" >/dev/null
elif ! sudo -n grep -Fqx "$import_line" "$caddyfile"; then
    printf '\n%s\n' "$import_line" | sudo -n tee -a "$caddyfile" >/dev/null
fi
caddy_bin="$brew_prefix/bin/caddy"
sudo -n "$brew_bin" services restart dnsmasq >/dev/null
sudo -n "$caddy_bin" validate --config "$caddyfile"
sudo -n "$brew_bin" services restart caddy >/dev/null
sudo -n "$caddy_bin" reload --config "$caddyfile"
printf 'ciaoship_local_setup=ready\n'
"#.to_owned())
    } else if cfg!(target_os = "linux") {
        Ok(r#"set -eu
command -v apt-get >/dev/null 2>&1 || { echo 'automatic Linux local setup requires apt-get' >&2; exit 1; }
sudo -n env DEBIAN_FRONTEND=noninteractive apt-get update
sudo -n env DEBIAN_FRONTEND=noninteractive apt-get install -y ca-certificates curl debian-archive-keyring debian-keyring apt-transport-https dnsmasq gnupg iproute2 systemd-resolved
if ! command -v caddy >/dev/null 2>&1 || ! sudo -n systemctl list-unit-files caddy.service 2>/dev/null | grep -q '^caddy.service'; then
    caddy_key=$(mktemp)
    trap 'rm -f "$caddy_key"' EXIT
    curl -fsSL 'https://dl.cloudsmith.io/public/caddy/stable/gpg.key' >"$caddy_key"
    sudo -n install -d -m 0755 /usr/share/keyrings
    sudo -n gpg --dearmor --yes -o /usr/share/keyrings/caddy-stable-archive-keyring.gpg "$caddy_key"
    curl -fsSL 'https://dl.cloudsmith.io/public/caddy/stable/debian.deb.txt' | sudo -n tee /etc/apt/sources.list.d/caddy-stable.list >/dev/null
    sudo -n chmod o+r /usr/share/keyrings/caddy-stable-archive-keyring.gpg /etc/apt/sources.list.d/caddy-stable.list
    sudo -n env DEBIAN_FRONTEND=noninteractive apt-get update
    sudo -n env DEBIAN_FRONTEND=noninteractive apt-get install -y caddy
fi
sudo -n install -d -m 0755 /etc/dnsmasq.d
printf '%s\n' '# CiaoShip .ciao resolver' 'listen-address=127.0.0.1' 'bind-interfaces' 'port=53' 'address=/.ciao/127.0.0.1' | sudo -n tee /etc/dnsmasq.d/ciaoship-ciao.conf >/dev/null
sudo -n systemctl enable --now dnsmasq
if ! command -v resolvectl >/dev/null 2>&1; then
    echo 'systemd-resolved/resolvectl is required for automatic .ciao DNS routing' >&2
    exit 1
fi
sudo -n install -d -m 0755 /etc/systemd/resolved.conf.d
printf '%s\n' '[Resolve]' 'DNS=127.0.0.1' 'Domains=~ciao' | sudo -n tee /etc/systemd/resolved.conf.d/ciaoship-ciao.conf >/dev/null
sudo -n systemctl enable --now systemd-resolved
sudo -n systemctl reload-or-restart systemd-resolved
sudo -n install -d -m 0755 /var/lib/ciaoship/local
sudo -n chown "$(id -un)" /var/lib/ciaoship/local
import_line='import /var/lib/ciaoship/local/*.caddy'
if ! sudo -n test -f /etc/caddy/Caddyfile; then
    printf '%s\n' "$import_line" | sudo -n tee /etc/caddy/Caddyfile >/dev/null
elif ! sudo -n grep -Fqx "$import_line" /etc/caddy/Caddyfile; then
    printf '\n%s\n' "$import_line" | sudo -n tee -a /etc/caddy/Caddyfile >/dev/null
fi
sudo -n caddy validate --config /etc/caddy/Caddyfile
sudo -n systemctl enable --now caddy
sudo -n systemctl reload caddy
printf 'ciaoship_local_setup=ready\n'
"#.to_owned())
    } else {
        Err(CiaoError::Config(
            "local .ciao development is supported on macOS and Linux".to_owned(),
        ))
    }
}

pub fn local_setup() -> Result<LocalSetupResult> {
    if local_setup_ready() {
        return Ok(local_setup_result());
    }
    local_admin_session()?;
    let output = run_local_script(local_setup_script()?.as_bytes())?;
    if output.status != 0 {
        return Err(CiaoError::LocalCommand {
            stage: "configure local .ciao resolver and Caddy".to_owned(),
            exit: output.status,
            stdout: output.stdout,
            stderr: output.stderr,
        });
    }
    Ok(local_setup_result())
}

fn local_setup_result() -> LocalSetupResult {
    let dependencies = if cfg!(target_os = "macos") {
        vec![
            "Homebrew (installed if missing)".to_owned(),
            "dnsmasq".to_owned(),
            "Caddy".to_owned(),
        ]
    } else {
        vec![
            "dnsmasq".to_owned(),
            "systemd-resolved".to_owned(),
            "Caddy".to_owned(),
        ]
    };
    LocalSetupResult {
        resolver: "*.ciao -> 127.0.0.1".to_owned(),
        proxy: "Caddy on http://*.ciao".to_owned(),
        dependencies,
        message: "local resolver and reverse proxy are ready".to_owned(),
    }
}

fn local_setup_ready() -> bool {
    let Ok(paths) = local_proxy_paths() else {
        return false;
    };
    let resolver_ready = if cfg!(target_os = "macos") {
        Path::new("/etc/resolver/ciao").is_file()
    } else {
        Path::new("/etc/dnsmasq.d/ciaoship-ciao.conf").is_file()
            && Path::new("/etc/systemd/resolved.conf.d/ciaoship-ciao.conf").is_file()
    };
    let services_ready = if cfg!(target_os = "macos") {
        Command::new("pgrep")
            .args(["-x", "dnsmasq"])
            .status()
            .is_ok_and(|status| status.success())
            && Command::new("pgrep")
                .args(["-x", "caddy"])
                .status()
                .is_ok_and(|status| status.success())
    } else {
        Command::new("systemctl")
            .args(["is-active", "--quiet", "dnsmasq"])
            .status()
            .is_ok_and(|status| status.success())
            && Command::new("systemctl")
                .args(["is-active", "--quiet", "caddy"])
                .status()
                .is_ok_and(|status| status.success())
    };
    resolver_ready
        && paths.caddyfile.is_file()
        && paths.fragment_dir.is_dir()
        && paths.caddy_bin.is_file()
        && services_ready
}

pub fn write_local_caddy_fragment(plan: &LocalDevPlan) -> Result<LocalProxyPaths> {
    let paths = local_proxy_paths()?;
    let fragment = local_caddy_fragment(plan)?;
    fs::create_dir_all(&paths.fragment_dir)?;
    let path = paths.fragment_dir.join(format!("{}.caddy", plan.name));
    fs::write(path, fragment)?;
    Ok(paths)
}

pub fn remove_local_caddy_fragment(name: &str) -> Result<()> {
    validate_local_name(name)?;
    let paths = local_proxy_paths()?;
    let path = paths.fragment_dir.join(format!("{name}.caddy"));
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

pub fn reload_local_caddy(paths: &LocalProxyPaths) -> Result<()> {
    let caddyfile = paths.caddyfile.to_string_lossy().into_owned();
    let caddy = paths.caddy_bin.to_string_lossy().into_owned();
    run_local_command(
        &caddy,
        &[
            "validate".to_owned(),
            "--config".to_owned(),
            caddyfile.clone(),
        ],
        None,
        "validate local Caddy configuration",
    )?;
    run_local_command(
        &caddy,
        &["reload".to_owned(), "--config".to_owned(), caddyfile],
        None,
        "reload local Caddy",
    )?;
    Ok(())
}

pub fn local_dev_script(plan: &LocalDevPlan) -> Result<Vec<u8>> {
    if plan.app_type != AppType::Service {
        return Err(CiaoError::Config(
            "static projects do not have a local process to run".to_owned(),
        ));
    }
    let run = plan
        .run_command
        .as_deref()
        .ok_or_else(|| CiaoError::Config("local service has no run command".to_owned()))?;
    if run.trim().is_empty() {
        return Err(CiaoError::Config(
            "local service run command cannot be empty".to_owned(),
        ));
    }
    let mut script = format!(
        "set -eu\ncd -- {}\nexport HOST=127.0.0.1\nexport PORT={}\n",
        shell_quote(&plan.source.to_string_lossy()),
        plan.port
    );
    for command in [&plan.install_command, &plan.build_command]
        .into_iter()
        .flatten()
    {
        if command.trim().is_empty() {
            return Err(CiaoError::Config(
                "local install/build command cannot be empty".to_owned(),
            ));
        }
        script.push_str(command);
        script.push('\n');
    }
    script.push_str(run);
    script.push('\n');
    Ok(script.into_bytes())
}

pub fn run_local_dev(plan: &LocalDevPlan) -> Result<i32> {
    let paths = local_proxy_paths()?;
    let fragment = paths.fragment_dir.join(format!("{}.caddy", plan.name));
    let caddy = paths.caddy_bin.to_string_lossy();
    let caddyfile = paths.caddyfile.to_string_lossy();
    let cleanup = format!(
        "cleanup() {{ rm -f {} >/dev/null 2>&1 || true; {} validate --config {} >/dev/null 2>&1 && {} reload --config {} >/dev/null 2>&1 || true; }}\ntrap cleanup EXIT\ntrap 'exit 130' INT TERM\n",
        shell_quote(&fragment.to_string_lossy()),
        shell_quote(&caddy),
        shell_quote(&caddyfile),
        shell_quote(&caddy),
        shell_quote(&caddyfile),
    );
    let mut script = cleanup.into_bytes();
    script.extend(local_dev_script(plan)?);
    let output = Command::new("sh")
        .arg("-s")
        .stdin(Stdio::piped())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .and_then(|mut child| {
            child
                .stdin
                .take()
                .expect("piped stdin")
                .write_all(&script)?;
            child.wait()
        })?;
    Ok(output.code().unwrap_or(128))
}

const CADDY_IMPORT: &str = "import /etc/caddy/ciaoship/*.caddy";

/// Return the fixed, idempotent host bootstrap script.
///
/// The script deliberately supports only the first-class host families. It
/// installs CiaoShip's small set of remote prerequisites and Caddy through a
/// native package manager, then leaves service supervision to systemd,
/// launchd, or Homebrew's launchd integration.
pub fn host_init_script(os: &HostOs) -> Result<String> {
    match os {
        HostOs::Linux => Ok(format!(
            r#"set -eu
command -v sudo >/dev/null 2>&1 || {{ echo 'CiaoShip requires sudo on the remote host' >&2; exit 1; }}
command -v apt-get >/dev/null 2>&1 || {{ echo 'automatic host initialization currently supports Debian/Ubuntu hosts with apt-get' >&2; exit 1; }}
sudo -n true
sudo -n env DEBIAN_FRONTEND=noninteractive apt-get update
sudo -n env DEBIAN_FRONTEND=noninteractive apt-get install -y ca-certificates curl debian-archive-keyring debian-keyring apt-transport-https gnupg iproute2 tar
if ! command -v caddy >/dev/null 2>&1 || ! sudo -n systemctl list-unit-files caddy.service 2>/dev/null | grep -q '^caddy.service'; then
    caddy_key=$(mktemp)
    trap 'rm -f "$caddy_key"' EXIT
    curl -fsSL 'https://dl.cloudsmith.io/public/caddy/stable/gpg.key' >"$caddy_key"
    sudo -n install -d -m 0755 /usr/share/keyrings
    sudo -n gpg --dearmor --yes -o /usr/share/keyrings/caddy-stable-archive-keyring.gpg "$caddy_key"
    curl -fsSL 'https://dl.cloudsmith.io/public/caddy/stable/debian.deb.txt' | sudo -n tee /etc/apt/sources.list.d/caddy-stable.list >/dev/null
    sudo -n chmod o+r /usr/share/keyrings/caddy-stable-archive-keyring.gpg /etc/apt/sources.list.d/caddy-stable.list
    sudo -n env DEBIAN_FRONTEND=noninteractive apt-get update
    sudo -n env DEBIAN_FRONTEND=noninteractive apt-get install -y caddy
fi
sudo -n install -d -m 0755 /etc/caddy/ciaoship
import_line='{CADDY_IMPORT}'
if ! sudo -n test -f /etc/caddy/Caddyfile; then
    printf '%s\n' "$import_line" | sudo -n tee /etc/caddy/Caddyfile >/dev/null
elif ! sudo -n grep -Fqx "$import_line" /etc/caddy/Caddyfile; then
    printf '\n%s\n' "$import_line" | sudo -n tee -a /etc/caddy/Caddyfile >/dev/null
fi
sudo -n caddy validate --config /etc/caddy/Caddyfile
sudo -n systemctl enable --now caddy
sudo -n systemctl reload caddy
printf 'ciaoship_host_init=ready\n'
"#
        )),
        HostOs::MacOs => Ok(format!(
            r#"set -eu
command -v tar >/dev/null 2>&1 || {{ echo 'macOS tar is required' >&2; exit 1; }}
command -v curl >/dev/null 2>&1 || {{ echo 'macOS curl is required' >&2; exit 1; }}
sudo -n true
brew_bin=''
for candidate in /opt/homebrew/bin/brew /usr/local/bin/brew /home/linuxbrew/.linuxbrew/bin/brew; do
    if [ -x "$candidate" ]; then brew_bin="$candidate"; break; fi
done
if [ -z "$brew_bin" ] && command -v brew >/dev/null 2>&1; then brew_bin=$(command -v brew); fi
if [ -z "$brew_bin" ]; then
    brew_install_script=$(mktemp -t ciaoship-homebrew)
    trap 'rm -f "$brew_install_script"' EXIT
    curl -fsSL 'https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh' -o "$brew_install_script"
    NONINTERACTIVE=1 /bin/bash "$brew_install_script"
    for candidate in /opt/homebrew/bin/brew /usr/local/bin/brew /home/linuxbrew/.linuxbrew/bin/brew; do
        if [ -x "$candidate" ]; then brew_bin="$candidate"; break; fi
    done
fi
[ -n "$brew_bin" ] || {{ echo 'Homebrew installation finished without a usable brew executable' >&2; exit 1; }}
brew_prefix=$("$brew_bin" --prefix)
export PATH="$brew_prefix/bin:$brew_prefix/sbin:$PATH"
caddy_bin=$(command -v caddy || true)
for candidate in "$brew_prefix/bin/caddy" "$brew_prefix/opt/caddy/bin/caddy" /opt/homebrew/bin/caddy /usr/local/bin/caddy; do
    if [ -z "$caddy_bin" ] && [ -x "$candidate" ]; then caddy_bin="$candidate"; fi
done
if [ -z "$caddy_bin" ]; then
    if ! "$brew_bin" list --formula caddy >/dev/null 2>&1; then "$brew_bin" install caddy; fi
    caddy_bin=$("$brew_bin" --prefix caddy)/bin/caddy
fi
[ -x "$caddy_bin" ] || {{ echo 'CiaoShip could not locate the Caddy binary after installation' >&2; exit 1; }}
sudo -n install -d -m 0755 /etc/caddy/ciaoship
sudo -n install -d -m 0755 "$brew_prefix/etc"
caddyfile="$brew_prefix/etc/Caddyfile"
import_line='{CADDY_IMPORT}'
if ! sudo -n test -f "$caddyfile"; then
    printf '%s\n' "$import_line" | sudo -n tee "$caddyfile" >/dev/null
elif ! sudo -n grep -Fqx "$import_line" "$caddyfile"; then
    printf '\n%s\n' "$import_line" | sudo -n tee -a "$caddyfile" >/dev/null
fi
sudo -n "$caddy_bin" validate --config "$caddyfile"
if ! pgrep -x caddy >/dev/null 2>&1; then sudo -n "$brew_bin" services start caddy >/dev/null; fi
sudo -n "$caddy_bin" reload --config "$caddyfile"
printf 'ciaoship_host_init=ready\n'
"#
        )),
        HostOs::Unknown(value) => Err(CiaoError::Config(format!(
            "host initialization is unsupported on OS `{value}`"
        ))),
    }
}

/// Install and configure the dependencies CiaoShip needs on a target host.
///
/// This is an explicit administrative operation. A deploy that includes a
/// domain calls it automatically; a domain-less deploy does not touch the
/// package manager or Caddy.
pub fn init_host(transport: &OpenSshTransport) -> Result<HostInitResult> {
    let platform = transport.inspect()?;
    match (&platform.os, platform.service_manager.as_str()) {
        (HostOs::Linux, "systemd") | (HostOs::MacOs, "launchd") => {}
        (HostOs::Linux, manager) => {
            return Err(CiaoError::Config(format!(
                "Linux host initialization requires systemd; detected {manager}"
            )))
        }
        (HostOs::MacOs, manager) => {
            return Err(CiaoError::Config(format!(
                "macOS host initialization requires launchd; detected {manager}"
            )))
        }
        (HostOs::Unknown(os), _) => {
            return Err(CiaoError::Config(format!(
                "host initialization is unsupported on OS {os}"
            )))
        }
    }
    let dependencies = match &platform.os {
        HostOs::Linux => vec![
            "apt-transport-https".to_owned(),
            "ca-certificates".to_owned(),
            "curl".to_owned(),
            "gnupg".to_owned(),
            "iproute2".to_owned(),
            "tar".to_owned(),
            "Caddy".to_owned(),
        ],
        HostOs::MacOs => vec![
            "Homebrew (installed if missing)".to_owned(),
            "Caddy".to_owned(),
        ],
        HostOs::Unknown(_) => Vec::new(),
    };
    remote_script(
        transport,
        "initialize host dependencies",
        &host_init_script(&platform.os)?,
    )?;
    Ok(HostInitResult {
        platform,
        dependencies,
        message: "host dependencies and Caddy are ready".to_owned(),
    })
}

fn runtime_init_script(
    os: &HostOs,
    runtime: &Runtime,
    user: &str,
    app_root: &str,
) -> Result<Option<String>> {
    validate_owner("service user", user)?;
    if !app_root.starts_with('/') || app_root.contains(['\n', '\r', ' ', ';', '|', '&', '$']) {
        return Err(CiaoError::Config(
            "runtime application path is invalid".to_owned(),
        ));
    }
    let script = match (os, runtime) {
        (_, Runtime::Static) => return Ok(None),
        (HostOs::Linux, Runtime::Rust) => {
            "set -eu\nif ! command -v cargo >/dev/null 2>&1 || ! command -v rustc >/dev/null 2>&1; then sudo -n env DEBIAN_FRONTEND=noninteractive apt-get update; sudo -n env DEBIAN_FRONTEND=noninteractive apt-get install -y cargo rustc; fi\ncommand -v cargo >/dev/null 2>&1\ncommand -v rustc >/dev/null 2>&1\n".to_owned()
        }
        (HostOs::Linux, Runtime::Go) => {
            "set -eu\nif ! command -v go >/dev/null 2>&1; then sudo -n env DEBIAN_FRONTEND=noninteractive apt-get update; sudo -n env DEBIAN_FRONTEND=noninteractive apt-get install -y golang-go; fi\ncommand -v go >/dev/null 2>&1\n".to_owned()
        }
        (HostOs::Linux, Runtime::Node) => {
            "set -eu\nif ! command -v node >/dev/null 2>&1 || ! command -v npm >/dev/null 2>&1; then sudo -n env DEBIAN_FRONTEND=noninteractive apt-get update; sudo -n env DEBIAN_FRONTEND=noninteractive apt-get install -y nodejs npm; fi\ncommand -v node >/dev/null 2>&1\ncommand -v npm >/dev/null 2>&1\n".to_owned()
        }
        (HostOs::Linux, Runtime::Bun) => format!(
            r#"set -eu
if ! command -v bun >/dev/null 2>&1; then
    sudo -n install -d -m 0755 {app_root}/shared/.bun
    sudo -n chown {user}:{user} {app_root}/shared/.bun
    sudo -n -u {user} env HOME={home} BUN_INSTALL={bun_install} sh -c 'curl -fsSL https://bun.sh/install | bash'
    sudo -n ln -sfn {bun_install}/bin/bun /usr/local/bin/bun
fi
command -v bun >/dev/null 2>&1
"#,
            app_root = shell_quote(app_root),
            user = shell_quote(user),
            home = shell_quote(&format!("{app_root}/shared")),
            bun_install = shell_quote(&format!("{app_root}/shared/.bun")),
        ),
        (HostOs::MacOs, Runtime::Rust) => macos_runtime_script("rust", &["cargo", "rustc"]),
        (HostOs::MacOs, Runtime::Go) => macos_runtime_script("go", &["go"]),
        (HostOs::MacOs, Runtime::Bun) => macos_runtime_script("bun", &["bun"]),
        (HostOs::MacOs, Runtime::Node) => macos_runtime_script("node", &["node", "npm"]),
        (HostOs::Unknown(value), _) => {
            return Err(CiaoError::Config(format!(
                "runtime initialization is unsupported on OS {value}"
            )))
        }
    };
    Ok(Some(script))
}

fn macos_runtime_script(formula: &str, commands: &[&str]) -> String {
    let checks = commands
        .iter()
        .map(|command| format!("command -v {command} >/dev/null 2>&1"))
        .collect::<Vec<_>>();
    let missing = checks
        .iter()
        .map(|check| format!("! {check}"))
        .collect::<Vec<_>>()
        .join(" || ");
    format!(
        "set -eu\nbrew_bin=''\nfor candidate in /opt/homebrew/bin/brew /usr/local/bin/brew; do if [ -x \"$candidate\" ]; then brew_bin=\"$candidate\"; break; fi; done\n[ -n \"$brew_bin\" ] || {{ echo 'Homebrew is missing; run ciaoship host init first' >&2; exit 1; }}\nif {missing}; then \"$brew_bin\" install {formula}; fi\n{checks}\n",
        formula = shell_quote(formula),
        missing = missing,
        checks = checks.join("\n"),
    )
}

fn ensure_runtime(
    transport: &OpenSshTransport,
    os: &HostOs,
    runtime: &Runtime,
    user: &str,
    app_root: &str,
) -> Result<()> {
    if let Some(script) = runtime_init_script(os, runtime, user, app_root)? {
        remote_script(transport, &format!("install {runtime} runtime"), &script)?;
    }
    Ok(())
}

pub fn healthcheck(host: &str, port: u16, health: &HealthConfig) -> Result<()> {
    if !health.path.starts_with('/')
        || health.path.contains(['\n', '\r', ' ', '#', '?'])
        || health.path.contains("..")
    {
        return Err(CiaoError::Config(
            "health.path must be a safe absolute URL path".to_owned(),
        ));
    }
    let address = format!("{host}:{port}");
    let mut addresses = address
        .to_socket_addrs()
        .map_err(|error| CiaoError::Transport {
            stage: "healthcheck".to_owned(),
            message: "could not resolve candidate address".to_owned(),
            details: format!(": {error}"),
        })?;
    let socket = addresses.next().ok_or_else(|| CiaoError::Transport {
        stage: "healthcheck".to_owned(),
        message: "candidate address has no socket".to_owned(),
        details: String::new(),
    })?;
    let mut stream =
        TcpStream::connect_timeout(&socket, Duration::from_secs(health.timeout_seconds.max(1)))?;
    stream.set_read_timeout(Some(Duration::from_secs(health.timeout_seconds.max(1))))?;
    stream.set_write_timeout(Some(Duration::from_secs(health.timeout_seconds.max(1))))?;
    let request = format!(
        "GET {} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        health.path
    );
    stream.write_all(request.as_bytes())?;
    let mut response = String::new();
    stream.read_to_string(&mut response)?;
    let status = response
        .split_whitespace()
        .nth(1)
        .and_then(|value| value.parse::<u16>().ok())
        .unwrap_or(0);
    if status != health.expected_status {
        return Err(CiaoError::Deployment {
            stage: "healthcheck".to_owned(),
            message: format!("expected HTTP {}, got {status}", health.expected_status),
            previous_release: "unknown".to_owned(),
        });
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperationResult {
    pub app: String,
    pub action: String,
    pub changed: bool,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeployResult {
    pub app: String,
    pub release: String,
    pub previous_release: Option<String>,
    pub port: Option<u16>,
    pub active: bool,
    pub dry_run: bool,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusResult {
    pub app: String,
    pub status: String,
    pub release: Option<String>,
    pub port: Option<u16>,
    pub app_type: Option<AppType>,
    pub service_manager: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReleaseInfo {
    pub app: String,
    pub release: String,
    pub active: bool,
    pub runtime: Runtime,
    pub app_type: AppType,
    pub port: Option<u16>,
    pub created_at_unix: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogsResult {
    pub app: String,
    pub logs: String,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleAction {
    Start,
    Stop,
    Restart,
}

impl LifecycleAction {
    fn as_str(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Stop => "stop",
            Self::Restart => "restart",
        }
    }
}

pub fn deploy(
    transport: &OpenSshTransport,
    source: &Path,
    plan: &ProjectPlan,
    domain: Option<&str>,
    dry_run: bool,
) -> Result<DeployResult> {
    if let Some(domain) = domain {
        validate_domain(domain)?;
    }
    let release = release_id();
    validate_identifier("release", &release)?;
    if dry_run {
        let steps = if plan.app_type == AppType::Static {
            "upload, create immutable release, activate current"
        } else {
            "upload, install dependencies, build, start candidate, healthcheck, activate service"
        };
        let planned_port = if plan.app_type == AppType::Static {
            None
        } else {
            Some(
                plan.port
                    .filter(|port| (PORT_START..=PORT_END).contains(port))
                    .unwrap_or(PORT_START),
            )
        };
        return Ok(DeployResult {
            app: plan.name.clone(),
            release,
            previous_release: None,
            port: planned_port,
            active: false,
            dry_run: true,
            message: format!(
                "Detected runtime: {}\nWould: {steps}{}",
                plan.runtime,
                domain
                    .map(|value| format!(", route {value}"))
                    .unwrap_or_default()
            ),
        });
    }
    let platform = transport.inspect()?;
    let root = host_app_root(&platform.os);
    let previous_release = read_current_release(transport, &root, &plan.name)?;
    if domain.is_some() {
        if let Err(error) = init_host(transport) {
            return Err(CiaoError::Deployment {
                stage: "host initialization".to_owned(),
                message: error.to_string(),
                previous_release: previous_release
                    .clone()
                    .unwrap_or_else(|| "none".to_owned()),
            });
        }
    }
    let retained_domain = read_existing_domain(transport, &plan.name)?;
    let effective_domain = domain.or(retained_domain.as_deref());
    let port = if plan.app_type == AppType::Static {
        None
    } else {
        Some(allocate_port(
            transport,
            &root,
            &plan.name,
            previous_release.as_deref(),
            plan.port,
        )?)
    };
    ensure_remote_layout(transport, &platform.os, &plan.name, &release)?;
    let staging = format!("/tmp/ciaoship-{}-{}", plan.name, release);
    let release_path = format!("{root}/{}/releases/{release}", plan.name);
    let user = service_user(transport, &platform.os, &plan.name)?;
    let user_group = match &platform.os {
        HostOs::MacOs => "staff".to_owned(),
        _ => user.clone(),
    };
    if let Err(error) = ensure_runtime(
        transport,
        &platform.os,
        &plan.runtime,
        &user,
        &format!("{root}/{}", plan.name),
    ) {
        return Err(CiaoError::Deployment {
            stage: "runtime initialization".to_owned(),
            message: error.to_string(),
            previous_release: previous_release
                .clone()
                .unwrap_or_else(|| "none".to_owned()),
        });
    }
    let result = (|| {
        remote_script(
            transport,
            "prepare upload staging",
            &format!(
                "set -eu\nsudo -n rm -rf {}\nsudo -n install -d -m 0755 {}\nsudo -n chown {}:{} {}\n",
                shell_quote(&staging),
                shell_quote(&staging),
                shell_quote(&user),
                shell_quote(&user_group),
                shell_quote(&staging)
            ),
        )?;
        transport.upload_tar(source, &staging)?;
        remote_script(
            transport,
            "finalize release",
            &format!(
                "set -eu\nsudo -n install -d -m 0755 {}\nsudo -n mv {} {}\nsudo -n chown -R {} {}\n",
                shell_quote(&format!("{root}/{}/releases", plan.name)),
                shell_quote(&staging),
                shell_quote(&release_path),
                shell_quote(&user),
                shell_quote(&release_path)
            ),
        )?;
        let mut manifest = ReleaseManifest::from_plan(release.clone(), source, plan);
        manifest.port = port;
        write_remote_file(
            transport,
            &format!("{release_path}/ciaoship-manifest.toml"),
            &manifest.to_toml()?,
            &user,
            "write release manifest",
        )?;
        if plan.app_type == AppType::Static {
            remote_script(
                transport,
                "activate static release",
                &switch_current_script(&platform.os, &root, &plan.name, &release_path),
            )?;
            harden_release(transport, &platform.os, &release_path)?;
        } else {
            let port = port.expect("service plans have a port");
            let start_script = start_script(
                &release_path,
                release_start_command(plan)?,
                port,
                &format!("{root}/{}/shared/env", plan.name),
            )?;
            write_remote_file(
                transport,
                &format!("{release_path}/start"),
                &start_script,
                &user,
                "write release start script",
            )?;
            remote_script(
                transport,
                "make release executable",
                &format!(
                    "set -eu\nsudo -n chmod 0755 {}\n",
                    shell_quote(&format!("{release_path}/start"))
                ),
            )?;
            if let Some(install) = plan.install_command.as_deref() {
                run_as_user_script(
                    transport,
                    &user,
                    "install dependencies",
                    &command_script(install, &release_path)?,
                )?;
            }
            if let Some(build) = plan.build_command.as_deref() {
                run_as_user_script(
                    transport,
                    &user,
                    "build",
                    &command_script(build, &release_path)?,
                )?;
            }
            let candidate_unit = service_unit_name(&plan.name, true);
            if platform.os == HostOs::MacOs {
                run_macos_candidate(transport, &user, &release_path, port, &plan.health)?;
            } else {
                install_service(
                    transport,
                    &platform.os,
                    &candidate_unit,
                    &user,
                    &release_path,
                    &format!("{root}/{}/shared/env", plan.name),
                    true,
                    "./start",
                )?;
                service_action(
                    transport,
                    &platform.os,
                    &candidate_unit,
                    LifecycleAction::Start,
                )?;
                remote_healthcheck(transport, port, &plan.health)?;
                service_action(
                    transport,
                    &platform.os,
                    &candidate_unit,
                    LifecycleAction::Stop,
                )?;
                remove_service(transport, &platform.os, &candidate_unit)?;
            }
            harden_release(transport, &platform.os, &release_path)?;
            let stable_unit = service_unit_name(&plan.name, false);
            install_service(
                transport,
                &platform.os,
                &stable_unit,
                &user,
                &format!("{root}/{}/current", plan.name),
                &format!("{root}/{}/shared/env", plan.name),
                false,
                "./start",
            )?;
            remote_script(
                transport,
                "activate release",
                &switch_current_script(&platform.os, &root, &plan.name, &release_path),
            )?;
            enable_service(transport, &platform.os, &stable_unit)?;
            service_action(
                transport,
                &platform.os,
                &stable_unit,
                LifecycleAction::Restart,
            )?;
            remote_healthcheck(transport, port, &plan.health)?;
        }
        if let Some(domain) = effective_domain {
            configure_domain(transport, &plan.name, domain)?;
        }
        prune_releases(transport, &root, &plan.name, 5)?;
        Ok::<(), CiaoError>(())
    })();
    if let Err(error) = result {
        let _ = remove_service(
            transport,
            &platform.os,
            &service_unit_name(&plan.name, true),
        );
        let current_after_error = read_current_release(transport, &root, &plan.name)
            .ok()
            .flatten();
        if let Some(previous) = previous_release.as_deref() {
            if current_after_error.as_deref() == Some(release.as_str()) {
                let _ = switch_current(transport, &root, &plan.name, previous);
                if plan.app_type == AppType::Service {
                    let _ = service_action(
                        transport,
                        &platform.os,
                        &service_unit_name(&plan.name, false),
                        LifecycleAction::Restart,
                    );
                }
                if let Some(domain) = retained_domain.as_deref() {
                    let _ = configure_domain(transport, &plan.name, domain);
                }
            }
        } else {
            if effective_domain.is_some() {
                let _ = remove_domain_fragment(transport, &plan.name);
            }
            if plan.app_type == AppType::Service {
                let _ = service_action(
                    transport,
                    &platform.os,
                    &service_unit_name(&plan.name, false),
                    LifecycleAction::Stop,
                );
            }
            if current_after_error.as_deref() == Some(release.as_str()) {
                let _ = remote_script(
                    transport,
                    "remove failed activation",
                    &format!(
                        "set -eu\nsudo -n rm -f {}/current\n",
                        shell_quote(&format!("{root}/{}", plan.name))
                    ),
                );
            }
            let _ = remove_service(
                transport,
                &platform.os,
                &service_unit_name(&plan.name, false),
            );
        }
        let current_after_recovery = match read_current_release(transport, &root, &plan.name) {
            Ok(current) => current,
            Err(_) => Some(release.clone()),
        };
        if current_after_recovery.as_deref() != Some(release.as_str()) {
            let _ = cleanup_release(transport, &platform.os, &plan.name, &release);
        }
        let active_message = previous_release
            .as_deref()
            .map(|previous| format!("previous release `{previous}` was restored when possible"))
            .unwrap_or_else(|| "no previous release existed".to_owned());
        return Err(CiaoError::Deployment {
            stage: "deploy".to_owned(),
            message: format!("{}; {active_message}", error),
            previous_release: previous_release.unwrap_or_else(|| "none".to_owned()),
        });
    }
    Ok(DeployResult {
        app: plan.name.clone(),
        release: release.clone(),
        previous_release,
        port,
        active: true,
        dry_run: false,
        message: format!("✓ release {release} active"),
    })
}

pub fn app_status(transport: &OpenSshTransport, app: &str) -> Result<StatusResult> {
    validate_identifier("app name", app)?;
    let platform = transport.inspect()?;
    let root = host_app_root(&platform.os);
    let release = read_current_release(transport, &root, app)?;
    let manifest = release
        .as_deref()
        .map(|release| read_release_manifest(transport, &root, app, release))
        .transpose()?;
    let status = match manifest.as_ref().map(|manifest| &manifest.app_type) {
        Some(AppType::Static) => "active".to_owned(),
        Some(AppType::Service) | None => match &platform.os {
            HostOs::Linux | HostOs::MacOs => {
                service_state(transport, &platform.os, &service_unit_name(app, false))?
            }
            HostOs::Unknown(_) => "unsupported".to_owned(),
        },
    };
    Ok(StatusResult {
        app: app.to_owned(),
        status: status.clone(),
        release,
        port: manifest.as_ref().and_then(|manifest| manifest.port),
        app_type: manifest.map(|manifest| manifest.app_type),
        service_manager: match platform.os {
            HostOs::Linux => "systemd".to_owned(),
            HostOs::MacOs => "launchd".to_owned(),
            HostOs::Unknown(value) => value,
        },
        message: format!("{app}: {status}"),
    })
}

pub fn list_apps(transport: &OpenSshTransport) -> Result<Vec<StatusResult>> {
    let platform = transport.inspect()?;
    let root = host_app_root(&platform.os);
    let output = remote_script(
        transport,
        "list applications",
        &format!(
            "set -eu\nif sudo -n test -d {}; then for path in {}/*; do if sudo -n test -d \"$path\"; then basename \"$path\"; fi; done; fi\n",
            shell_quote(&root),
            shell_quote(&root)
        ),
    )?;
    let mut apps = Vec::new();
    for app in output
        .stdout
        .lines()
        .map(str::trim)
        .filter(|app| !app.is_empty())
    {
        validate_identifier("app name", app)?;
        apps.push(app_status(transport, app)?);
    }
    apps.sort_by(|left, right| left.app.cmp(&right.app));
    Ok(apps)
}

pub fn list_releases(transport: &OpenSshTransport, app: &str) -> Result<Vec<ReleaseInfo>> {
    validate_identifier("app name", app)?;
    let platform = transport.inspect()?;
    let root = host_app_root(&platform.os);
    let current = read_current_release(transport, &root, app)?;
    let output = remote_script(
        transport,
        "list releases",
        &format!(
            "set -eu\nif sudo -n test -d {root}/{app}/releases; then for path in {root}/{app}/releases/*; do if sudo -n test -d \"$path\"; then basename \"$path\"; fi; done; fi\n",
            root = shell_quote(&root),
            app = shell_quote(app)
        ),
    )?;
    let mut releases = Vec::new();
    for release in output
        .stdout
        .lines()
        .map(str::trim)
        .filter(|release| !release.is_empty())
    {
        validate_identifier("release", release)?;
        let manifest = read_release_manifest(transport, &root, app, release)?;
        releases.push(ReleaseInfo {
            app: app.to_owned(),
            release: release.to_owned(),
            active: current.as_deref() == Some(release),
            runtime: manifest.runtime,
            app_type: manifest.app_type,
            port: manifest.port,
            created_at_unix: manifest.created_at_unix,
        });
    }
    releases.sort_by(|left, right| right.release.cmp(&left.release));
    Ok(releases)
}

pub fn app_logs(
    transport: &OpenSshTransport,
    app: &str,
    follow: bool,
    since: Option<&str>,
) -> Result<LogsResult> {
    validate_identifier("app name", app)?;
    let platform = transport.inspect()?;
    if follow {
        return Err(CiaoError::Config(
            "`logs --follow` is not available over synchronous SSH; omit it for a bounded snapshot"
                .to_owned(),
        ));
    }
    let root = host_app_root(&platform.os);
    if let Some(release) = read_current_release(transport, &root, app)? {
        let manifest = read_release_manifest(transport, &root, app, &release)?;
        if manifest.app_type == AppType::Static {
            return Err(CiaoError::Config(format!(
                "app `{app}` is static and has no service logs"
            )));
        }
    }
    let result = match platform.os {
        HostOs::Linux => {
            let mut args = vec![
                "-u".to_owned(),
                service_unit_name(app, false),
                "--no-pager".to_owned(),
            ];
            if let Some(since) = since {
                validate_since(since)?;
                args.extend(["--since".to_owned(), since.to_owned()]);
            }
            let command = CommandSpec {
                program: "sudo".to_owned(),
                args: {
                    let mut sudo_args = vec!["-n".to_owned(), "journalctl".to_owned()];
                    sudo_args.extend(args);
                    sudo_args
                },
                stdin: None,
                stage: "read logs".to_owned(),
            };
            transport.exec(command.clone())?
        }
        HostOs::MacOs => {
            if follow {
                return Err(CiaoError::Config(
                    "`logs --follow` is not available over synchronous SSH; omit it for a bounded snapshot".to_owned(),
                ));
            }
            let command = CommandSpec::fixed("sh", &["-s"], "read logs").with_stdin(
                format!(
                    "set -eu\nlog show --last 1h --predicate 'process == \"{}\"' --style compact\n",
                    app
                )
                .into_bytes(),
            );
            transport.exec(command)?
        }
        HostOs::Unknown(_) => {
            return Err(CiaoError::Config("unsupported host OS for logs".to_owned()))
        }
    };
    Ok(LogsResult {
        app: app.to_owned(),
        logs: result.stdout,
        message: format!("logs for {app}"),
    })
}

pub fn lifecycle_action(
    transport: &OpenSshTransport,
    app: &str,
    action: LifecycleAction,
) -> Result<OperationResult> {
    validate_identifier("app name", app)?;
    let platform = transport.inspect()?;
    let root = host_app_root(&platform.os);
    if let Some(release) = read_current_release(transport, &root, app)? {
        let manifest = read_release_manifest(transport, &root, app, &release)?;
        if manifest.app_type == AppType::Static {
            return Err(CiaoError::Config(format!(
                "app `{app}` is static and has no service lifecycle"
            )));
        }
    }
    let unit = service_unit_name(app, false);
    service_action(transport, &platform.os, &unit, action)?;
    Ok(OperationResult {
        app: app.to_owned(),
        action: action.as_str().to_owned(),
        changed: true,
        message: format!("✓ {action:?} {app}"),
    })
}

pub fn rollback(transport: &OpenSshTransport, app: &str) -> Result<OperationResult> {
    validate_identifier("app name", app)?;
    let platform = transport.inspect()?;
    let root = host_app_root(&platform.os);
    let current = read_current_release(transport, &root, app)?
        .ok_or_else(|| CiaoError::Config(format!("app `{app}` has no active release")))?;
    let previous = previous_release(transport, &root, app, &current)?.ok_or_else(|| {
        CiaoError::Config(format!(
            "app `{app}` has no previous release to roll back to"
        ))
    })?;
    let current_manifest = read_release_manifest(transport, &root, app, &current)?;
    let manifest = read_release_manifest(transport, &root, app, &previous)?;
    let previous_path = format!("{root}/{app}/releases/{previous}");
    let current_path = format!("{root}/{app}/releases/{current}");
    let retained_domain = read_existing_domain(transport, app)?;
    if manifest.app_type == AppType::Service {
        validate_release_candidate(transport, &platform.os, app, &previous_path, &manifest)?;
    }
    let activation = (|| {
        remote_script(
            transport,
            "rollback activation",
            &switch_current_script(&platform.os, &root, app, &previous_path),
        )?;
        if manifest.app_type == AppType::Service {
            service_action(
                transport,
                &platform.os,
                &service_unit_name(app, false),
                LifecycleAction::Restart,
            )?;
            remote_healthcheck(
                transport,
                manifest
                    .port
                    .ok_or_else(|| CiaoError::Config("rollback release has no port".to_owned()))?,
                &manifest.health,
            )?;
        }
        if let Some(domain) = retained_domain.as_deref() {
            add_domain(transport, app, domain)?;
        }
        Ok::<(), CiaoError>(())
    })();
    if let Err(error) = activation {
        let restore = (|| {
            remote_script(
                transport,
                "restore active release after rollback failure",
                &switch_current_script(&platform.os, &root, app, &current_path),
            )?;
            if current_manifest.app_type == AppType::Service {
                service_action(
                    transport,
                    &platform.os,
                    &service_unit_name(app, false),
                    LifecycleAction::Restart,
                )?;
                remote_healthcheck(
                    transport,
                    current_manifest.port.ok_or_else(|| {
                        CiaoError::Config("active release has no port".to_owned())
                    })?,
                    &current_manifest.health,
                )?;
            }
            if let Some(domain) = retained_domain.as_deref() {
                add_domain(transport, app, domain)?;
            }
            Ok::<(), CiaoError>(())
        })();
        let recovery = match restore {
            Ok(()) => format!("active release `{current}` was restored"),
            Err(restore_error) => {
                format!("active release `{current}` may require manual recovery: {restore_error}")
            }
        };
        return Err(CiaoError::Deployment {
            stage: "rollback".to_owned(),
            message: format!("{error}; {recovery}"),
            previous_release: current,
        });
    }
    Ok(OperationResult {
        app: app.to_owned(),
        action: "rollback".to_owned(),
        changed: true,
        message: format!("✓ rolled back {app} from {current} to {previous}"),
    })
}

pub fn set_env(transport: &OpenSshTransport, app: &str, key: &str, value: &str) -> Result<()> {
    validate_identifier("app name", app)?;
    validate_env_key(key)?;
    if value.contains(['\n', '\r']) {
        return Err(CiaoError::Config(
            "environment values cannot contain newlines".to_owned(),
        ));
    }
    if value.len() > 64 * 1024 {
        return Err(CiaoError::Config(
            "environment value exceeds the 64 KiB limit".to_owned(),
        ));
    }
    let platform = transport.inspect()?;
    let root = host_app_root(&platform.os);
    let user = service_user(transport, &platform.os, app)?;
    let path = format!("{root}/{app}/shared/env");
    let script = format!(
        "set -eu\nroot={}\nfile={}\nsudo -n install -d -m 0755 \"$root\"\nsudo -n touch \"$file\"\nsudo -n chmod 0600 \"$file\"\nsudo -n sed -i.bak '/^{}=/d' \"$file\"\nprintf '%s=%s\\n' {} {} | sudo -n tee -a \"$file\" >/dev/null\nsudo -n rm -f \"$file.bak\"\nsudo -n chown {} \"$file\"\n",
        shell_quote(&format!("{root}/{app}/shared")),
        shell_quote(&path),
        regex_literal(key),
        shell_quote(key),
        shell_quote(value),
        shell_quote(&user),
    );
    remote_script(transport, "set environment", &script)
        .map_err(|error| redact_error(error, &[value]))?;
    Ok(())
}

pub fn unset_env(transport: &OpenSshTransport, app: &str, key: &str) -> Result<()> {
    validate_identifier("app name", app)?;
    validate_env_key(key)?;
    let platform = transport.inspect()?;
    let root = host_app_root(&platform.os);
    let path = format!("{root}/{app}/shared/env");
    remote_script(
        transport,
        "unset environment",
        &format!(
            "set -eu\nif sudo -n test -f {}; then sudo -n sed -i.bak '/^{}=/d' {}; sudo -n rm -f {}.bak; fi\n",
            shell_quote(&path),
            regex_literal(key),
            shell_quote(&path),
            shell_quote(&path)
        ),
    )?;
    Ok(())
}

pub fn add_domain(transport: &OpenSshTransport, app: &str, domain: &str) -> Result<()> {
    validate_identifier("app name", app)?;
    validate_domain(domain)?;
    init_host(transport)?;
    configure_domain(transport, app, domain)
}

fn configure_domain(transport: &OpenSshTransport, app: &str, domain: &str) -> Result<()> {
    validate_identifier("app name", app)?;
    validate_domain(domain)?;
    let platform = transport.inspect()?;
    let root = host_app_root(&platform.os);
    let release = read_current_release(transport, &root, app)?
        .ok_or_else(|| CiaoError::Config(format!("app `{app}` has no active release")))?;
    let fragment = caddy_fragment(transport, &root, app, &release, domain)?;
    let fragment_path = format!("/etc/caddy/ciaoship/{app}.caddy");
    remote_script(
        transport,
        "prepare Caddy directory",
        "set -eu\nsudo -n install -d -m 0755 /etc/caddy/ciaoship\n",
    )?;
    write_remote_file(
        transport,
        &fragment_path,
        &fragment,
        "root",
        "write Caddy fragment",
    )?;
    remote_script(
        transport,
        "reload Caddy",
        &caddy_reload_script(&platform.os),
    )?;
    Ok(())
}

fn remove_domain_fragment(transport: &OpenSshTransport, app: &str) -> Result<()> {
    validate_identifier("app name", app)?;
    let path = format!("/etc/caddy/ciaoship/{app}.caddy");
    remote_script(
        transport,
        "cleanup Caddy fragment",
        &format!("set -eu\nsudo -n rm -f {}\n", shell_quote(&path)),
    )?;
    Ok(())
}

fn caddy_fragment(
    transport: &OpenSshTransport,
    root: &str,
    app: &str,
    release: &str,
    domain: &str,
) -> Result<String> {
    if let Some(static_directory) = read_release_static_directory(transport, root, app, release)? {
        let static_root = format!("{root}/{app}/releases/{release}/{static_directory}");
        Ok(format!(
            "{domain} {{\n    root * {}\n    file_server\n}}\n",
            static_root
        ))
    } else {
        let port = read_release_port(transport, root, app, release)?.unwrap_or(PORT_START);
        Ok(format!(
            "{domain} {{\n    reverse_proxy 127.0.0.1:{port}\n}}\n"
        ))
    }
}

fn read_existing_domain(transport: &OpenSshTransport, app: &str) -> Result<Option<String>> {
    validate_identifier("app name", app)?;
    let path = format!("/etc/caddy/ciaoship/{app}.caddy");
    let output = remote_script(
        transport,
        "read existing domain",
        &format!(
            "set -eu\nif sudo -n test -f {}; then sudo -n awk 'NR == 1 {{print $1; exit}}' {}; fi\n",
            shell_quote(&path),
            shell_quote(&path)
        ),
    )?;
    let value = output.stdout.trim();
    if value.is_empty() {
        Ok(None)
    } else {
        validate_domain(value)?;
        Ok(Some(value.to_owned()))
    }
}

pub fn remove_domain(transport: &OpenSshTransport, app: &str, domain: &str) -> Result<()> {
    validate_identifier("app name", app)?;
    validate_domain(domain)?;
    let existing = read_existing_domain(transport, app)?;
    match existing.as_deref() {
        None => return Ok(()),
        Some(existing) if existing != domain => {
            return Err(CiaoError::Config(format!(
                "app `{app}` is configured for `{existing}`, not `{domain}`"
            )))
        }
        Some(_) => {}
    }
    let platform = transport.inspect()?;
    let path = format!("/etc/caddy/ciaoship/{app}.caddy");
    remote_script(
        transport,
        "remove Caddy fragment",
        &format!(
            "set -eu\nsudo -n rm -f {}\n{}",
            shell_quote(&path),
            caddy_reload_script(&platform.os)
        ),
    )?;
    Ok(())
}

fn caddy_reload_script(os: &HostOs) -> String {
    let (setup, config, reload) = match os {
        HostOs::MacOs => (
            r#"caddy_config=''
brew_bin=''
for candidate in /opt/homebrew/bin/brew /usr/local/bin/brew /home/linuxbrew/.linuxbrew/bin/brew; do
    if [ -x "$candidate" ]; then brew_bin="$candidate"; break; fi
done
[ -n "$brew_bin" ] || { echo 'Homebrew is not available for the remote Caddy service' >&2; exit 1; }
brew_prefix=$("$brew_bin" --prefix)
caddy_config="$brew_prefix/etc/Caddyfile"
"#,
            "\"$caddy_config\"",
            "sudo -n \"$caddy_bin\" reload --config \"$caddy_config\"",
        ),
        _ => ("", "/etc/caddy/Caddyfile", "sudo -n systemctl reload caddy"),
    };
    format!(
        "set -eu\n{setup}sudo -n test -f {config}\nif ! sudo -n grep -Fq 'import /etc/caddy/ciaoship/*.caddy' {config}; then echo 'Caddyfile must import /etc/caddy/ciaoship/*.caddy' >&2; exit 1; fi\ncaddy_bin=$(command -v caddy || true)\nfor candidate in /opt/homebrew/bin/caddy /usr/local/bin/caddy /opt/homebrew/opt/caddy/bin/caddy /usr/bin/caddy; do if [ -z \"$caddy_bin\" ] && [ -x \"$candidate\" ]; then caddy_bin=\"$candidate\"; fi; done\nif [ -z \"$caddy_bin\" ]; then echo 'Caddy is not installed; run host initialization' >&2; exit 1; fi\nsudo -n \"$caddy_bin\" validate --config {config} && {reload}\n",
        setup = setup,
        config = config,
        reload = reload,
    )
}

pub fn validate_domain(domain: &str) -> Result<()> {
    if domain.len() > 253
        || domain.is_empty()
        || domain.starts_with('.')
        || domain.ends_with('.')
        || domain.split('.').any(|label| {
            label.is_empty()
                || label.len() > 63
                || label.starts_with('-')
                || label.ends_with('-')
                || !label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
    {
        return Err(CiaoError::InvalidIdentifier {
            field: "domain",
            value: domain.to_owned(),
            reason: "must contain DNS labels only",
        });
    }
    Ok(())
}

fn host_app_root(os: &HostOs) -> String {
    match os {
        HostOs::MacOs => "/Library/Ciaoship/apps".to_owned(),
        _ => APP_ROOT.to_owned(),
    }
}

fn service_user(transport: &OpenSshTransport, os: &HostOs, app: &str) -> Result<String> {
    let user = match os {
        HostOs::MacOs => ssh_login_user(&transport.target).ok_or_else(|| {
            CiaoError::Config(
                "macOS LaunchDaemon requires an explicit user@host SSH target".to_owned(),
            )
        })?,
        _ => format!("ciaoship-{app}"),
    };
    validate_owner("service user", &user)?;
    Ok(user)
}

fn validate_owner(field: &'static str, value: &str) -> Result<()> {
    if value.is_empty()
        || value.starts_with('-')
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
    {
        return Err(CiaoError::InvalidIdentifier {
            field,
            value: value.to_owned(),
            reason: "only letters, numbers, `.`, `_` and `-` are allowed",
        });
    }
    Ok(())
}

fn ssh_login_user(target: &str) -> Option<String> {
    let value = target.split('@').next().unwrap_or(target);
    if value.is_empty() || value == target {
        None
    } else {
        Some(value.to_owned())
    }
}

fn ensure_remote_layout(
    transport: &OpenSshTransport,
    os: &HostOs,
    app: &str,
    release: &str,
) -> Result<()> {
    validate_identifier("app name", app)?;
    validate_identifier("release", release)?;
    let root = host_app_root(os);
    let user = service_user(transport, os, app)?;
    let script = match os {
        HostOs::Linux => format!(
            "set -eu\nif ! id -u {user} >/dev/null 2>&1; then sudo -n useradd --system --user-group --home-dir {app_root} --shell /usr/sbin/nologin {user}; fi\nsudo -n install -d -m 0755 {root}/{app}/releases {root}/{app}/shared\nsudo -n chown root:root {root}/{app} {root}/{app}/releases\nsudo -n chown {user}:{user} {root}/{app}/shared\nsudo -n chmod 0755 {root}/{app} {root}/{app}/releases\nsudo -n chmod 0750 {root}/{app}/shared\n",
            user = shell_quote(&user),
            app_root = shell_quote(&format!("{root}/{app}")),
            root = shell_quote(&root),
            app = shell_quote(app),
        ),
        HostOs::MacOs => format!(
            "set -eu\nsudo -n install -d -m 0755 {root}/{app}/releases {root}/{app}/shared\nsudo -n chown root:wheel {root}/{app} {root}/{app}/releases\nsudo -n chown {user}:staff {root}/{app}/shared\nsudo -n chmod 0755 {root}/{app} {root}/{app}/releases\nsudo -n chmod 0750 {root}/{app}/shared\n",
            root = shell_quote(&root),
            app = shell_quote(app),
            user = shell_quote(&user),
        ),
        HostOs::Unknown(value) => {
            return Err(CiaoError::Config(format!("unsupported host OS `{value}`")))
        }
    };
    remote_script(transport, "prepare remote layout", &script)?;
    Ok(())
}

fn read_current_release(
    transport: &OpenSshTransport,
    root: &str,
    app: &str,
) -> Result<Option<String>> {
    validate_identifier("app name", app)?;
    let path = format!("{root}/{app}/current");
    let output = remote_script(
        transport,
        "read current release",
        &format!(
            "set -eu\nif test -L {}; then basename \"$(readlink {})\"; fi\n",
            shell_quote(&path),
            shell_quote(&path)
        ),
    )?;
    let value = output.stdout.trim();
    if value.is_empty() {
        Ok(None)
    } else {
        validate_identifier("release", value)?;
        Ok(Some(value.to_owned()))
    }
}

fn previous_release(
    transport: &OpenSshTransport,
    root: &str,
    app: &str,
    current: &str,
) -> Result<Option<String>> {
    validate_identifier("release", current)?;
    let output = remote_script(
        transport,
        "find previous release",
        &format!(
            "set -eu\nfor release in $(ls -1dt {root}/{app}/releases/* 2>/dev/null || true); do name=$(basename \"$release\"); case \"$name\" in {current}) continue;; esac; printf '%s\\n' \"$name\"; break; done\n",
            root = shell_quote(root),
            app = shell_quote(app),
            current = current
        ),
    )?;
    let value = output.stdout.trim();
    if value.is_empty() {
        Ok(None)
    } else {
        validate_identifier("release", value)?;
        Ok(Some(value.to_owned()))
    }
}

fn read_release_manifest(
    transport: &OpenSshTransport,
    root: &str,
    app: &str,
    release: &str,
) -> Result<ReleaseManifest> {
    validate_identifier("app name", app)?;
    validate_identifier("release", release)?;
    let path = format!("{root}/{app}/releases/{release}/ciaoship-manifest.toml");
    let output = remote_script(
        transport,
        "read release manifest",
        &format!("set -eu\nsudo -n cat {}\n", shell_quote(&path)),
    )?;
    toml::from_str(output.stdout.trim())
        .map_err(|error| CiaoError::Config(format!("release manifest is invalid: {error}")))
}

fn read_release_port(
    transport: &OpenSshTransport,
    root: &str,
    app: &str,
    release: &str,
) -> Result<Option<u16>> {
    Ok(read_release_manifest(transport, root, app, release)?.port)
}

fn read_release_static_directory(
    transport: &OpenSshTransport,
    root: &str,
    app: &str,
    release: &str,
) -> Result<Option<String>> {
    Ok(read_release_manifest(transport, root, app, release)?.static_directory)
}

fn allocate_port(
    transport: &OpenSshTransport,
    root: &str,
    app: &str,
    current: Option<&str>,
    requested: Option<u16>,
) -> Result<u16> {
    let current_port = current.and_then(|release| {
        read_release_port(transport, root, app, release)
            .ok()
            .flatten()
    });
    let start = match (requested, current_port) {
        (Some(port), Some(active)) if port == active => port.saturating_add(1),
        (Some(port), _) if (PORT_START..=PORT_END).contains(&port) => port,
        _ => current_port
            .map(|port| port.saturating_add(1).min(PORT_END))
            .unwrap_or(PORT_START),
    };
    let output = remote_script(
        transport,
        "allocate internal port",
        &format!(
            "set -eu\nif command -v ss >/dev/null 2>&1; then ss -ltnH 2>/dev/null | awk '{{print $4}}' > /tmp/ciaoship-ports.$$; elif command -v lsof >/dev/null 2>&1; then lsof -nP -iTCP -sTCP:LISTEN 2>/dev/null | awk 'NR > 1 {{print $9}}' > /tmp/ciaoship-ports.$$; elif command -v netstat >/dev/null 2>&1; then netstat -ltn 2>/dev/null | awk 'NR > 2 {{print $4}}' > /tmp/ciaoship-ports.$$; else echo 'port allocation requires ss, lsof or netstat' >&2; exit 1; fi\ntrap 'rm -f /tmp/ciaoship-ports.$$' EXIT\nfor port in $(seq {start} {PORT_END}); do\n  if grep -Eq \"([.:])$port$\" /tmp/ciaoship-ports.$$; then continue; fi\n  printf '%s\\n' \"$port\"\n  exit 0\ndone\nexit 1\n"
        ),
    )?;
    output
        .stdout
        .trim()
        .parse::<u16>()
        .map_err(|_| CiaoError::Config("remote port allocator returned invalid port".to_owned()))
}

fn start_script(release_path: &str, command: &str, port: u16, env_file: &str) -> Result<String> {
    if !release_path.starts_with('/') || release_path.contains(['\n', '\r']) {
        return Err(CiaoError::Config("release path is invalid".to_owned()));
    }
    if !env_file.starts_with('/') || env_file.contains(['\n', '\r']) {
        return Err(CiaoError::Config("environment path is invalid".to_owned()));
    }
    if command.trim().is_empty() {
        return Err(CiaoError::Config(
            "service run command cannot be empty".to_owned(),
        ));
    }
    Ok(format!(
        "#!/bin/sh\nset -eu\ncd -- {}\nif test -f {}; then set -a; . {}; set +a; fi\nexport HOST=127.0.0.1\nexport PORT={}\nexec {}\n",
        shell_quote(release_path),
        shell_quote(env_file),
        shell_quote(env_file),
        port,
        command
    ))
}

fn harden_release(transport: &OpenSshTransport, os: &HostOs, release_path: &str) -> Result<()> {
    if !release_path.starts_with('/') || release_path.contains(['\n', '\r', ';', '|', '&']) {
        return Err(CiaoError::Config("release path is invalid".to_owned()));
    }
    let group = match os {
        HostOs::MacOs => "wheel",
        _ => "root",
    };
    remote_script(
        transport,
        "harden release ownership",
        &format!(
            "set -eu\nsudo -n chown -R root:{group} {}\nsudo -n chmod -R a-w {}\nsudo -n find {} -type d -exec chmod 0755 {{}} +\nsudo -n find {} -type f -name start -exec chmod 0755 {{}} +\n",
            shell_quote(release_path),
            shell_quote(release_path),
            shell_quote(release_path),
            shell_quote(release_path)
        ),
    )?;
    Ok(())
}

fn redact_error(error: CiaoError, secrets: &[&str]) -> CiaoError {
    let redact = |value: String| {
        secrets
            .iter()
            .filter(|secret| !secret.is_empty())
            .fold(value, |value, secret| value.replace(secret, "[REDACTED]"))
    };
    match error {
        CiaoError::RemoteCommand {
            stage,
            exit,
            stdout,
            stderr,
        } => CiaoError::RemoteCommand {
            stage,
            exit,
            stdout: redact(stdout),
            stderr: redact(stderr),
        },
        CiaoError::Transport {
            stage,
            message,
            details,
        } => CiaoError::Transport {
            stage,
            message: redact(message),
            details: redact(details),
        },
        CiaoError::Deployment {
            stage,
            message,
            previous_release,
        } => CiaoError::Deployment {
            stage,
            message: redact(message),
            previous_release,
        },
        other => other,
    }
}

fn run_macos_candidate(
    transport: &OpenSshTransport,
    user: &str,
    release_path: &str,
    port: u16,
    health: &HealthConfig,
) -> Result<()> {
    validate_owner("service user", user)?;
    let pid_file = format!("/tmp/ciaoship-candidate-{}.pid", port);
    let script = format!(
        "set -eu\ncd -- {}\nnohup ./start > /tmp/ciaoship-candidate-{}.log 2>&1 &\nprintf '%s\\n' \"$!\" | tee {} >/dev/null\n",
        shell_quote(release_path),
        port,
        shell_quote(&pid_file)
    );
    remote_script(transport, "start macOS candidate", &script)?;
    let health_result = remote_healthcheck(transport, port, health);
    let stop_result = remote_script(
        transport,
        "stop macOS candidate",
        &format!(
            "set -eu\nif test -s {}; then pid=$(cat {}); case \"$pid\" in (*[!0-9]*|'') exit 1;; esac; kill \"$pid\" 2>/dev/null || true; fi\nrm -f {}\n",
            shell_quote(&pid_file),
            shell_quote(&pid_file),
            shell_quote(&pid_file)
        ),
    )
    .map(|_| ());
    match (health_result, stop_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Err(health_error), Err(stop_error)) => Err(CiaoError::Deployment {
            stage: "stop macOS candidate".to_owned(),
            message: format!("{health_error}; candidate cleanup also failed: {stop_error}"),
            previous_release: "unknown".to_owned(),
        }),
    }
}

fn switch_current_script(os: &HostOs, root: &str, app: &str, release_path: &str) -> String {
    let group = match os {
        HostOs::MacOs => "wheel",
        _ => "root",
    };
    format!(
        "set -eu\nsudo -n test -d {}\nsudo -n ln -sfn {} {}/current\nsudo -n chown -h root:{group} {}/current\n",
        shell_quote(release_path),
        shell_quote(release_path),
        shell_quote(&format!("{root}/{app}")),
        shell_quote(&format!("{root}/{app}")),
    )
}

fn switch_current(
    transport: &OpenSshTransport,
    root: &str,
    app: &str,
    release: &str,
) -> Result<()> {
    validate_identifier("app name", app)?;
    validate_identifier("release", release)?;
    let os = if root.starts_with("/Library/") {
        HostOs::MacOs
    } else {
        HostOs::Linux
    };
    remote_script(
        transport,
        "restore active release",
        &switch_current_script(&os, root, app, &format!("{root}/{app}/releases/{release}")),
    )?;
    Ok(())
}

fn validate_release_candidate(
    transport: &OpenSshTransport,
    os: &HostOs,
    app: &str,
    release_path: &str,
    manifest: &ReleaseManifest,
) -> Result<()> {
    let port = manifest
        .port
        .ok_or_else(|| CiaoError::Config("service release has no port".to_owned()))?;
    let user = service_user(transport, os, app)?;
    if *os == HostOs::MacOs {
        run_macos_candidate(transport, &user, release_path, port, &manifest.health)
    } else {
        let unit = service_unit_name(app, true);
        install_service(
            transport,
            os,
            &unit,
            &user,
            release_path,
            &format!("{}/{app}/shared/env", host_app_root(os)),
            true,
            "./start",
        )?;
        let result = (|| {
            service_action(transport, os, &unit, LifecycleAction::Start)?;
            remote_healthcheck(transport, port, &manifest.health)?;
            Ok::<(), CiaoError>(())
        })();
        let _ = service_action(transport, os, &unit, LifecycleAction::Stop);
        let _ = remove_service(transport, os, &unit);
        result
    }
}

fn write_remote_file(
    transport: &OpenSshTransport,
    path: &str,
    contents: &str,
    owner: &str,
    stage: &str,
) -> Result<()> {
    if path.contains(['\n', '\r', ';', '|', '&', '$', '`', ' ']) || !path.starts_with('/') {
        return Err(CiaoError::Config("remote file path is invalid".to_owned()));
    }
    validate_owner("file owner", owner)?;
    let command = CommandSpec {
        program: "sudo".to_owned(),
        args: vec!["-n".to_owned(), "tee".to_owned(), path.to_owned()],
        stdin: Some(contents.as_bytes().to_vec()),
        stage: stage.to_owned(),
    };
    transport.exec(command)?;
    remote_script(
        transport,
        stage,
        &format!(
            "set -eu\nsudo -n chown {} {}\n",
            shell_quote(owner),
            shell_quote(path)
        ),
    )?;
    Ok(())
}

fn run_as_user_script(
    transport: &OpenSshTransport,
    user: &str,
    stage: &str,
    script: &[u8],
) -> Result<()> {
    validate_owner("service user", user)?;
    let command = CommandSpec {
        program: "sudo".to_owned(),
        args: vec![
            "-n".to_owned(),
            "-u".to_owned(),
            user.to_owned(),
            "sh".to_owned(),
            "-s".to_owned(),
        ],
        stdin: Some(script.to_vec()),
        stage: stage.to_owned(),
    };
    transport.exec(command).map(|_| ())
}

fn remote_script(transport: &OpenSshTransport, stage: &str, script: &str) -> Result<CommandOutput> {
    transport.exec(CommandSpec::fixed("sh", &["-s"], stage).with_stdin(script.as_bytes().to_vec()))
}

fn service_unit_name(app: &str, candidate: bool) -> String {
    if candidate {
        format!("ciaoship-{app}-candidate.service")
    } else {
        format!("ciaoship-{app}.service")
    }
}

#[allow(clippy::too_many_arguments)]
fn install_service(
    transport: &OpenSshTransport,
    os: &HostOs,
    unit: &str,
    user: &str,
    working_directory: &str,
    env_file: &str,
    candidate: bool,
    command: &str,
) -> Result<()> {
    let app = unit
        .strip_prefix("ciaoship-")
        .unwrap_or(unit)
        .trim_end_matches("-candidate.service")
        .trim_end_matches(".service");
    validate_identifier("app name", app)?;
    let unit_contents = match os {
        HostOs::Linux => format!(
            "[Unit]\nDescription=CiaoShip app {app}\nAfter=network.target\n\n[Service]\nUser={user}\nWorkingDirectory={working_directory}\nEnvironmentFile=-{env_file}\nExecStart=/bin/sh -lc {exec}\nRestart={}\nRestartSec=2\n\n[Install]\nWantedBy=multi-user.target\n",
            if candidate { "no" } else { "on-failure" },
            app = app,
            user = user,
            working_directory = working_directory,
            env_file = env_file,
            exec = shell_quote(&format!("exec {command}")),
        ),
        HostOs::MacOs => {
            if candidate {
                return Err(CiaoError::Config(
                    "candidate launchd activation requires a separate plist and is not enabled yet".to_owned(),
                ));
            }
            launchd_plist(app, working_directory, command, user)?
        }
        HostOs::Unknown(value) => return Err(CiaoError::Config(format!("unsupported host OS `{value}`"))),
    };
    let path = match os {
        HostOs::Linux => format!("/etc/systemd/system/{unit}"),
        HostOs::MacOs => format!(
            "/Library/LaunchDaemons/dev.ciaoship.{}.plist",
            app.trim_end_matches("-candidate")
        ),
        HostOs::Unknown(_) => unreachable!(),
    };
    write_remote_file(
        transport,
        &path,
        &unit_contents,
        "root",
        "install service definition",
    )?;
    remote_script(
        transport,
        "reload service manager",
        match os {
            HostOs::Linux => "set -eu\nsudo -n systemctl daemon-reload\n",
            HostOs::MacOs => "set -eu\ntrue\n",
            HostOs::Unknown(_) => unreachable!(),
        },
    )?;
    Ok(())
}

fn enable_service(transport: &OpenSshTransport, os: &HostOs, unit: &str) -> Result<()> {
    validate_service_unit(unit)?;
    match os {
        HostOs::Linux => {
            remote_script(
                transport,
                "enable service at boot",
                &format!("set -eu\nsudo -n systemctl enable {}\n", shell_quote(unit)),
            )?;
        }
        HostOs::MacOs => {}
        HostOs::Unknown(value) => {
            return Err(CiaoError::Config(format!("unsupported host OS `{value}`")))
        }
    }
    Ok(())
}

fn service_action(
    transport: &OpenSshTransport,
    os: &HostOs,
    unit: &str,
    action: LifecycleAction,
) -> Result<()> {
    validate_service_unit(unit)?;
    match os {
        HostOs::Linux => {
            let verb = action.as_str();
            remote_script(
                transport,
                "service lifecycle",
                &format!("set -eu\nsudo -n systemctl {verb} {}\n", shell_quote(unit)),
            )?;
            if matches!(action, LifecycleAction::Start | LifecycleAction::Restart) {
                remote_script(
                    transport,
                    "verify service lifecycle",
                    &format!(
                        "set -eu\nsudo -n systemctl is-active --quiet {}\n",
                        shell_quote(unit)
                    ),
                )?;
            }
        }
        HostOs::MacOs => {
            let label = unit
                .trim_end_matches(".service")
                .replace("-candidate", ".candidate");
            let plist = format!(
                "/Library/LaunchDaemons/dev.ciaoship.{}.plist",
                label.trim_start_matches("ciaoship-")
            );
            let script = match action {
                LifecycleAction::Start => format!("set -eu\nsudo -n launchctl bootstrap system {} 2>/dev/null || sudo -n launchctl kickstart -k system/dev.ciaoship.{}\n", shell_quote(&plist), shell_quote(&label)),
                LifecycleAction::Stop => format!("set -eu\nsudo -n launchctl bootout system/dev.ciaoship.{} 2>/dev/null || true\n", shell_quote(&label)),
                LifecycleAction::Restart => format!("set -eu\nsudo -n launchctl bootout system/dev.ciaoship.{} 2>/dev/null || true\nsudo -n launchctl bootstrap system {}\n", shell_quote(&label), shell_quote(&plist)),
            };
            remote_script(transport, "service lifecycle", &script)?;
        }
        HostOs::Unknown(value) => {
            return Err(CiaoError::Config(format!("unsupported host OS `{value}`")))
        }
    }
    Ok(())
}

fn remove_service(transport: &OpenSshTransport, os: &HostOs, unit: &str) -> Result<()> {
    validate_service_unit(unit)?;
    match os {
        HostOs::Linux => remote_script(transport, "remove candidate service", &format!("set -eu\nsudo -n systemctl disable --now {} 2>/dev/null || true\nsudo -n rm -f /etc/systemd/system/{}\nsudo -n systemctl daemon-reload\n", shell_quote(unit), shell_quote(unit)))?,
        HostOs::MacOs => {
            let label = unit
                .trim_end_matches(".service")
                .replace("-candidate", ".candidate");
            let plist = format!(
                "/Library/LaunchDaemons/dev.ciaoship.{}.plist",
                label.trim_start_matches("ciaoship-")
            );
            remote_script(
                transport,
                "remove candidate service",
                &format!(
                    "set -eu\nsudo -n launchctl bootout system/dev.ciaoship.{} 2>/dev/null || true\nsudo -n rm -f {}\n",
                    shell_quote(&label),
                    shell_quote(&plist)
                ),
            )?
        }
        HostOs::Unknown(value) => return Err(CiaoError::Config(format!("unsupported host OS `{value}`"))),
    };
    Ok(())
}

fn service_state(transport: &OpenSshTransport, os: &HostOs, unit: &str) -> Result<String> {
    validate_service_unit(unit)?;
    let output = match os {
        HostOs::Linux => remote_script(
            transport,
            "read service status",
            &format!(
                "set -eu\nsudo -n systemctl is-active {} 2>/dev/null || true\n",
                shell_quote(unit)
            ),
        )?,
        HostOs::MacOs => {
            let label = unit
                .trim_end_matches(".service")
                .trim_start_matches("ciaoship-");
            remote_script(
                transport,
                "read service status",
                &format!(
                    "set -eu\nif sudo -n launchctl print system/dev.ciaoship.{} >/dev/null 2>&1; then printf 'active\\n'; else printf 'inactive\\n'; fi\n",
                    shell_quote(label)
                ),
            )?
        }
        HostOs::Unknown(value) => {
            return Err(CiaoError::Config(format!("unsupported host OS `{value}`")))
        }
    };
    Ok(output.stdout.trim().to_owned().if_empty("unknown"))
}

fn remote_healthcheck(
    transport: &OpenSshTransport,
    port: u16,
    health: &HealthConfig,
) -> Result<()> {
    let url = format!("http://127.0.0.1:{port}{}", health.path);
    let script = format!(
        "set -eu\nexpected={}\nif command -v curl >/dev/null 2>&1; then actual=$(curl --silent --show-error --max-time {} --output /dev/null --write-out '%{{http_code}}' {}); elif command -v wget >/dev/null 2>&1; then actual=$(wget --server-response --spider --timeout={} {} 2>&1 | awk '/HTTP\\// {{code=$2}} END {{print code}}'); else echo 'curl or wget is required for HTTP healthchecks' >&2; exit 1; fi\n[ \"$actual\" = \"$expected\" ] || {{ echo \"expected HTTP $expected, got $actual\" >&2; exit 1; }}\n",
        health.expected_status,
        health.timeout_seconds,
        shell_quote(&url),
        health.timeout_seconds,
        shell_quote(&url)
    );
    remote_script(transport, "healthcheck", &script).map(|_| ())
}

fn cleanup_release(
    transport: &OpenSshTransport,
    os: &HostOs,
    app: &str,
    release: &str,
) -> Result<()> {
    validate_identifier("app name", app)?;
    validate_identifier("release", release)?;
    let root = host_app_root(os);
    let current = read_current_release(transport, &root, app)?;
    if current.as_deref() == Some(release) {
        return Err(CiaoError::Config(
            "refusing to clean a release currently selected by current".to_owned(),
        ));
    }
    remote_script(
        transport,
        "cleanup failed release",
        &format!(
            "set -eu\nsudo -n rm -rf {}/{}/releases/{} /tmp/ciaoship-{}-{}\n",
            shell_quote(&root),
            shell_quote(app),
            shell_quote(release),
            shell_quote(app),
            shell_quote(release)
        ),
    )
    .map(|_| ())
}

fn prune_releases(transport: &OpenSshTransport, root: &str, app: &str, keep: usize) -> Result<()> {
    let current = read_current_release(transport, root, app)?;
    let current_case = current.unwrap_or_default();
    remote_script(
        transport,
        "prune old releases",
        &format!(
            "set -eu\ncount=0\nfor release in $(ls -1dt {root}/{app}/releases/* 2>/dev/null || true); do name=$(basename \"$release\"); if [ \"$name\" = {current} ]; then continue; fi; count=$((count+1)); if [ $count -gt {keep} ]; then sudo -n rm -rf \"$release\"; fi; done\n",
            root = shell_quote(root),
            app = shell_quote(app),
            current = shell_quote(&current_case),
        ),
    )?;
    Ok(())
}

fn validate_service_unit(unit: &str) -> Result<()> {
    if !unit.starts_with("ciaoship-")
        || !unit.ends_with(".service")
        || unit.contains(['/', ' ', '\n', '\r', ';', '|', '&', '$', '`'])
    {
        return Err(CiaoError::Config("invalid managed service name".to_owned()));
    }
    Ok(())
}

fn validate_since(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 32
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-+ _.".contains(&byte))
    {
        return Err(CiaoError::Config("invalid --since value".to_owned()));
    }
    Ok(())
}

fn regex_literal(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('.', "\\.")
        .replace('*', "\\*")
        .replace('[', "\\[")
        .replace(']', "\\]")
        .replace('^', "\\^")
        .replace('$', "\\$")
}

trait StringIfEmpty {
    fn if_empty(self, fallback: &str) -> String;
}

impl StringIfEmpty for String {
    fn if_empty(self, fallback: &str) -> String {
        if self.is_empty() {
            fallback.to_owned()
        } else {
            self
        }
    }
}

pub fn systemd_unit(
    app: &str,
    user: &str,
    working_directory: &str,
    command: &str,
) -> Result<String> {
    validate_identifier("app name", app)?;
    validate_identifier("Unix user", user)?;
    if !working_directory.starts_with('/') || working_directory.contains(['\n', '\r']) {
        return Err(CiaoError::Config("working directory is invalid".to_owned()));
    }
    Ok(format!(
        "[Unit]\nDescription=CiaoShip app {app}\nAfter=network.target\n\n[Service]\nUser={user}\nWorkingDirectory={working_directory}\nEnvironmentFile={APP_ROOT}/{app}/shared/env\nExecStart=/bin/sh -lc {command}\nRestart=on-failure\nRestartSec=2\n\n[Install]\nWantedBy=multi-user.target\n"
    ))
}

pub fn launchd_plist(
    app: &str,
    working_directory: &str,
    command: &str,
    user: &str,
) -> Result<String> {
    validate_identifier("app name", app)?;
    validate_owner("launchd user", user)?;
    if !working_directory.starts_with('/') || working_directory.contains(['\n', '\r']) {
        return Err(CiaoError::Config("working directory is invalid".to_owned()));
    }
    let label = format!("dev.ciaoship.{app}");
    Ok(format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\"><dict>\n<key>Label</key><string>{label}</string>\n<key>UserName</key><string>{user}</string>\n<key>ProgramArguments</key><array><string>/bin/sh</string><string>-lc</string><string>{}</string></array>\n<key>WorkingDirectory</key><string>{working_directory}</string>\n<key>KeepAlive</key><true/>\n<key>RunAtLoad</key><true/>\n</dict></plist>\n",
        xml_escape(command),
        label = xml_escape(&label),
        user = xml_escape(user),
    ))
}

pub fn remote_path(app: &str, suffix: &str) -> Result<String> {
    validate_identifier("app name", app)?;
    if suffix.is_empty()
        || !suffix.starts_with('/')
        || suffix.contains("..")
        || suffix.contains(['\n', '\r', ';', '|', '&', '$', '`'])
    {
        return Err(CiaoError::Config("invalid remote CiaoShip path".to_owned()));
    }
    Ok(format!("{APP_ROOT}/{app}{suffix}"))
}

fn validate_upload_destination(destination: &str) -> Result<()> {
    if !destination.starts_with("/tmp/ciaoship-")
        || destination.contains("..")
        || destination.contains(['\n', '\r', ';', '|', '&', '$', '`', ' '])
        || !destination
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"/_-".contains(&byte))
    {
        return Err(CiaoError::Config("invalid upload staging path".to_owned()));
    }
    Ok(())
}

fn exit_code(status: ExitStatus) -> i32 {
    status.code().unwrap_or(128)
}

pub fn truncate(value: &str) -> String {
    const LIMIT: usize = 16 * 1024;
    if value.len() <= LIMIT {
        value.to_owned()
    } else {
        format!(
            "{}\n[… output truncated …]",
            &value[..value.floor_char_boundary(LIMIT)]
        )
    }
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn identifiers_reject_shell_injection() {
        for value in ["x;rm", "x$(id)", "-oProxyCommand=evil", "x y", ""] {
            assert!(validate_identifier("app", value).is_err());
        }
    }

    #[test]
    fn ssh_target_rejects_options_and_shell() {
        for value in [
            "-oProxyCommand=id",
            "user@host;id",
            "user@host/../x",
            "u@h@x",
            "u@host:22",
        ] {
            assert!(validate_ssh_target(value).is_err(), "{value}");
        }
        assert!(validate_ssh_target("user@server").is_ok());
    }

    #[test]
    fn detection_is_deterministic() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("go.mod"), "module example.com/app\n").unwrap();
        fs::write(
            directory.path().join("ciaoship.toml"),
            "[app]\nname = \"go-demo\"\n",
        )
        .unwrap();
        let plan = detect_project(directory.path()).unwrap();
        assert_eq!(plan.runtime, Runtime::Go);
        assert_eq!(plan.run_command.as_deref(), Some("./app"));
    }

    #[test]
    fn custom_config_overrides_detected_plan() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("package.json"), "{}\n").unwrap();
        fs::write(
            directory.path().join("ciaoship.toml"),
            "[app]\nname = \"api\"\n[build]\ninstall = \"npm ci --ignore-scripts\"\ncommand = \"npm run compile\"\n[run]\ncommand = \"node server.js\"\nport = 8080\n[health]\npath = \"/health\"\ntimeout = \"3s\"\n",
        )
        .unwrap();
        let plan = detect_project(directory.path()).unwrap();
        assert_eq!(plan.name, "api");
        assert_eq!(plan.port, Some(8080));
        assert_eq!(plan.health.timeout_seconds, 3);
        assert_eq!(plan.build_command.as_deref(), Some("npm run compile"));
    }

    #[test]
    fn local_dev_keeps_name_and_mapping_stable_while_avoiding_collisions() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("package.json"), "{}\n").unwrap();
        fs::write(
            directory.path().join("ciaoship.toml"),
            "[app]\nname = \"api\"\n[dev]\nname = \"admin\"\nport = 41001\ncommand = \"node server.js\"\n",
        )
        .unwrap();
        let detected = detect_project(directory.path()).unwrap();
        let config = Config::default();
        let first = local_dev_plan(directory.path(), &detected, None, &config.local).unwrap();
        assert_eq!(first.name, "admin");
        assert_eq!(first.domain, "admin.ciao");
        assert_eq!(first.port, 41001);

        let mut persisted = config.local.clone();
        persisted.projects.insert(
            first.name.clone(),
            LocalProject {
                domain: first.domain.clone(),
                port: first.port,
                source: first.source.display().to_string(),
                app_type: Some(AppType::Service),
                static_root: None,
            },
        );
        let second = local_dev_plan(directory.path(), &detected, None, &persisted).unwrap();
        assert_eq!(second.port, first.port);
    }

    #[test]
    fn local_proxy_covers_service_and_static_projects() {
        let directory = tempfile::tempdir().unwrap();
        let service = LocalDevPlan {
            name: "api".to_owned(),
            domain: "api.ciao".to_owned(),
            port: 41000,
            source: directory.path().to_path_buf(),
            runtime: Runtime::Node,
            app_type: AppType::Service,
            install_command: None,
            build_command: None,
            run_command: Some("node server.js".to_owned()),
            static_root: None,
        };
        assert!(local_caddy_fragment(&service)
            .unwrap()
            .contains("reverse_proxy 127.0.0.1:41000"));

        let static_plan = LocalDevPlan {
            name: "site".to_owned(),
            domain: "site.ciao".to_owned(),
            port: 41001,
            source: directory.path().to_path_buf(),
            runtime: Runtime::Static,
            app_type: AppType::Static,
            install_command: None,
            build_command: None,
            run_command: None,
            static_root: Some(directory.path().join("dist")),
        };
        assert!(local_caddy_fragment(&static_plan)
            .unwrap()
            .contains("file_server"));
    }

    #[test]
    fn local_setup_script_installs_native_dependencies_without_hosts_entries() {
        let script = local_setup_script().unwrap();
        assert!(script.contains("dnsmasq"));
        assert!(script.contains("Caddy"));
        assert!(!script.contains("/etc/hosts"));
        assert!(script.contains("address=/.ciao/127.0.0.1"));
    }

    #[test]
    fn manifest_is_serializable() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(
            directory.path().join("Cargo.toml"),
            "[package]\nname=\"demo\"\n",
        )
        .unwrap();
        fs::write(
            directory.path().join("ciaoship.toml"),
            "[app]\nname = \"rust-demo\"\n",
        )
        .unwrap();
        let plan = detect_project(directory.path()).unwrap();
        let manifest = ReleaseManifest::from_plan("r1".to_owned(), directory.path(), &plan);
        assert!(manifest.to_toml().unwrap().contains("runtime = \"Rust\""));
    }

    #[test]
    fn config_round_trip() {
        let mut file = NamedTempFile::new().unwrap();
        let mut config = Config::default();
        config.add_host(Host::new("home", "user@server").unwrap());
        config.save(file.path()).unwrap();
        file.flush().unwrap();
        let loaded = Config::load(file.path()).unwrap();
        assert_eq!(loaded.hosts["home"].ssh, "user@server");
    }

    #[test]
    fn generated_service_rejects_untrusted_identifiers() {
        assert!(systemd_unit("bad;rm", "ciaoship-bad", "/tmp", "./app").is_err());
        assert!(launchd_plist("good", "/tmp", "./app", "luca")
            .unwrap()
            .contains("KeepAlive"));
    }

    #[test]
    fn shell_script_uses_fixed_cwd_quote() {
        let script = command_script("echo ok", "/tmp/a'b").unwrap();
        assert!(String::from_utf8(script).unwrap().contains("'\\''"));
    }

    #[test]
    fn host_bootstrap_covers_caddy_and_native_package_managers() {
        let linux = host_init_script(&HostOs::Linux).unwrap();
        assert!(linux.contains("apt-get install -y"));
        assert!(linux.contains("dl.cloudsmith.io/public/caddy/stable"));
        assert!(linux.contains(CADDY_IMPORT));
        assert!(linux.contains("systemctl enable --now caddy"));

        let macos = host_init_script(&HostOs::MacOs).unwrap();
        assert!(macos.contains("install.sh"));
        assert!(macos.contains("install caddy"));
        assert!(macos.contains(CADDY_IMPORT));
    }

    #[test]
    fn runtime_bootstrap_is_scoped_to_detected_runtime() {
        let node = runtime_init_script(
            &HostOs::Linux,
            &Runtime::Node,
            "ciaoship-demo",
            "/var/lib/ciaoship/apps/demo",
        )
        .unwrap()
        .unwrap();
        assert!(node.contains("apt-get install -y nodejs npm"));
        assert!(!node.contains("caddy"));

        let bun = runtime_init_script(
            &HostOs::Linux,
            &Runtime::Bun,
            "ciaoship-demo",
            "/var/lib/ciaoship/apps/demo",
        )
        .unwrap()
        .unwrap();
        assert!(bun.contains("https://bun.sh/install"));
        assert!(bun.contains("BUN_INSTALL"));
    }
}
