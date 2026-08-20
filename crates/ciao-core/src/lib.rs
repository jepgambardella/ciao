//! Shared Ciao domain, detection and deployment primitives.
//!
//! The CLI and MCP layers deliberately depend on this crate instead of
//! invoking one another. Remote work is performed through OpenSSH, while the
//! target machine remains a normal systemd/launchd host.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt::{Display, Formatter};
use std::fs;
use std::io::{self, Read, Write};
use std::net::{IpAddr, TcpStream, ToSocketAddrs};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use thiserror::Error;

pub const APP_ROOT: &str = "/var/lib/ciao/apps";
pub const CONFIG_ENV: &str = "CIAO_CONFIG";
pub const GITHUB_CONFIG_ENV: &str = "CIAO_GITHUB_CONFIG";
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

static CANCELLATION_REQUESTED: AtomicBool = AtomicBool::new(false);

pub fn request_cancellation() {
    CANCELLATION_REQUESTED.store(true, Ordering::SeqCst);
}

pub fn cancellation_requested() -> bool {
    CANCELLATION_REQUESTED.load(Ordering::SeqCst)
}

pub fn reset_cancellation() {
    CANCELLATION_REQUESTED.store(false, Ordering::SeqCst);
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Host {
    #[serde(skip)]
    pub name: String,
    pub ssh: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity_file: Option<PathBuf>,
}

impl Host {
    pub fn new(name: impl Into<String>, ssh: impl Into<String>) -> Result<Self> {
        let name = name.into();
        validate_identifier("host name", &name)?;
        let ssh = ssh.into();
        validate_ssh_target(&ssh)?;
        Ok(Self {
            name,
            ssh,
            identity_file: None,
        })
    }

    pub fn with_identity_file(mut self, identity_file: PathBuf) -> Result<Self> {
        validate_identity_file(&identity_file)?;
        self.identity_file = Some(identity_file);
        Ok(self)
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

/// Receives coarse-grained lifecycle events without coupling the core to a
/// terminal UI. The CLI renders these as spinners; MCP, JSON and CI use the
/// no-op implementation.
pub trait ProgressReporter {
    fn started(&self, _step: &str) {}
    fn updated(&self, _message: &str) {}
    fn finished(&self, _step: &str) {}
    fn failed(&self, _step: &str) {}
    fn cancelled(&self) -> bool {
        cancellation_requested()
    }
}

fn progress_step<T>(
    reporter: &dyn ProgressReporter,
    name: &str,
    action: impl FnOnce() -> Result<T>,
) -> Result<T> {
    if reporter.cancelled() {
        reporter.failed(name);
        return Err(CiaoError::Config(
            "operation interrupted by user".to_owned(),
        ));
    }
    reporter.started(name);
    match action() {
        Ok(value) => {
            reporter.finished(name);
            Ok(value)
        }
        Err(error) => {
            reporter.failed(name);
            if reporter.cancelled() {
                Err(CiaoError::Config(format!(
                    "operation interrupted by user during {name}: {error}"
                )))
            } else {
                Err(error)
            }
        }
    }
}

fn progress_step_uncancellable<T>(
    reporter: &dyn ProgressReporter,
    name: &str,
    action: impl FnOnce() -> Result<T>,
) -> Result<T> {
    reporter.started(name);
    match action() {
        Ok(value) => {
            reporter.finished(name);
            Ok(value)
        }
        Err(error) => {
            reporter.failed(name);
            Err(error)
        }
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct NoopProgressReporter;

impl ProgressReporter for NoopProgressReporter {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeployHostMode {
    NonInteractive,
    Interactive,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Runtime {
    Rust,
    Go,
    Bun,
    Node,
    Astro,
    Python,
    Static,
}

impl Display for Runtime {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Rust => "Rust",
            Self::Go => "Go",
            Self::Bun => "Bun",
            Self::Node => "Node",
            Self::Astro => "Astro",
            Self::Python => "Python",
            Self::Static => "Static",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProjectRole {
    Backend,
    Frontend,
}

impl Display for ProjectRole {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Backend => "backend",
            Self::Frontend => "frontend",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectComponent {
    pub name: String,
    pub role: ProjectRole,
    pub path: PathBuf,
    pub plan: ProjectPlan,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitHubRepoRef {
    pub owner: String,
    pub repo: String,
}

impl GitHubRepoRef {
    pub fn full_name(&self) -> String {
        format!("{}/{}", self.owner, self.repo)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitHubRepository {
    pub owner: String,
    pub repo: String,
    #[serde(default)]
    pub owner_id: String,
    pub repository_id: String,
    pub default_branch: String,
    pub remote: String,
    pub private: bool,
}

impl GitHubRepository {
    pub fn reference(&self) -> GitHubRepoRef {
        GitHubRepoRef {
            owner: self.owner.clone(),
            repo: self.repo.clone(),
        }
    }

    pub fn full_name(&self) -> String {
        format!("{}/{}", self.owner, self.repo)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitHubDeploymentLink {
    pub repository: String,
    pub repository_id: String,
    pub branch: String,
    pub host: String,
    pub tailscale_host: String,
    pub ssh_user: String,
    pub federated_identity_id: Option<String>,
    pub workflow_path: PathBuf,
    #[serde(default)]
    pub public_key: Option<String>,
    #[serde(default)]
    pub ciao_version: String,
    #[serde(default)]
    pub source_token_configured: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GitHubConfig {
    #[serde(default)]
    pub links: BTreeMap<String, GitHubDeploymentLink>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TailscaleTarget {
    pub hostname: Option<String>,
    pub ipv4: Option<String>,
    pub online: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TailscaleInstallResult {
    pub executable: String,
    pub installed: bool,
}

impl TailscaleTarget {
    pub fn preferred_address(&self) -> Result<String> {
        self.hostname
            .clone()
            .or_else(|| self.ipv4.clone())
            .ok_or_else(|| {
                CiaoError::Config(
                    "Tailscale is connected but did not report a MagicDNS hostname or IPv4 address"
                        .to_owned(),
                )
            })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedSshKey {
    pub private_key: String,
    pub public_key: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowSpec {
    pub branch: String,
    pub ciao_version: String,
    pub ciao_repository: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TailscaleFederatedIdentity {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub audience: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CiTarget {
    pub host: String,
    pub user: String,
    pub app: String,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalResolverResult {
    pub resolver: String,
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
            if let Some(identity_file) = &host.identity_file {
                validate_identity_file(identity_file)?;
            }
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
        .map(|path| path.join(".config/ciao/config.toml"))
        .unwrap_or_else(|| PathBuf::from(".ciao/config.toml"))
}

fn validate_identity_file(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty()
        || path.to_string_lossy().contains(['\n', '\r'])
        || !path.is_absolute()
    {
        return Err(CiaoError::Config(
            "SSH identity file must be an absolute path without newlines".to_owned(),
        ));
    }
    Ok(())
}

pub fn github_config_path() -> PathBuf {
    if let Ok(path) = std::env::var(GITHUB_CONFIG_ENV) {
        return PathBuf::from(path);
    }
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|path| path.join(".config/ciao/github.toml"))
        .unwrap_or_else(|| PathBuf::from(".ciao/github.toml"))
}

impl GitHubConfig {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        toml::from_str(&fs::read_to_string(path)?)
            .map_err(|error| CiaoError::Config(format!("GitHub configuration is invalid: {error}")))
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
}

pub fn git_remote_origin(root: &Path) -> Result<Option<String>> {
    let output = run_local_command(
        "git",
        &[
            "-C".to_owned(),
            root.display().to_string(),
            "config".to_owned(),
            "--get".to_owned(),
            "remote.origin.url".to_owned(),
        ],
        None,
        "read GitHub origin",
    );
    match output {
        Ok(output) => {
            let remote = output.stdout.trim();
            if remote.is_empty() {
                Ok(None)
            } else {
                Ok(Some(remote.to_owned()))
            }
        }
        Err(CiaoError::LocalCommand { exit: 1, .. }) => Ok(None),
        Err(error) => Err(error),
    }
}

pub fn parse_github_remote(remote: &str) -> Result<Option<GitHubRepoRef>> {
    let value = remote.trim();
    let path = if let Some(path) = value.strip_prefix("https://github.com/") {
        path
    } else if let Some(path) = value.strip_prefix("http://github.com/") {
        path
    } else if let Some(path) = value.strip_prefix("git@github.com:") {
        path
    } else if let Some(path) = value.strip_prefix("ssh://git@github.com/") {
        path
    } else {
        return Ok(None);
    };
    let path = path.trim_end_matches('/').trim_end_matches(".git");
    let mut parts = path.split('/');
    let owner = parts.next().unwrap_or_default();
    let repo = parts.next().unwrap_or_default();
    if parts.next().is_some() || owner.is_empty() || repo.is_empty() {
        return Err(CiaoError::Config(format!(
            "GitHub remote must use owner/repository: {remote}"
        )));
    }
    validate_github_segment("GitHub owner", owner)?;
    validate_github_segment("GitHub repository", repo)?;
    Ok(Some(GitHubRepoRef {
        owner: owner.to_owned(),
        repo: repo.to_owned(),
    }))
}

pub fn detect_github_repository(root: &Path) -> Result<Option<GitHubRepository>> {
    let Some(remote) = git_remote_origin(root)? else {
        return Ok(None);
    };
    let Some(reference) = parse_github_remote(&remote)? else {
        return Ok(None);
    };
    let branch = run_local_command(
        "git",
        &[
            "-C".to_owned(),
            root.display().to_string(),
            "branch".to_owned(),
            "--show-current".to_owned(),
        ],
        None,
        "read current Git branch",
    )?
    .stdout
    .trim()
    .to_owned();
    Ok(Some(GitHubRepository {
        owner: reference.owner,
        repo: reference.repo,
        owner_id: String::new(),
        repository_id: String::new(),
        default_branch: if branch.is_empty() {
            "main".to_owned()
        } else {
            branch
        },
        remote,
        private: false,
    }))
}

pub fn github_repository_metadata(reference: &GitHubRepoRef) -> Result<GitHubRepository> {
    let output = run_local_command(
        "gh",
        &["api".to_owned(), format!("repos/{}", reference.full_name())],
        None,
        "read GitHub repository metadata",
    )?;
    let value: serde_json::Value = serde_json::from_str(&output.stdout).map_err(|error| {
        CiaoError::Config(format!(
            "GitHub returned invalid repository metadata: {error}"
        ))
    })?;
    let repository_id = value
        .get("id")
        .and_then(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .or_else(|| value.as_u64().map(|id| id.to_string()))
        })
        .filter(|id| !id.is_empty())
        .ok_or_else(|| {
            CiaoError::Config("GitHub metadata did not include repository id".to_owned())
        })?;
    let owner_id = value
        .get("owner")
        .and_then(|owner| owner.get("id"))
        .and_then(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .or_else(|| value.as_u64().map(|id| id.to_string()))
        })
        .filter(|id| !id.is_empty())
        .ok_or_else(|| CiaoError::Config("GitHub metadata did not include owner id".to_owned()))?;
    let default_branch = value
        .get("default_branch")
        .and_then(serde_json::Value::as_str)
        .filter(|branch| !branch.is_empty())
        .ok_or_else(|| {
            CiaoError::Config("GitHub metadata did not include the default branch".to_owned())
        })?;
    let private = value
        .get("private")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    Ok(GitHubRepository {
        owner: reference.owner.clone(),
        repo: reference.repo.clone(),
        owner_id,
        repository_id,
        default_branch: default_branch.to_owned(),
        remote: format!("https://github.com/{}.git", reference.full_name()),
        private,
    })
}

pub fn github_auth_status() -> Result<CommandOutput> {
    run_local_command(
        "gh",
        &["auth".to_owned(), "status".to_owned()],
        None,
        "check GitHub authentication",
    )
}

pub fn ssh_user_from_target(target: &str) -> Option<String> {
    ssh_login_user(target)
}

pub fn ssh_host_from_target(target: &str) -> &str {
    target.rsplit('@').next().unwrap_or(target)
}

pub fn ssh_target_uses_tailscale(target: &str) -> bool {
    let host = ssh_host_from_target(target);
    host.ends_with(".ts.net")
        || host
            .parse::<IpAddr>()
            .map(|address| match address {
                IpAddr::V4(address) => {
                    let octets = address.octets();
                    octets[0] == 100 && (64..=127).contains(&octets[1])
                }
                IpAddr::V6(address) => address.segments()[0] == 0xfd7a,
            })
            .unwrap_or(false)
}

pub fn github_set_secret(repository: &GitHubRepoRef, name: &str, value: &str) -> Result<()> {
    validate_github_secret_name(name)?;
    if value.is_empty() {
        return Err(CiaoError::Config(format!(
            "GitHub secret {name} cannot be empty"
        )));
    }
    run_local_command(
        "gh",
        &[
            "secret".to_owned(),
            "set".to_owned(),
            name.to_owned(),
            "--repo".to_owned(),
            repository.full_name(),
        ],
        Some(value.as_bytes()),
        &format!("configure GitHub secret {name}"),
    )?;
    Ok(())
}

pub fn github_set_variable(repository: &GitHubRepoRef, name: &str, value: &str) -> Result<()> {
    validate_github_variable_name(name)?;
    if value.is_empty() || value.contains(['\n', '\r']) {
        return Err(CiaoError::Config(format!(
            "GitHub variable {name} must be a single non-empty line"
        )));
    }
    run_local_command(
        "gh",
        &[
            "variable".to_owned(),
            "set".to_owned(),
            name.to_owned(),
            "--repo".to_owned(),
            repository.full_name(),
            "--body".to_owned(),
            value.to_owned(),
        ],
        None,
        &format!("configure GitHub variable {name}"),
    )?;
    Ok(())
}

pub fn github_delete_secret(repository: &GitHubRepoRef, name: &str) -> Result<()> {
    validate_github_secret_name(name)?;
    run_local_command(
        "gh",
        &[
            "secret".to_owned(),
            "delete".to_owned(),
            name.to_owned(),
            "--repo".to_owned(),
            repository.full_name(),
        ],
        None,
        &format!("remove GitHub secret {name}"),
    )?;
    Ok(())
}

pub fn github_delete_variable(repository: &GitHubRepoRef, name: &str) -> Result<()> {
    validate_github_variable_name(name)?;
    run_local_command(
        "gh",
        &[
            "variable".to_owned(),
            "delete".to_owned(),
            name.to_owned(),
            "--repo".to_owned(),
            repository.full_name(),
        ],
        None,
        &format!("remove GitHub variable {name}"),
    )?;
    Ok(())
}

fn validate_github_secret_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 100
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return Err(CiaoError::InvalidIdentifier {
            field: "GitHub secret",
            value: name.to_owned(),
            reason: "must contain only letters, numbers and `_`",
        });
    }
    Ok(())
}

fn validate_github_variable_name(name: &str) -> Result<()> {
    validate_github_secret_name(name).map_err(|error| match error {
        CiaoError::InvalidIdentifier { value, .. } => CiaoError::InvalidIdentifier {
            field: "GitHub variable",
            value,
            reason: "must contain only letters, numbers and `_`",
        },
        other => other,
    })
}

pub fn ci_target_from_env(env: &BTreeMap<String, String>) -> Result<CiTarget> {
    let required = |key: &str| {
        env.get(key)
            .map(String::as_str)
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| CiaoError::Config(format!("CI variable {key} is required")))
    };
    let host = required("CIAO_HOST")?.to_owned();
    let user = required("CIAO_USER")?.to_owned();
    let app = required("CIAO_APP")?.to_owned();
    validate_ssh_target(&format!("{user}@{host}"))?;
    validate_identifier("app name", &app)?;
    Ok(CiTarget { host, user, app })
}

pub fn workflow_spec_for(
    branch: impl Into<String>,
    ciao_version: impl Into<String>,
) -> WorkflowSpec {
    WorkflowSpec {
        branch: branch.into(),
        ciao_version: ciao_version.into(),
        ciao_repository: "jepgambardella/ciao".to_owned(),
    }
}

pub fn github_workflow_path(root: &Path) -> PathBuf {
    root.join(".github/workflows/ciao-deploy.yml")
}

pub fn git_revision(root: &Path) -> Result<String> {
    let revision = run_local_command(
        "git",
        &[
            "-C".to_owned(),
            root.display().to_string(),
            "rev-parse".to_owned(),
            "HEAD".to_owned(),
        ],
        None,
        "read Ciao workflow revision",
    )?
    .stdout
    .trim()
    .to_owned();
    if revision.len() != 40 || !revision.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(CiaoError::Config(
            "Ciao workflow requires a full Git commit revision".to_owned(),
        ));
    }
    Ok(revision)
}

fn yaml_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

pub fn render_github_workflow(spec: &WorkflowSpec) -> Result<String> {
    if spec.branch.is_empty() || spec.branch.contains(['\n', '\r', '\'']) {
        return Err(CiaoError::Config(
            "GitHub workflow branch is invalid".to_owned(),
        ));
    }
    if spec.ciao_version.is_empty()
        || spec.ciao_version.contains(['\n', '\r', '\''])
        || spec.ciao_version == "latest"
    {
        return Err(CiaoError::Config(
            "GitHub workflow requires a pinned Ciao version".to_owned(),
        ));
    }
    if !spec.ciao_repository.contains('/') || spec.ciao_repository.contains(['\n', '\r', '\'', ' '])
    {
        return Err(CiaoError::Config(
            "Ciao GitHub repository is invalid".to_owned(),
        ));
    }
    let branch = yaml_quote(&spec.branch);
    let version = yaml_quote(&spec.ciao_version);
    let gh = "$";
    Ok(format!(
        r#"name: Ciao Deploy

on:
  push:
    branches:
      - {branch}

permissions:
  contents: read
  id-token: write

concurrency:
  group: ciao-production
  cancel-in-progress: false

jobs:
  deploy:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - name: Checkout pinned Ciao
        uses: actions/checkout@v4
        with:
          repository: {repo}
          ref: {version}
          path: .ciao-tool
          token: {gh}{{{{ secrets.CIAO_GITHUB_TOKEN || github.token }}}}
      - name: Connect Tailscale
        uses: tailscale/github-action@v4
        with:
          oauth-client-id: {gh}{{{{ vars.TS_OAUTH_CLIENT_ID }}}}
          audience: {gh}{{{{ vars.TS_AUDIENCE }}}}
          tags: tag:ciao-ci
          ping: {gh}{{{{ vars.CIAO_HOST }}}}
      - name: Install Rust
        uses: dtolnay/rust-toolchain@stable
      - name: Install Ciao
        run: cargo install --locked --path .ciao-tool/crates/ciao
      - name: Configure SSH
        env:
          CIAO_SSH_KEY: {gh}{{{{ secrets.CIAO_SSH_KEY }}}}
          CIAO_SSH_KNOWN_HOSTS: {gh}{{{{ secrets.CIAO_SSH_KNOWN_HOSTS }}}}
        run: |
          install -d -m 700 "$HOME/.ssh"
          printf '%s' "$CIAO_SSH_KEY" > "$HOME/.ssh/ciao_ci_ed25519"
          chmod 600 "$HOME/.ssh/ciao_ci_ed25519"
          printf '%s\n' "$CIAO_SSH_KNOWN_HOSTS" > "$HOME/.ssh/known_hosts"
          chmod 600 "$HOME/.ssh/known_hosts"
          cat > "$HOME/.ssh/config" <<'EOF'
          Host ciao-target
            HostName {gh}{{{{ vars.CIAO_HOST }}}}
            User {gh}{{{{ vars.CIAO_USER }}}}
            IdentityFile ~/.ssh/ciao_ci_ed25519
            IdentitiesOnly yes
            UserKnownHostsFile ~/.ssh/known_hosts
            StrictHostKeyChecking yes
          EOF
      - name: Deploy
        env:
          CIAO_HOST: {gh}{{{{ vars.CIAO_HOST }}}}
          CIAO_USER: {gh}{{{{ vars.CIAO_USER }}}}
          CIAO_APP: {gh}{{{{ vars.CIAO_APP }}}}
        run: ciao deploy --ci --path . ciao-target
"#,
        branch = branch,
        repo = spec.ciao_repository,
        version = version,
        gh = gh,
    ))
}

pub fn tailscale_target_from_status(status: &serde_json::Value) -> Result<TailscaleTarget> {
    let backend_state = status
        .get("BackendState")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let online = backend_state == "Running";
    let hostname = status
        .get("Self")
        .and_then(|self_node| self_node.get("DNSName"))
        .and_then(serde_json::Value::as_str)
        .map(|value| value.trim_end_matches('.').to_owned());
    let ipv4 = status
        .get("TailscaleIPs")
        .and_then(serde_json::Value::as_array)
        .and_then(|values| {
            values.iter().find_map(|value| {
                let address = value.as_str()?;
                match address.parse::<IpAddr>().ok()? {
                    IpAddr::V4(_) => Some(address.to_owned()),
                    IpAddr::V6(_) => None,
                }
            })
        });
    if !online {
        return Err(CiaoError::Config(
            "Tailscale is installed but not connected on the target".to_owned(),
        ));
    }
    if hostname.is_none() && ipv4.is_none() {
        return Err(CiaoError::Config(
            "Tailscale status did not include a usable target address".to_owned(),
        ));
    }
    Ok(TailscaleTarget {
        hostname,
        ipv4,
        online,
    })
}

pub fn tailscale_status_command() -> CommandSpec {
    CommandSpec::fixed("sh", &["-s"], "inspect Tailscale status").with_stdin(
        br#"set -eu
for candidate in /usr/local/bin/tailscale /usr/local/opt/tailscale/bin/tailscale /opt/homebrew/bin/tailscale /opt/homebrew/opt/tailscale/bin/tailscale /usr/bin/tailscale /Applications/Tailscale.app/Contents/MacOS/tailscale /Applications/Tailscale.app/Contents/MacOS/Tailscale; do
    if [ -x "$candidate" ]; then exec "$candidate" status --json; fi
done
if command -v tailscale >/dev/null 2>&1; then exec tailscale status --json; fi
echo 'Tailscale CLI was not found on the target' >&2
exit 127
"#
        .to_vec(),
    )
    .with_full_output()
}

/// Find an already installed local Tailscale CLI before attempting any
/// installation. The standalone macOS app can keep the CLI outside PATH.
pub fn local_tailscale_executable() -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(path) = find_executable("tailscale") {
        candidates.push(path);
    }
    if cfg!(target_os = "macos") {
        candidates.extend([
            PathBuf::from("/usr/local/bin/tailscale"),
            PathBuf::from("/usr/local/opt/tailscale/bin/tailscale"),
            PathBuf::from("/opt/homebrew/bin/tailscale"),
            PathBuf::from("/Applications/Tailscale.app/Contents/MacOS/tailscale"),
            PathBuf::from("/Applications/Tailscale.app/Contents/MacOS/Tailscale"),
        ]);
    }
    candidates.into_iter().find(|path| path.is_file())
}

/// Install Tailscale only when it is missing from the local device. Joining a
/// tailnet is handled separately by the caller through the browser-guided
/// authentication flow below.
pub fn ensure_local_tailscale() -> Result<TailscaleInstallResult> {
    if let Some(executable) = local_tailscale_executable() {
        return Ok(TailscaleInstallResult {
            executable: executable.display().to_string(),
            installed: false,
        });
    }
    let script = if cfg!(target_os = "macos") {
        r#"set -eu
command -v curl >/dev/null 2>&1 || { echo 'macOS curl is required to install Tailscale' >&2; exit 1; }
brew_bin=''
for candidate in /opt/homebrew/bin/brew /usr/local/bin/brew /home/linuxbrew/.linuxbrew/bin/brew; do
    if [ -x "$candidate" ]; then brew_bin="$candidate"; break; fi
done
if [ -z "$brew_bin" ] && command -v brew >/dev/null 2>&1; then brew_bin=$(command -v brew); fi
if [ -z "$brew_bin" ]; then
    brew_script=$(mktemp -t ciao-homebrew)
    trap 'rm -f "$brew_script"' EXIT
    curl -fsSL 'https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh' -o "$brew_script"
    NONINTERACTIVE=1 /bin/bash "$brew_script"
    for candidate in /opt/homebrew/bin/brew /usr/local/bin/brew /home/linuxbrew/.linuxbrew/bin/brew; do
        if [ -x "$candidate" ]; then brew_bin="$candidate"; break; fi
    done
fi
[ -n "$brew_bin" ] || { echo 'Homebrew installation finished without a usable brew executable' >&2; exit 1; }
if ! "$brew_bin" list --formula tailscale >/dev/null 2>&1; then "$brew_bin" install tailscale; fi
brew_prefix=$("$brew_bin" --prefix)
export PATH="$brew_prefix/bin:$PATH"
sudo -n "$brew_bin" services start tailscale >/dev/null 2>&1 || true
tailscale_bin="$brew_prefix/opt/tailscale/bin/tailscale"
[ -x "$tailscale_bin" ] || tailscale_bin="$brew_prefix/bin/tailscale"
[ -x "$tailscale_bin" ] || { echo 'Tailscale was installed but its CLI was not found' >&2; exit 1; }
"#.to_owned()
    } else if cfg!(target_os = "linux") {
        r#"set -eu
command -v curl >/dev/null 2>&1 || { echo 'curl is required to install Tailscale' >&2; exit 1; }
curl -fsSL 'https://tailscale.com/install.sh' | sudo -n sh
if command -v systemctl >/dev/null 2>&1; then sudo -n systemctl enable --now tailscaled; fi
command -v tailscale >/dev/null 2>&1 || { echo 'Tailscale was installed but its CLI is not on PATH' >&2; exit 1; }
"#.to_owned()
    } else {
        return Err(CiaoError::Config(
            "automatic Tailscale installation is supported on macOS and Linux".to_owned(),
        ));
    };
    let output = run_local_interactive_capture(&format!(
        "set -eu\nsudo -v </dev/tty >/dev/tty 2>/dev/tty\n{script}"
    ))?;
    if output.status != 0 {
        return Err(CiaoError::LocalCommand {
            stage: "install local Tailscale client".to_owned(),
            exit: output.status,
            stdout: output.stdout,
            stderr: output.stderr,
        });
    }
    let executable = local_tailscale_executable().ok_or_else(|| {
        CiaoError::Config(
            "Tailscale installation finished, but no local `tailscale` executable was found"
                .to_owned(),
        )
    })?;
    Ok(TailscaleInstallResult {
        executable: executable.display().to_string(),
        installed: true,
    })
}

pub fn local_tailscale_target() -> Result<TailscaleTarget> {
    let executable = local_tailscale_executable().ok_or_else(|| {
        CiaoError::Config(
            "Tailscale is not installed on this device; Ciao can install it during GitHub setup"
                .to_owned(),
        )
    })?;
    let (program, args) = if cfg!(target_os = "linux") {
        (
            "sudo".to_owned(),
            vec![
                "-n".to_owned(),
                executable.display().to_string(),
                "status".to_owned(),
                "--json".to_owned(),
            ],
        )
    } else {
        (
            executable.display().to_string(),
            vec!["status".to_owned(), "--json".to_owned()],
        )
    };
    let output = run_local_command_full(&program, &args, None, "inspect local Tailscale status")?;
    let status = tailscale_status_value(&output.stdout, &output.stderr).ok_or_else(|| {
        CiaoError::Config("local Tailscale returned invalid status JSON".to_owned())
    })?;
    tailscale_target_from_status(&status)
}

/// Start a local browser-guided login and return its URL. The command is
/// detached so the caller can open the browser before waiting for Running.
pub fn start_local_tailscale_auth() -> Result<Option<String>> {
    if local_tailscale_target().is_ok() {
        return Ok(None);
    }
    let executable = local_tailscale_executable().ok_or_else(|| {
        CiaoError::Config(
            "Tailscale is not installed on this device; Ciao can install it during GitHub setup"
                .to_owned(),
        )
    })?;
    let executable = shell_quote(&executable.display().to_string());
    let privilege = if cfg!(target_os = "linux") {
        "sudo -n "
    } else {
        ""
    };
    let state = format!("/tmp/ciao-tailscale-auth-local-{}", std::process::id());
    let script = format!(
        r#"set -eu
state={state}
mkdir -p "$state"
chmod 700 "$state"
output="$state/output"
rm -f "$output"
rm -f "$state/pid"
nohup {privilege}{executable} up > "$output" 2>&1 < /dev/null &
printf '%s\n' "$!" > "$state/pid"
for attempt in $(seq 1 45); do
    url=$(grep -Eo 'https://login\.tailscale\.com/[A-Za-z0-9._/?=&%:-]+' "$output" 2>/dev/null | head -n 1 || true)
    if [ -n "$url" ]; then printf '%s\n' "$url"; exit 0; fi
    status=$({privilege}{executable} status --json 2>/dev/null || true)
    if printf '%s' "$status" | grep -q '"BackendState"[[:space:]]*:[[:space:]]*"Running"'; then
        printf '%s\n' '__CIAO_TAILSCALE_CONNECTED__'
        exit 0
    fi
    sleep 1
done
cat "$output" 2>/dev/null || true
exit 124
"#,
        state = shell_quote(&state),
        privilege = privilege,
        executable = executable,
    );
    let output = if cfg!(target_os = "linux") {
        run_local_interactive_capture(&format!(
            "set -eu\nsudo -v </dev/tty >/dev/tty 2>/dev/tty\n{script}"
        ))?
    } else {
        run_local_script(script.as_bytes())?
    };
    if output.status != 0 {
        let _ = stop_local_tailscale_auth();
        return Err(CiaoError::LocalCommand {
            stage: "start guided local Tailscale authentication".to_owned(),
            exit: output.status,
            stdout: output.stdout,
            stderr: output.stderr,
        });
    }
    if output.stdout.contains(TAILSCALE_CONNECTED_MARKER) {
        let _ = stop_local_tailscale_auth();
        return Ok(None);
    }
    if let Some(url) = tailscale_auth_url_from_output(&output.stdout) {
        Ok(Some(url))
    } else {
        let _ = stop_local_tailscale_auth();
        Err(CiaoError::Config(
            "Ciao could not obtain a local Tailscale browser login URL".to_owned(),
        ))
    }
}

/// Stop the detached local login command and remove only Ciao's temporary
/// state. The PID is checked before sending a signal so a stale file cannot
/// terminate an unrelated process after PID reuse.
pub fn stop_local_tailscale_auth() -> Result<()> {
    let state = format!("/tmp/ciao-tailscale-auth-local-{}", std::process::id());
    let script = format!(
        r#"set +e
state={state}
if test -s "$state/pid"; then
    pid=$(cat "$state/pid" 2>/dev/null || true)
    case "$pid" in
        ''|*[!0-9]*) ;;
        *)
            command=$(ps -p "$pid" -o command= 2>/dev/null || true)
            case "$command" in
                *tailscale*) kill "$pid" 2>/dev/null || true ;;
            esac
            ;;
    esac
fi
rm -rf "$state"
"#,
        state = shell_quote(&state),
    );
    run_local_script(script.as_bytes()).map(|_| ())
}

pub fn wait_for_local_tailscale_auth(timeout: Duration) -> Result<TailscaleTarget> {
    let deadline = Instant::now() + timeout;
    let mut last_error = None;
    while Instant::now() < deadline {
        match local_tailscale_target() {
            Ok(target) => {
                let _ = stop_local_tailscale_auth();
                return Ok(target);
            }
            Err(error @ CiaoError::Config(_)) | Err(error @ CiaoError::LocalCommand { .. }) => {
                last_error = Some(error)
            }
            Err(error) => return Err(error),
        }
        if cancellation_requested() {
            let _ = stop_local_tailscale_auth();
            return Err(CiaoError::Config(
                "operation interrupted by user; local Tailscale login was stopped".to_owned(),
            ));
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    let _ = stop_local_tailscale_auth();
    Err(CiaoError::Config(format!(
        "timed out waiting for local Tailscale browser authentication{}",
        last_error
            .map(|error| format!(": {error}"))
            .unwrap_or_default()
    )))
}

/// Reuse the target's Tailscale installation or install it through the
/// target's native path when the explicit GitHub setup needs it. Authentication
/// is started by Ciao and completed in the user's browser.
pub fn ensure_tailscale_target(
    transport: &OpenSshTransport,
    os: &HostOs,
) -> Result<TailscaleInstallResult> {
    let detection = remote_script(
        transport,
        "detect Tailscale installation",
        "set -eu\nfor candidate in /usr/local/bin/tailscale /usr/local/opt/tailscale/bin/tailscale /opt/homebrew/bin/tailscale /opt/homebrew/opt/tailscale/bin/tailscale /usr/bin/tailscale /Applications/Tailscale.app/Contents/MacOS/tailscale /Applications/Tailscale.app/Contents/MacOS/Tailscale; do if [ -x \"$candidate\" ]; then printf '%s\\n' \"$candidate\"; exit 0; fi; done\nif command -v tailscale >/dev/null 2>&1; then command -v tailscale; else printf '__CIAO_TAILSCALE_MISSING__\\n'; fi\n",
    )?;
    let executable = detection.stdout.trim();
    if !executable.is_empty() && executable != "__CIAO_TAILSCALE_MISSING__" {
        return Ok(TailscaleInstallResult {
            executable: executable.to_owned(),
            installed: false,
        });
    }
    remote_script(
        transport,
        "install Tailscale on target",
        &target_tailscale_install_script(os)?,
    )?;
    Ok(TailscaleInstallResult {
        executable: "tailscale".to_owned(),
        installed: true,
    })
}

fn target_tailscale_install_script(os: &HostOs) -> Result<String> {
    match os {
        HostOs::Linux => Ok(r#"set -eu
command -v curl >/dev/null 2>&1 || { echo 'curl is required to install Tailscale on Linux' >&2; exit 1; }
sudo -n true
curl -fsSL 'https://tailscale.com/install.sh' | sudo -n sh
if command -v systemctl >/dev/null 2>&1; then sudo -n systemctl enable --now tailscaled; fi
command -v tailscale >/dev/null 2>&1 || { echo 'Tailscale installation finished without a usable CLI' >&2; exit 1; }
"#.to_owned()),
        HostOs::MacOs => Ok(r#"set -eu
command -v curl >/dev/null 2>&1 || { echo 'macOS curl is required to install Tailscale' >&2; exit 1; }
brew_bin=''
for candidate in /opt/homebrew/bin/brew /usr/local/bin/brew /home/linuxbrew/.linuxbrew/bin/brew; do
    if [ -x "$candidate" ]; then brew_bin="$candidate"; break; fi
done
if [ -z "$brew_bin" ] && command -v brew >/dev/null 2>&1; then brew_bin=$(command -v brew); fi
if [ -z "$brew_bin" ]; then
    brew_script=$(mktemp -t ciao-homebrew)
    trap 'rm -f "$brew_script"' EXIT
    curl -fsSL 'https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh' -o "$brew_script"
    NONINTERACTIVE=1 /bin/bash "$brew_script"
    for candidate in /opt/homebrew/bin/brew /usr/local/bin/brew /home/linuxbrew/.linuxbrew/bin/brew; do
        if [ -x "$candidate" ]; then brew_bin="$candidate"; break; fi
    done
fi
[ -n "$brew_bin" ] || { echo 'Homebrew installation finished without a usable brew executable' >&2; exit 1; }
if ! "$brew_bin" list --formula tailscale >/dev/null 2>&1; then "$brew_bin" install tailscale; fi
brew_prefix=$("$brew_bin" --prefix)
export PATH="$brew_prefix/bin:$PATH"
sudo -n true
sudo -n "$brew_bin" services start tailscale >/dev/null 2>&1 || true
tailscale_bin="$brew_prefix/opt/tailscale/bin/tailscale"
[ -x "$tailscale_bin" ] || tailscale_bin="$brew_prefix/bin/tailscale"
[ -x "$tailscale_bin" ] || { echo 'Tailscale installation finished without a usable CLI' >&2; exit 1; }
"#.to_owned()),
        HostOs::Unknown(value) => Err(CiaoError::Config(format!(
            "automatic Tailscale installation is unsupported on host OS {value}"
        ))),
    }
}

pub fn tailscale_target(transport: &dyn RemoteHost) -> Result<TailscaleTarget> {
    let status = match transport.exec(tailscale_status_command()) {
        Ok(output) => tailscale_status_value(&output.stdout, &output.stderr)
            .ok_or_else(|| CiaoError::Config("Tailscale returned invalid status JSON".to_owned())),
        Err(CiaoError::RemoteCommand {
            ref stderr,
            ref stdout,
            ..
        }) => {
            let combined = format!("{stdout}\n{stderr}").to_ascii_lowercase();
            if combined.contains("not found") || combined.contains("command not found") {
                return Err(CiaoError::Config(
                    "Tailscale is not installed on the target; run `ciao github setup` to install it automatically".to_owned(),
                ));
            }
            match tailscale_status_value(stdout, stderr) {
                Some(status) => Ok(status),
                None => Err(CiaoError::Config(
                    "Tailscale returned invalid status JSON".to_owned(),
                )),
            }
        }
        Err(error) => Err(error),
    };
    let status = status?;
    tailscale_target_from_status(&status)
}

fn tailscale_status_value(stdout: &str, stderr: &str) -> Option<serde_json::Value> {
    [stdout, stderr].into_iter().find_map(parse_json_object)
}

/// Parse a Tailscale JSON response even when a transport prepends or appends
/// harmless text (for example an SSH login banner). The command is still
/// expected to contain a JSON object; unrelated text is never returned.
fn parse_json_object(value: &str) -> Option<serde_json::Value> {
    if let Ok(candidate) = serde_json::from_str::<serde_json::Value>(value.trim()) {
        if candidate.is_object() {
            return Some(candidate);
        }
    }
    let mut offset = 0;
    while let Some(relative) = value[offset..].find('{') {
        let start = offset + relative;
        let mut deserializer = serde_json::Deserializer::from_str(&value[start..]);
        if let Ok(candidate) = serde_json::Value::deserialize(&mut deserializer) {
            if candidate.is_object() {
                return Some(candidate);
            }
        }
        offset = start + 1;
        if offset >= value.len() {
            break;
        }
    }
    None
}

const TAILSCALE_CONNECTED_MARKER: &str = "__CIAO_TAILSCALE_CONNECTED__";

/// Extract the login URL emitted by `tailscale up` without treating any other
/// output as a URL. The value is safe to hand to a browser launcher because it
/// must use Tailscale's HTTPS login origin.
pub fn tailscale_auth_url_from_output(output: &str) -> Option<String> {
    output.split_whitespace().find_map(|token| {
        let token =
            token.trim_matches(|character: char| matches!(character, '"' | '\'' | '(' | '[' | '{'));
        let token = token.strip_prefix("https://login.tailscale.com/")?;
        let end = token
            .find(['"', '\'', ')', ']', '}', ',', ';'])
            .unwrap_or(token.len());
        let path = token[..end].trim_end_matches('.');
        if path.is_empty() {
            None
        } else {
            Some(format!("https://login.tailscale.com/{path}"))
        }
    })
}

/// Start a background `tailscale up` on the target and return its login URL.
/// The SSH command exits after the URL is available, while the detached
/// process remains alive until the browser authentication completes.
pub fn start_tailscale_auth(transport: &dyn RemoteHost) -> Result<Option<String>> {
    if tailscale_target(transport).is_ok() {
        return Ok(None);
    }
    let output = match transport.exec(tailscale_auth_start_command()) {
        Ok(output) => output,
        Err(error) => {
            let _ = stop_tailscale_auth(transport);
            return Err(error);
        }
    };
    if output.stdout.contains(TAILSCALE_CONNECTED_MARKER) {
        let _ = stop_tailscale_auth(transport);
        return Ok(None);
    }
    if let Some(url) = tailscale_auth_url_from_output(&output.stdout) {
        return Ok(Some(url));
    }
    let details = [output.stdout.trim(), output.stderr.trim()]
        .into_iter()
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    let details = details
        .lines()
        .filter(|line| !line.contains("https://login.tailscale.com/"))
        .collect::<Vec<_>>()
        .join("\n");
    let _ = stop_tailscale_auth(transport);
    Err(CiaoError::Config(format!(
        "Ciao could not obtain a Tailscale browser login URL from the target{}",
        if details.is_empty() {
            String::new()
        } else {
            format!("; target output: {details}")
        }
    )))
}

/// Stop the detached target login command and remove only its temporary Ciao
/// state. The remote script checks that the PID still belongs to Tailscale.
pub fn stop_tailscale_auth(transport: &dyn RemoteHost) -> Result<()> {
    let command = CommandSpec::fixed("sh", &["-s"], "stop guided Tailscale authentication")
        .with_stdin(
            br#"set +e
state="/tmp/ciao-tailscale-auth-$(id -u)"
if test -s "$state/pid"; then
    pid=$(cat "$state/pid" 2>/dev/null || true)
    case "$pid" in
        ''|*[!0-9]*) ;;
        *)
            command=$(ps -p "$pid" -o command= 2>/dev/null || true)
            case "$command" in
                *tailscale*) kill "$pid" 2>/dev/null || true ;;
            esac
            ;;
    esac
fi
rm -rf "$state"
"#
            .to_vec(),
        );
    transport.exec(command).map(|_| ())
}

/// Wait for the browser-guided target login to finish, with a bounded timeout
/// so a closed browser or an expired login cannot leave Ciao hanging forever.
pub fn wait_for_tailscale_auth(
    transport: &dyn RemoteHost,
    timeout: Duration,
) -> Result<TailscaleTarget> {
    let deadline = Instant::now() + timeout;
    let mut last_error = None;
    while Instant::now() < deadline {
        match tailscale_target(transport) {
            Ok(target) => {
                let _ = stop_tailscale_auth(transport);
                return Ok(target);
            }
            Err(error @ CiaoError::Config(_)) => last_error = Some(error),
            Err(error) => return Err(error),
        }
        if cancellation_requested() {
            let _ = stop_tailscale_auth(transport);
            return Err(CiaoError::Config(
                "operation interrupted by user; target Tailscale login was stopped".to_owned(),
            ));
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    let _ = stop_tailscale_auth(transport);
    Err(CiaoError::Config(format!(
        "timed out waiting for Tailscale browser authentication on the target{}",
        last_error
            .map(|error| format!(": {error}"))
            .unwrap_or_default()
    )))
}

fn tailscale_auth_start_command() -> CommandSpec {
    CommandSpec::fixed("sh", &["-s"], "start guided Tailscale authentication").with_stdin(
        br#"set -eu
state="/tmp/ciao-tailscale-auth-$(id -u)"
tailscale_bin=''
for candidate in /usr/local/bin/tailscale /usr/local/opt/tailscale/bin/tailscale /opt/homebrew/bin/tailscale /opt/homebrew/opt/tailscale/bin/tailscale /usr/bin/tailscale /Applications/Tailscale.app/Contents/MacOS/tailscale /Applications/Tailscale.app/Contents/MacOS/Tailscale; do
    if [ -x "$candidate" ]; then tailscale_bin="$candidate"; break; fi
done
if [ -z "$tailscale_bin" ] && command -v tailscale >/dev/null 2>&1; then tailscale_bin=$(command -v tailscale); fi
[ -n "$tailscale_bin" ] || { echo 'Tailscale CLI was not found on the target' >&2; exit 127; }
status=$(sudo -n "$tailscale_bin" status --json 2>/dev/null || true)
if printf '%s' "$status" | grep -q '"BackendState"[[:space:]]*:[[:space:]]*"Running"'; then
        printf '%s\n' '__CIAO_TAILSCALE_CONNECTED__'
        exit 0
fi
install -d -m 700 "$state"
rm -f "$state/output" "$state/pid"
: > "$state/output"
chmod 600 "$state/output"
nohup sudo -n "$tailscale_bin" up > "$state/output" 2>&1 < /dev/null &
printf '%s\n' "$!" > "$state/pid"
for attempt in $(seq 1 45); do
    url=$(grep -Eo 'https://login\.tailscale\.com/[A-Za-z0-9._/?=&%:-]+' "$state/output" 2>/dev/null | head -n 1 || true)
    if [ -n "$url" ]; then printf '%s\n' "$url"; exit 0; fi
    status=$(sudo -n "$tailscale_bin" status --json 2>/dev/null || true)
    if printf '%s' "$status" | grep -q '"BackendState"[[:space:]]*:[[:space:]]*"Running"'; then
        printf '%s\n' '__CIAO_TAILSCALE_CONNECTED__'
        exit 0
    fi
    sleep 1
done
cat "$state/output" 2>/dev/null || true
exit 124
"#.to_vec(),
    )
}

pub fn generate_ed25519_key(comment: &str) -> Result<GeneratedSshKey> {
    if comment.is_empty() || comment.contains(['\n', '\r']) {
        return Err(CiaoError::Config("SSH key comment is invalid".to_owned()));
    }
    let temp = tempfile_path("ciao-ssh-key");
    let _output = match run_local_command(
        "ssh-keygen",
        &[
            "-q".to_owned(),
            "-t".to_owned(),
            "ed25519".to_owned(),
            "-N".to_owned(),
            String::new(),
            "-C".to_owned(),
            comment.to_owned(),
            "-f".to_owned(),
            temp.display().to_string(),
        ],
        None,
        "generate CI SSH key",
    ) {
        Ok(output) => output,
        Err(error) => {
            let _ = fs::remove_file(&temp);
            let _ = fs::remove_file(temp.with_extension("pub"));
            return Err(error);
        }
    };
    let private_key = fs::read_to_string(&temp);
    let public_key = fs::read_to_string(temp.with_extension("pub"));
    let cleanup_private = fs::remove_file(&temp);
    let _ = fs::remove_file(temp.with_extension("pub"));
    cleanup_private?;
    let private_key = private_key?;
    let public_key = public_key?;
    Ok(GeneratedSshKey {
        private_key,
        public_key: public_key.trim().to_owned(),
    })
}

/// Return the local path used by the opt-in SSH bootstrap key.
pub fn default_ssh_identity_path(host_name: &str) -> Result<PathBuf> {
    validate_identifier("host name", host_name)?;
    let home = std::env::var_os("HOME").map(PathBuf::from).ok_or_else(|| {
        CiaoError::Config("cannot determine HOME for the SSH identity path".to_owned())
    })?;
    Ok(home
        .join(".ssh")
        .join("ciao")
        .join(format!("{host_name}_ed25519")))
}

#[cfg(test)]
fn ssh_command_arguments_for_test(transport: &OpenSshTransport) -> Vec<String> {
    let command = transport.ssh_command(&CommandSpec::fixed("true", &[], "test"));
    command
        .get_args()
        .map(|value| value.to_string_lossy().into_owned())
        .collect()
}

/// Create or reuse the optional local bootstrap identity and return its
/// public key. Existing Ciao identities are never overwritten.
pub fn ensure_ssh_identity(path: &Path, comment: &str) -> Result<String> {
    validate_identity_file(path)?;
    if comment.is_empty() || comment.contains(['\n', '\r']) {
        return Err(CiaoError::Config("SSH key comment is invalid".to_owned()));
    }
    let public_path = PathBuf::from(format!("{}.pub", path.display()));
    if path.exists() {
        if !path.is_file() {
            return Err(CiaoError::Config(format!(
                "SSH identity path is not a regular file: {}",
                path.display()
            )));
        }
        secure_private_key_permissions(path)?;
        if public_path.exists() {
            let public_key = read_ssh_public_key(&public_path)?;
            let derived = run_local_command(
                "ssh-keygen",
                &["-y".to_owned(), "-f".to_owned(), path.display().to_string()],
                None,
                "verify existing SSH public key",
            )?;
            let derived = validate_ssh_public_key(derived.stdout.trim())?;
            if derived
                != public_key
                    .split_whitespace()
                    .take(2)
                    .collect::<Vec<_>>()
                    .join(" ")
            {
                return Err(CiaoError::Config(format!(
                    "SSH public key does not match its private key: {}",
                    public_path.display()
                )));
            }
            return Ok(public_key);
        }
        let output = run_local_command(
            "ssh-keygen",
            &["-y".to_owned(), "-f".to_owned(), path.display().to_string()],
            None,
            "read existing SSH public key",
        )?;
        let public_key = validate_ssh_public_key(output.stdout.trim())?;
        fs::write(&public_path, format!("{public_key}\n"))?;
        secure_public_key_permissions(&public_path)?;
        return Ok(public_key);
    }
    if public_path.exists() {
        return Err(CiaoError::Config(format!(
            "SSH public key exists without its private key: {}",
            public_path.display()
        )));
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
        secure_directory_permissions(parent)?;
    }
    let result = run_local_command(
        "ssh-keygen",
        &[
            "-q".to_owned(),
            "-t".to_owned(),
            "ed25519".to_owned(),
            "-N".to_owned(),
            String::new(),
            "-C".to_owned(),
            comment.to_owned(),
            "-f".to_owned(),
            path.display().to_string(),
        ],
        None,
        "create SSH identity",
    );
    if let Err(error) = result {
        let _ = fs::remove_file(path);
        let _ = fs::remove_file(&public_path);
        return Err(error);
    }
    secure_private_key_permissions(path)?;
    secure_public_key_permissions(&public_path)?;
    read_ssh_public_key(&public_path)
}

/// Install a public key through one normal interactive OpenSSH session.
/// Passwords are read by OpenSSH from the user's terminal and are never
/// accepted as a Ciao argument, environment variable, or file.
pub fn install_public_key_interactively(
    target: &str,
    public_key: &str,
    identity_file: Option<&Path>,
) -> Result<()> {
    validate_ssh_target(target)?;
    if let Some(path) = identity_file {
        validate_identity_file(path)?;
    }
    let script = install_public_key_script(public_key)?;
    let mut command = Command::new("ssh");
    command
        .arg("-o")
        .arg("ConnectTimeout=10")
        .arg("-o")
        .arg("ServerAliveInterval=15")
        .arg("-o")
        .arg("ServerAliveCountMax=2")
        .args(
            identity_file
                .into_iter()
                .flat_map(|path| ["-i".to_owned(), path.display().to_string()]),
        )
        .arg(target)
        .arg("sh")
        .arg("-s")
        .stdin(Stdio::piped())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    own_process_group(&mut command);
    let mut child = command.spawn().map_err(|error| CiaoError::Transport {
        stage: "install SSH public key".to_owned(),
        message: error.to_string(),
        details: String::new(),
    })?;
    child
        .stdin
        .take()
        .ok_or_else(|| CiaoError::Transport {
            stage: "install SSH public key".to_owned(),
            message: "SSH stdin was not available".to_owned(),
            details: String::new(),
        })?
        .write_all(script.as_bytes())?;
    let status = child.wait()?;
    if status.success() {
        Ok(())
    } else {
        Err(CiaoError::LocalCommand {
            stage: "install SSH public key".to_owned(),
            exit: exit_code(status),
            stdout: String::new(),
            stderr: "OpenSSH could not authenticate or install the public key".to_owned(),
        })
    }
}

fn read_ssh_public_key(path: &Path) -> Result<String> {
    let contents = fs::read_to_string(path)?;
    validate_ssh_public_key(contents.trim())
}

fn validate_ssh_public_key(value: &str) -> Result<String> {
    let mut fields = value.split_whitespace();
    let key_type = fields.next().unwrap_or_default();
    let key_data = fields.next().unwrap_or_default();
    let supported = [
        "ssh-ed25519",
        "ssh-rsa",
        "ecdsa-sha2-nistp256",
        "ecdsa-sha2-nistp384",
        "ecdsa-sha2-nistp521",
        "sk-ssh-ed25519@openssh.com",
        "sk-ecdsa-sha2-nistp256@openssh.com",
    ];
    if value.is_empty()
        || value.contains(['\n', '\r'])
        || value.len() > 16 * 1024
        || !supported.contains(&key_type)
        || key_data.is_empty()
        || fields.any(|field| field.contains(['\n', '\r']))
    {
        return Err(CiaoError::Config("SSH public key is invalid".to_owned()));
    }
    Ok(value.to_owned())
}

fn secure_directory_permissions(path: &Path) -> Result<()> {
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

fn secure_private_key_permissions(path: &Path) -> Result<()> {
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

fn secure_public_key_permissions(path: &Path) -> Result<()> {
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o644))?;
    Ok(())
}

pub fn install_public_key_script(public_key: &str) -> Result<String> {
    let key = validate_ssh_public_key(public_key.trim())?;
    Ok(format!(
        "set -eu\ninstall -d -m 700 \"$HOME/.ssh\"\ntouch \"$HOME/.ssh/authorized_keys\"\nchmod 600 \"$HOME/.ssh/authorized_keys\"\ngrep -Fqx {} \"$HOME/.ssh/authorized_keys\" || printf '%s\\n' {} >> \"$HOME/.ssh/authorized_keys\"\n",
        shell_quote(&key),
        shell_quote(&key)
    ))
}

fn tempfile_path(prefix: &str) -> PathBuf {
    let suffix = release_id();
    std::env::temp_dir().join(format!("{prefix}-{suffix}"))
}

pub fn tailscale_federated_identity_request(
    repository: &GitHubRepository,
) -> Result<serde_json::Value> {
    if repository.repository_id.is_empty() {
        return Err(CiaoError::Config(
            "GitHub repository ID is required to scope Tailscale identity".to_owned(),
        ));
    }
    let description = repository
        .full_name()
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, ' ' | '-' | '_' | '.') {
                character
            } else {
                '-'
            }
        })
        .collect::<String>();
    let subject = if repository.owner_id.is_empty() {
        format!(
            "repo:{}:ref:refs/heads/{}",
            repository.full_name(),
            repository.default_branch
        )
    } else {
        format!(
            "repo:{}@{}/{}@{}:ref:refs/heads/{}",
            repository.owner,
            repository.owner_id,
            repository.repo,
            repository.repository_id,
            repository.default_branch
        )
    };
    Ok(serde_json::json!({
        "keyType": "federated",
        "description": format!("Ciao GitHub CI - {description}"),
        "scopes": ["auth_keys"],
        "tags": ["tag:ciao-ci"],
        "audience": format!("api.tailscale.com/ciao-{}", repository.repository_id),
        "issuer": "https://token.actions.githubusercontent.com",
        "subject": subject,
        "customClaimRules": {
            "repository_id": repository.repository_id,
            "ref": format!("refs/heads/{}", repository.default_branch)
        }
    }))
}

pub fn tailscale_create_federated_identity(
    api_token: &str,
    request: &serde_json::Value,
) -> Result<TailscaleFederatedIdentity> {
    validate_tailscale_token(api_token)?;
    let body = serde_json::to_string(request)
        .map_err(|error| CiaoError::Serialization(error.to_string()))?;
    let output = run_tailscale_curl(
        api_token,
        "POST",
        "https://api.tailscale.com/api/v2/tailnet/-/keys",
        Some("application/json"),
        Some(&body),
        "create Tailscale federated identity",
    )?;
    serde_json::from_str(output.stdout.trim()).map_err(|error| {
        CiaoError::Config(format!(
            "Tailscale returned invalid federated identity metadata: {error}"
        ))
    })
}

pub fn tailscale_delete_federated_identity(api_token: &str, identity_id: &str) -> Result<()> {
    validate_tailscale_token(api_token)?;
    validate_tailscale_id(identity_id)?;
    run_tailscale_curl(
        api_token,
        "DELETE",
        &format!("https://api.tailscale.com/api/v2/tailnet/-/keys/{identity_id}"),
        None,
        None,
        "remove Tailscale federated identity",
    )?;
    Ok(())
}

pub fn tailscale_fetch_policy(api_token: &str) -> Result<String> {
    validate_tailscale_token(api_token)?;
    let output = run_tailscale_curl(
        api_token,
        "GET",
        "https://api.tailscale.com/api/v2/tailnet/-/acl",
        None,
        None,
        "read Tailscale policy",
    )?;
    Ok(output.stdout)
}

pub fn tailscale_validate_policy(api_token: &str, policy: &str) -> Result<String> {
    validate_tailscale_token(api_token)?;
    if policy.trim().is_empty() {
        return Err(CiaoError::Config(
            "Tailscale policy cannot be empty".to_owned(),
        ));
    }
    let output = run_tailscale_curl(
        api_token,
        "POST",
        "https://api.tailscale.com/api/v2/tailnet/-/acl/validate",
        Some("application/hujson"),
        Some(policy),
        "validate Tailscale policy",
    )?;
    Ok(output.stdout)
}

pub fn tailscale_preview_policy(
    api_token: &str,
    policy: &str,
    preview_for: &str,
) -> Result<String> {
    validate_tailscale_token(api_token)?;
    if policy.trim().is_empty() {
        return Err(CiaoError::Config(
            "Tailscale policy cannot be empty".to_owned(),
        ));
    }
    let output = run_tailscale_curl(
        api_token,
        "POST",
        &format!(
            "https://api.tailscale.com/api/v2/tailnet/-/acl/preview?type=ipport&previewFor={}",
            percent_encode_query_component(preview_for)
        ),
        Some("application/hujson"),
        Some(policy),
        "preview Tailscale policy",
    )?;
    Ok(output.stdout)
}

fn percent_encode_query_component(value: &str) -> String {
    value
        .bytes()
        .flat_map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                vec![byte as char]
            }
            byte => format!("%{byte:02X}").chars().collect(),
        })
        .collect()
}

pub fn tailscale_apply_policy(api_token: &str, policy: &str) -> Result<()> {
    validate_tailscale_token(api_token)?;
    if policy.trim().is_empty() {
        return Err(CiaoError::Config(
            "Tailscale policy cannot be empty".to_owned(),
        ));
    }
    run_tailscale_curl(
        api_token,
        "POST",
        "https://api.tailscale.com/api/v2/tailnet/-/acl",
        Some("application/hujson"),
        Some(policy),
        "apply Tailscale policy",
    )?;
    Ok(())
}

fn run_tailscale_curl(
    api_token: &str,
    method: &str,
    url: &str,
    content_type: Option<&str>,
    body: Option<&str>,
    stage: &str,
) -> Result<CommandOutput> {
    validate_tailscale_token(api_token)?;
    let mut config = format!(
        "fail-with-body\nsilent\nshow-error\nuser = \"{}:\"\nrequest = \"{}\"\nurl = \"{}\"\n",
        curl_config_quote(api_token),
        curl_config_quote(method),
        curl_config_quote(url),
    );
    config.push_str("header = \"Accept: application/json\"\n");
    if let Some(content_type) = content_type {
        config.push_str(&format!(
            "header = \"Content-Type: {}\"\n",
            curl_config_quote(content_type)
        ));
    }
    let mut args = vec![
        "--connect-timeout".to_owned(),
        "10".to_owned(),
        "--max-time".to_owned(),
        "60".to_owned(),
        "--config".to_owned(),
        "-".to_owned(),
    ];
    if let Some(body) = body {
        args.push("--data-binary".to_owned());
        args.push(body.to_owned());
    }
    run_local_command("curl", &args, Some(config.as_bytes()), stage)
}

fn curl_config_quote(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\r', "\\r")
        .replace('\n', "\\n")
}

pub fn install_ci_public_key(transport: &OpenSshTransport, public_key: &str) -> Result<()> {
    let user = ssh_login_user(&transport.target).ok_or_else(|| {
        CiaoError::Config(
            "CI SSH key installation requires an explicit user@host SSH target".to_owned(),
        )
    })?;
    let script = install_public_key_script(public_key)?;
    run_as_user_script(
        transport,
        &user,
        "install CI SSH public key",
        script.as_bytes(),
    )
}

pub fn remove_ci_public_key(transport: &OpenSshTransport, public_key: &str) -> Result<()> {
    let user = ssh_login_user(&transport.target).ok_or_else(|| {
        CiaoError::Config("CI SSH key removal requires an explicit user@host SSH target".to_owned())
    })?;
    let key = public_key.trim();
    if key.is_empty() || key.contains(['\n', '\r']) {
        return Err(CiaoError::Config("SSH public key is invalid".to_owned()));
    }
    let script = format!(
        "set -eu\nfile=\"$HOME/.ssh/authorized_keys\"\nif test -f \"$file\"; then tmp=$(mktemp)\ntrap 'rm -f \"$tmp\"' EXIT\ngrep -Fvx -- {} \"$file\" > \"$tmp\" || true\ncat \"$tmp\" > \"$file\"\nchmod 600 \"$file\"\nfi\n",
        shell_quote(key)
    );
    run_as_user_script(
        transport,
        &user,
        "remove CI SSH public key",
        script.as_bytes(),
    )
}

/// Copy the host key already trusted by the user's OpenSSH configuration to
/// the Tailscale name used by GitHub Actions. We intentionally do not use
/// `ssh-keyscan`: it observes an unauthenticated key and would turn a setup
/// convenience into a silent trust-on-first-use downgrade.
pub fn capture_known_hosts(transport: &OpenSshTransport, host: &str) -> Result<String> {
    validate_ssh_target(host)?;
    let source_host = ssh_host_from_target(&transport.target);
    let files = ssh_user_known_hosts_files(&transport.target);
    let mut keys = Vec::new();
    for candidate in [source_host, host] {
        if keys.is_empty() {
            for file in &files {
                for key in ssh_keygen_find(candidate, file)? {
                    if !keys.contains(&key) {
                        keys.push(key);
                    }
                }
            }
        }
        if !keys.is_empty() {
            break;
        }
    }
    if keys.is_empty() {
        return Err(CiaoError::Config(format!(
            "Ciao could not find a trusted OpenSSH host key for `{source_host}`; connect once with `ssh {}` after verifying its fingerprint, then rerun GitHub setup",
            transport.target
        )));
    }
    Ok(format!(
        "{}\n",
        keys.into_iter()
            .map(|key| format!("{host} {key}"))
            .collect::<Vec<_>>()
            .join("\n")
    ))
}

fn ssh_user_known_hosts_files(target: &str) -> Vec<PathBuf> {
    let mut files = Vec::new();
    if let Ok(output) = Command::new("ssh").arg("-G").arg(target).output() {
        if output.status.success() {
            let text = String::from_utf8_lossy(&output.stdout);
            for line in text.lines() {
                let Some(value) = line.strip_prefix("userknownhostsfile ") else {
                    continue;
                };
                for path in value.split_whitespace() {
                    let path = if path == "~" {
                        std::env::var_os("HOME")
                            .map(PathBuf::from)
                            .unwrap_or_else(|| PathBuf::from(path))
                    } else if let Some(path) = path.strip_prefix("~/") {
                        std::env::var_os("HOME")
                            .map(PathBuf::from)
                            .map(|home| home.join(path))
                            .unwrap_or_else(|| PathBuf::from(path))
                    } else if Path::new(path).is_absolute() {
                        PathBuf::from(path)
                    } else if let Some(home) = std::env::var_os("HOME") {
                        PathBuf::from(home).join(path)
                    } else {
                        PathBuf::from(path)
                    };
                    if !files.contains(&path) {
                        files.push(path);
                    }
                }
            }
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        let default = PathBuf::from(home).join(".ssh/known_hosts");
        if !files.contains(&default) {
            files.push(default);
        }
    }
    files
}

fn ssh_keygen_find(host: &str, file: &Path) -> Result<Vec<String>> {
    let output = Command::new("ssh-keygen")
        .arg("-F")
        .arg(host)
        .arg("-f")
        .arg(file)
        .output()
        .map_err(|error| CiaoError::Transport {
            stage: "read trusted SSH host key".to_owned(),
            message: error.to_string(),
            details: String::new(),
        })?;
    if !output.status.success() {
        return Ok(Vec::new());
    }
    Ok(trusted_known_host_lines(&String::from_utf8_lossy(
        &output.stdout,
    )))
}

fn trusted_known_host_lines(found: &str) -> Vec<String> {
    found
        .lines()
        .filter_map(|line| {
            let fields = line.split_whitespace().collect::<Vec<_>>();
            if fields.len() < 3 || fields[0].starts_with('#') || fields[0].starts_with('@') {
                return None;
            }
            let key_type = fields[1];
            let supported = [
                "ssh-ed25519",
                "ssh-rsa",
                "ecdsa-sha2-nistp256",
                "ecdsa-sha2-nistp384",
                "ecdsa-sha2-nistp521",
                "sk-ssh-ed25519@openssh.com",
                "sk-ecdsa-sha2-nistp256@openssh.com",
            ];
            if !supported.contains(&key_type) {
                return None;
            }
            Some(fields[1..].join(" "))
        })
        .collect()
}

fn validate_tailscale_token(token: &str) -> Result<()> {
    if token.trim().is_empty() || token.contains(['\n', '\r']) {
        return Err(CiaoError::Config(
            "Tailscale bootstrap token must be a single non-empty line".to_owned(),
        ));
    }
    Ok(())
}

fn validate_tailscale_id(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
    {
        return Err(CiaoError::InvalidIdentifier {
            field: "Tailscale identity id",
            value: value.to_owned(),
            reason: "must contain only letters, numbers, `_` and `-`",
        });
    }
    Ok(())
}

pub fn tailscale_policy_patch(
    policy: &serde_json::Value,
    target: &str,
) -> Result<serde_json::Value> {
    validate_ssh_target(target)?;
    let mut patched = policy.clone();
    let object = patched
        .as_object_mut()
        .ok_or_else(|| CiaoError::Config("Tailscale policy must be a JSON object".to_owned()))?;
    let grant = serde_json::json!({
        "src": ["tag:ciao-ci"],
        "dst": [target],
        "ip": ["tcp:22"]
    });
    let tag_owners = object
        .entry("tagOwners")
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
        .ok_or_else(|| {
            CiaoError::Config("Tailscale policy tagOwners is not an object".to_owned())
        })?;
    let owners = tag_owners
        .entry("tag:ciao-ci")
        .or_insert_with(|| serde_json::json!(["autogroup:admin"]));
    let owners = owners.as_array_mut().ok_or_else(|| {
        CiaoError::Config("Tailscale policy tag:ciao-ci owners is not an array".to_owned())
    })?;
    if !owners
        .iter()
        .any(|owner| owner.as_str() == Some("autogroup:admin"))
    {
        owners.push(serde_json::json!("autogroup:admin"));
    }
    let grants = object
        .entry("grants")
        .or_insert_with(|| serde_json::Value::Array(Vec::new()))
        .as_array_mut()
        .ok_or_else(|| CiaoError::Config("Tailscale policy grants is not an array".to_owned()))?;
    if !grants.iter().any(|value| value == &grant) {
        grants.push(grant);
    }
    Ok(patched)
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

fn validate_github_segment(field: &'static str, value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 100
        || value.starts_with('-')
        || value.ends_with('-')
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
    let config_path = root.join("ciao.toml");
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
    } else if is_python_project(root) {
        let install = match config
            .build
            .as_ref()
            .and_then(|build| build.install.clone())
        {
            Some(command) => Some(command),
            None => Some(python_install_command(root)?),
        };
        let run = match config.run.as_ref().and_then(|run| run.command.clone()) {
            Some(command) => Some(command),
            None => Some(python_run_command(root)?),
        };
        (
            Runtime::Python,
            AppType::Service,
            install,
            None,
            run,
            config.run.as_ref().and_then(|run| run.port).or(Some(8000)),
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
        if is_astro_project(root)? {
            if astro_is_server_output(root)? {
                (
                    Runtime::Astro,
                    AppType::Service,
                    Some(install.to_owned()),
                    Some(format!("{runner} run build")),
                    Some(format!("{runner} start")),
                    Some(3000),
                    None,
                )
            } else {
                (
                    Runtime::Astro,
                    AppType::Static,
                    Some(install.to_owned()),
                    Some(format!("{runner} run build")),
                    None,
                    None,
                    Some("dist".to_owned()),
                )
            }
        } else {
            (
                Runtime::Node,
                AppType::Service,
                Some(install.to_owned()),
                Some(format!("{runner} run build")),
                Some(format!("{runner} start")),
                Some(3000),
                None,
            )
        }
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
                "no supported project marker found (Cargo.toml, go.mod, package.json, Python files or dist/build/public)".to_owned(),
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
            plan.run_command = None;
            plan.port = None;
            plan.static_directory = ["dist", "build", "public"]
                .into_iter()
                .find(|directory| root.join(directory).is_dir())
                .map(str::to_owned)
                .or_else(|| (plan.runtime == Runtime::Astro).then_some("dist".to_owned()));
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

/// Detect the common two-directory full-stack layout without requiring a
/// project file:
///
/// ```text
/// project/
///   backend/   (Flask or another supported Python service)
///   frontend/  (Next, Astro or another supported Node app)
/// ```
///
/// The deploy engine still receives one `ProjectPlan` at a time. This API
/// keeps component detection explicit and lets callers choose a safe
/// orchestration policy instead of silently combining two processes.
pub fn detect_project_components(root: &Path) -> Result<Vec<ProjectComponent>> {
    let root_name = root_project_name(root)?;
    validate_identifier("app name", &root_name)?;

    let candidates = [
        (
            ProjectRole::Backend,
            &["backend", "api", "server"] as &[&str],
        ),
        (
            ProjectRole::Frontend,
            &["frontend", "web", "client", "ui"] as &[&str],
        ),
    ];
    let mut components = Vec::new();
    for (role, directories) in candidates {
        for directory in directories {
            let path = root.join(directory);
            if !path.is_dir() || !project_marker_exists(&path) {
                continue;
            }
            let mut plan = match detect_project(&path) {
                Ok(plan) => plan,
                Err(_) => continue,
            };
            let configured_name = path.join("ciao.toml").is_file();
            if !configured_name {
                plan.name = format!("{root_name}-{directory}");
                validate_identifier("app name", &plan.name)?;
            }
            components.push(ProjectComponent {
                name: plan.name.clone(),
                role,
                path,
                plan,
            });
            break;
        }
    }
    if components
        .iter()
        .any(|component| component.role == ProjectRole::Backend)
        && components
            .iter()
            .any(|component| component.role == ProjectRole::Frontend)
    {
        Ok(components)
    } else {
        Ok(Vec::new())
    }
}

fn root_project_name(root: &Path) -> Result<String> {
    if root.join("ciao.toml").is_file() {
        let config: ProjectConfig = toml::from_str(&fs::read_to_string(root.join("ciao.toml"))?)
            .map_err(|error| CiaoError::Config(error.to_string()))?;
        if let Some(name) = config.app.and_then(|app| app.name) {
            return Ok(name);
        }
    }
    root.file_name()
        .map(|value| value.to_string_lossy().into_owned())
        .ok_or_else(|| CiaoError::Detection("project has no usable name".to_owned()))
}

fn project_marker_exists(root: &Path) -> bool {
    [
        "Cargo.toml",
        "go.mod",
        "package.json",
        "bun.lock",
        "bun.lockb",
        "requirements.txt",
        "pyproject.toml",
        "Pipfile",
        "setup.py",
        "app.py",
        "main.py",
        "dist",
        "build",
        "public",
    ]
    .into_iter()
    .any(|file| root.join(file).exists())
}

fn is_astro_project(root: &Path) -> Result<bool> {
    let contents = fs::read_to_string(root.join("package.json"))
        .map_err(|error| CiaoError::Detection(format!("cannot read package.json: {error}")))?;
    let package: serde_json::Value = serde_json::from_str(&contents)
        .map_err(|error| CiaoError::Detection(format!("invalid package.json: {error}")))?;
    Ok(["dependencies", "devDependencies", "peerDependencies"]
        .into_iter()
        .filter_map(|section| package.get(section))
        .any(|section| section.get("astro").is_some())
        || [
            "astro.config.mjs",
            "astro.config.js",
            "astro.config.ts",
            "astro.config.cjs",
        ]
        .into_iter()
        .any(|file| root.join(file).is_file()))
}

fn is_python_project(root: &Path) -> bool {
    [
        "requirements.txt",
        "pyproject.toml",
        "Pipfile",
        "setup.py",
        "app.py",
        "main.py",
        "wsgi.py",
        "manage.py",
    ]
    .into_iter()
    .any(|file| root.join(file).is_file())
}

fn python_install_command(root: &Path) -> Result<String> {
    if root.join("requirements.txt").is_file() {
        return Ok(
            "python3 -m venv .venv && .venv/bin/python -m pip install --upgrade pip && .venv/bin/python -m pip install -r requirements.txt".to_owned(),
        );
    }
    if root.join("pyproject.toml").is_file() || root.join("setup.py").is_file() {
        return Ok(
            "python3 -m venv .venv && .venv/bin/python -m pip install --upgrade pip && .venv/bin/python -m pip install .".to_owned(),
        );
    }
    Err(CiaoError::Detection(
        "Python app needs requirements.txt, pyproject.toml or setup.py".to_owned(),
    ))
}

fn python_dependency_declared(root: &Path, dependency: &str) -> bool {
    let mut contents = String::new();
    if let Ok(value) = fs::read_to_string(root.join("requirements.txt")) {
        contents.push_str(&value);
        contents.push('\n');
    }
    if let Ok(value) = fs::read_to_string(root.join("pyproject.toml")) {
        contents.push_str(&value);
    }
    contents.lines().any(|line| {
        let line = line.trim().to_ascii_lowercase();
        let line = line.split('#').next().unwrap_or_default().trim();
        line == dependency
            || line.starts_with(&format!("{dependency}=="))
            || line.starts_with(&format!("{dependency}>"))
            || line.starts_with(&format!("{dependency}<"))
            || line.starts_with(&format!("{dependency}~"))
            || line.starts_with(&format!("{dependency}["))
            || line.contains(&format!("'{dependency}'"))
            || line.contains(&format!("\"{dependency}\""))
    })
}

fn python_entrypoint(root: &Path) -> Option<&'static str> {
    [
        ("app.py", "app"),
        ("main.py", "main"),
        ("wsgi.py", "wsgi"),
        ("run.py", "run"),
    ]
    .into_iter()
    .find(|(file, _)| root.join(file).is_file())
    .map(|(_, module)| module)
}

fn python_run_command(root: &Path) -> Result<String> {
    let module = python_entrypoint(root).ok_or_else(|| {
        CiaoError::Detection(
            "Python app entrypoint not found; add app.py, main.py, wsgi.py or a [run] command to ciao.toml"
                .to_owned(),
        )
    })?;
    if python_dependency_declared(root, "gunicorn") {
        return Ok(format!(
            ".venv/bin/gunicorn --bind \"$HOST:$PORT\" {module}:app"
        ));
    }
    if python_dependency_declared(root, "uvicorn") {
        return Ok(format!(
            ".venv/bin/uvicorn {module}:app --host \"$HOST\" --port \"$PORT\""
        ));
    }
    if python_dependency_declared(root, "flask") {
        return Ok(format!(
            ".venv/bin/python -m flask --app {module} run --host \"$HOST\" --port \"$PORT\""
        ));
    }
    Ok(format!(".venv/bin/python {module}.py"))
}

fn astro_is_server_output(root: &Path) -> Result<bool> {
    for file in [
        "astro.config.mjs",
        "astro.config.js",
        "astro.config.ts",
        "astro.config.cjs",
    ] {
        let path = root.join(file);
        if path.is_file() {
            let contents = fs::read_to_string(path)
                .map_err(|error| CiaoError::Detection(format!("cannot read {file}: {error}")))?;
            let normalized = contents.replace(['\'', '"', '`', ' ', '\n', '\r', '\t'], "");
            if normalized.contains("output:server")
                || normalized.contains("output:hybrid")
                || normalized.contains("output=server")
                || normalized.contains("output=hybrid")
            {
                return Ok(true);
            }
        }
    }
    Ok(false)
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
    pub full_output: bool,
}

impl CommandSpec {
    pub fn fixed(program: impl Into<String>, args: &[&str], stage: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            args: args.iter().map(|arg| (*arg).to_owned()).collect(),
            stdin: None,
            stage: stage.into(),
            full_output: false,
        }
    }

    pub fn with_stdin(mut self, stdin: impl Into<Vec<u8>>) -> Self {
        self.stdin = Some(stdin.into());
        self
    }

    pub fn with_full_output(mut self) -> Self {
        self.full_output = true;
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
        Self::from_output_with_limit(output, false)
    }

    fn from_output_with_limit(output: std::process::Output, full_output: bool) -> Self {
        Self {
            status: output.status.code().unwrap_or(128),
            stdout: if full_output {
                String::from_utf8_lossy(&output.stdout).into_owned()
            } else {
                truncate(&String::from_utf8_lossy(&output.stdout))
            },
            stderr: if full_output {
                String::from_utf8_lossy(&output.stderr).into_owned()
            } else {
                truncate(&String::from_utf8_lossy(&output.stderr))
            },
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
    pub identity_file: Option<PathBuf>,
}

impl OpenSshTransport {
    pub fn new(target: impl Into<String>) -> Result<Self> {
        let target = target.into();
        validate_ssh_target(&target)?;
        Ok(Self {
            target,
            connect_timeout_seconds: 10,
            identity_file: None,
        })
    }

    pub fn with_identity_file(mut self, identity_file: Option<PathBuf>) -> Result<Self> {
        if let Some(path) = &identity_file {
            validate_identity_file(path)?;
        }
        self.identity_file = identity_file;
        Ok(self)
    }

    fn ssh_command(&self, command: &CommandSpec) -> Command {
        let mut process = Command::new("ssh");
        process
            .arg("-T")
            .arg("-o")
            .arg("BatchMode=yes")
            .arg("-o")
            .arg(format!("ConnectTimeout={}", self.connect_timeout_seconds))
            .arg("-o")
            .arg("ServerAliveInterval=15")
            .arg("-o")
            .arg("ServerAliveCountMax=2")
            .args(
                self.identity_file
                    .iter()
                    .flat_map(|_| ["-o".to_owned(), "IdentitiesOnly=yes".to_owned()]),
            )
            .args(
                self.identity_file
                    .iter()
                    .flat_map(|path| ["-i".to_owned(), path.display().to_string()]),
            )
            .arg(&self.target)
            .arg(&command.program);
        for arg in &command.args {
            process.arg(arg);
        }
        own_process_group(&mut process);
        process
    }

    /// Upload a filtered source snapshot to a new, caller-generated directory.
    /// The local tar and remote extractor are separate processes; no shell
    /// pipeline is constructed from user input.
    pub fn upload_tar(&self, source: &Path, destination: &str) -> Result<()> {
        self.upload_tar_with_progress(source, destination, &NoopProgressReporter)
    }

    pub fn upload_tar_with_progress(
        &self,
        source: &Path,
        destination: &str,
        reporter: &dyn ProgressReporter,
    ) -> Result<()> {
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
            .arg("--exclude=./.git")
            .arg("--exclude=.ciao")
            .arg("--exclude=./.ciao")
            .arg("--exclude=target")
            .arg("--exclude=./target")
            .arg("--exclude=./target/*")
            .arg("--exclude=node_modules")
            .arg("--exclude=./node_modules")
            .arg("--exclude=./node_modules/*")
            .arg("--exclude=.env")
            .arg("--exclude=./.env")
            .arg("--exclude=.env.*")
            .arg("--exclude=./.env.*")
            .arg("--exclude=.envrc")
            .arg("--exclude=./.envrc")
            .arg("--exclude=.dev.vars")
            .arg("--exclude=./.dev.vars")
            .arg("--exclude=*.pem")
            .arg("--exclude=./*.pem")
            .arg("--exclude=*.key")
            .arg("--exclude=./*.key")
            .arg("--exclude=.ssh")
            .arg("--exclude=./.ssh")
            .arg("--exclude=._*")
            .arg("--exclude=./._*")
            .arg("--exclude=.DS_Store")
            .arg("--exclude=./.DS_Store")
            .args(
                upload_ignore_patterns(source)
                    .iter()
                    .map(|pattern| format!("--exclude={pattern}")),
            )
            .arg("--directory")
            .arg(source)
            .arg(".");
        own_process_group(&mut tar_command);
        let mut tar = tar_command
            // macOS' BSD tar otherwise emits AppleDouble metadata entries
            // when archiving files with Finder metadata. The env var is
            // harmless on Linux and keeps the remote release platform-neutral.
            .env("COPYFILE_DISABLE", "1")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| CiaoError::Transport {
                stage: "upload".to_owned(),
                message: error.to_string(),
                details: String::new(),
            })?;

        let remote_spec = CommandSpec::fixed(
            "sudo",
            &[
                "-n",
                "tar",
                "--extract",
                "--file=-",
                "--no-same-owner",
                "--no-same-permissions",
                "--directory",
            ],
            "remote source extraction",
        );
        let remote = match self
            .ssh_command(&CommandSpec {
                args: {
                    let mut args = remote_spec.args;
                    args.push(destination.to_owned());
                    args
                },
                ..remote_spec
            })
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(remote) => remote,
            Err(error) => {
                terminate_child(&mut tar);
                return Err(CiaoError::Transport {
                    stage: "upload".to_owned(),
                    message: error.to_string(),
                    details: String::new(),
                });
            }
        };
        let mut children = UploadChildren::new(tar, remote);
        let mut tar_stdout = children
            .tar
            .stdout
            .take()
            .ok_or_else(|| CiaoError::Transport {
                stage: "upload".to_owned(),
                message: "tar stdout was not available".to_owned(),
                details: String::new(),
            })?;
        let mut remote_stdin =
            children
                .remote
                .stdin
                .take()
                .ok_or_else(|| CiaoError::Transport {
                    stage: "upload".to_owned(),
                    message: "SSH stdin was not available".to_owned(),
                    details: String::new(),
                })?;
        let tar_stderr = children
            .tar
            .stderr
            .take()
            .ok_or_else(|| CiaoError::Transport {
                stage: "upload".to_owned(),
                message: "tar stderr was not available".to_owned(),
                details: String::new(),
            })?;
        let remote_stdout = children
            .remote
            .stdout
            .take()
            .ok_or_else(|| CiaoError::Transport {
                stage: "upload".to_owned(),
                message: "SSH stdout was not available".to_owned(),
                details: String::new(),
            })?;
        let remote_stderr = children
            .remote
            .stderr
            .take()
            .ok_or_else(|| CiaoError::Transport {
                stage: "upload".to_owned(),
                message: "SSH stderr was not available".to_owned(),
                details: String::new(),
            })?;
        // Drain every child output while stdin is being copied. Waiting until
        // after the upload can deadlock when tar or ssh fills a pipe buffer.
        let tar_stderr_reader = spawn_pipe_reader(tar_stderr);
        let remote_stdout_reader = spawn_pipe_reader(remote_stdout);
        let remote_stderr_reader = spawn_pipe_reader(remote_stderr);
        reporter.updated("upload source (starting SSH transfer) ");
        let copy_result = copy_upload_stream(&mut tar_stdout, &mut remote_stdin, reporter);
        drop(remote_stdin);
        if copy_result.is_err() {
            // If either side stops consuming the stream, terminate both
            // children before waiting. This also covers a failed remote tar
            // without leaving a local tar/ssh pair behind.
            children.kill_all();
        }
        let tar_status = children.tar.wait();
        if tar_status.is_err() {
            children.remote.kill().ok();
        }
        let remote_status = children.remote.wait();
        if remote_status.is_err() {
            children.tar.kill().ok();
        }
        let tar_stderr = join_pipe_reader(tar_stderr_reader, "tar stderr")?;
        let remote_stdout = join_pipe_reader(remote_stdout_reader, "SSH stdout")?;
        let remote_stderr = join_pipe_reader(remote_stderr_reader, "SSH stderr")?;
        let tar_status = tar_status.map_err(CiaoError::Io)?;
        let remote_status = remote_status.map_err(CiaoError::Io)?;
        children.disarm();
        if !remote_status.success() {
            return Err(CiaoError::RemoteCommand {
                stage: "remote source extraction".to_owned(),
                exit: exit_code(remote_status),
                stdout: truncate(&String::from_utf8_lossy(&remote_stdout)),
                stderr: truncate(&String::from_utf8_lossy(&remote_stderr)),
            });
        }
        let transferred = copy_result.map_err(|error| CiaoError::Transport {
            stage: "upload source over SSH".to_owned(),
            message: error.to_string(),
            details: "the local archive or the remote SSH extractor stopped accepting data"
                .to_owned(),
        })?;
        if !tar_status.success() {
            return Err(CiaoError::RemoteCommand {
                stage: "local source archive".to_owned(),
                exit: exit_code(tar_status),
                stdout: String::new(),
                stderr: truncate(&String::from_utf8_lossy(&tar_stderr)),
            });
        }
        reporter.updated(&format!("upload source ({})", format_bytes(transferred)));
        Ok(())
    }
}

struct UploadChildren {
    tar: Child,
    remote: Child,
    armed: bool,
}

impl UploadChildren {
    fn new(tar: Child, remote: Child) -> Self {
        Self {
            tar,
            remote,
            armed: true,
        }
    }

    fn kill_all(&mut self) {
        terminate_child(&mut self.tar);
        terminate_child(&mut self.remote);
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for UploadChildren {
    fn drop(&mut self) {
        if self.armed {
            self.kill_all();
        }
    }
}

fn terminate_child(child: &mut Child) {
    match child.try_wait() {
        Ok(Some(_)) => {}
        Ok(None) | Err(_) => {
            terminate_process_group(child);
            for _ in 0..10 {
                match child.try_wait() {
                    Ok(Some(_)) => return,
                    Ok(None) => std::thread::sleep(Duration::from_millis(25)),
                    Err(_) => break,
                }
            }
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[cfg(unix)]
fn own_process_group(command: &mut Command) {
    command.process_group(0);
}

#[cfg(not(unix))]
fn own_process_group(_command: &mut Command) {}

#[cfg(unix)]
fn terminate_process_group(child: &Child) {
    let pid = child.id() as libc::pid_t;
    if pid > 0 {
        // Ciao gives every foreground child its own process group. This also
        // stops a shell-launched compiler or dev server when the parent is
        // interrupted, instead of leaving a grandchild behind.
        unsafe {
            let _ = libc::kill(-pid, libc::SIGTERM);
        }
    }
}

#[cfg(not(unix))]
fn terminate_process_group(_child: &Child) {}

/// Wait for a foreground child without turning Ctrl-C into a leaked server.
/// The child normally receives the terminal signal itself and exits through
/// its own cleanup path. If it does not, Ciao gives it a short grace period and
/// then kills and waits for it before returning.
fn wait_child_with_cancellation(child: &mut Child) -> io::Result<Option<ExitStatus>> {
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        if cancellation_requested() {
            for _ in 0..10 {
                if let Some(status) = child.try_wait()? {
                    return Ok(Some(status));
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            terminate_child(child);
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[allow(clippy::io_other_error)]
fn wait_with_output_cancellation(child: &mut Child) -> io::Result<Output> {
    let stdout_reader = child.stdout.take().map(spawn_pipe_reader);
    let stderr_reader = child.stderr.take().map(spawn_pipe_reader);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(error) => {
                terminate_child(child);
                return Err(error);
            }
        }
        if cancellation_requested() {
            let mut exited = None;
            for _ in 0..10 {
                if let Some(status) = child.try_wait()? {
                    exited = Some(status);
                    break;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            if let Some(status) = exited {
                break status;
            }
            if child.try_wait()?.is_none() {
                terminate_child(child);
            }
            break child.try_wait()?.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::Other,
                    "child did not exit after cancellation",
                )
            })?;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    Ok(Output {
        status,
        stdout: join_pipe_reader_io(stdout_reader)?,
        stderr: join_pipe_reader_io(stderr_reader)?,
    })
}

fn spawn_pipe_reader<R>(mut reader: R) -> JoinHandle<io::Result<Vec<u8>>>
where
    R: Read + Send + 'static,
{
    std::thread::spawn(move || {
        let mut output = Vec::new();
        reader.read_to_end(&mut output)?;
        Ok(output)
    })
}

fn join_pipe_reader(reader: JoinHandle<io::Result<Vec<u8>>>, stream: &str) -> Result<Vec<u8>> {
    reader
        .join()
        .map_err(|_| CiaoError::Transport {
            stage: "upload".to_owned(),
            message: format!("{stream} reader thread panicked"),
            details: String::new(),
        })?
        .map_err(|error| CiaoError::Transport {
            stage: "upload".to_owned(),
            message: format!("could not read {stream}: {error}"),
            details: String::new(),
        })
}

#[allow(clippy::io_other_error)]
fn join_pipe_reader_io(reader: Option<JoinHandle<io::Result<Vec<u8>>>>) -> io::Result<Vec<u8>> {
    match reader {
        Some(reader) => reader
            .join()
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "pipe reader thread panicked"))?,
        None => Ok(Vec::new()),
    }
}

fn copy_upload_stream(
    reader: &mut impl Read,
    writer: &mut impl Write,
    reporter: &dyn ProgressReporter,
) -> io::Result<u64> {
    let mut buffer = [0_u8; 64 * 1024];
    let mut transferred = 0_u64;
    let mut last_update = Instant::now();
    reporter.updated("upload source (0 B via SSH)");
    loop {
        if reporter.cancelled() {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "operation interrupted by user",
            ));
        }
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        writer.write_all(&buffer[..read])?;
        transferred += read as u64;
        if last_update.elapsed() >= Duration::from_millis(250) {
            reporter.updated(&format!(
                "upload source ({} via SSH)",
                format_bytes(transferred)
            ));
            last_update = Instant::now();
        }
    }
    Ok(transferred)
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} {}", bytes, UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn ignore_patterns(source: &Path) -> Vec<String> {
    [".gitignore", ".ciaoignore"]
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

fn upload_ignore_patterns(source: &Path) -> Vec<String> {
    ignore_patterns(source)
        .into_iter()
        .flat_map(|pattern| {
            if pattern.starts_with("./") {
                vec![pattern]
            } else {
                vec![pattern.clone(), format!("./{pattern}")]
            }
        })
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
        process.stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = process.spawn().map_err(|error| CiaoError::Transport {
            stage: command.stage.clone(),
            message: error.to_string(),
            details: String::new(),
        })?;
        if let Some(stdin) = command.stdin {
            let mut child_stdin = match child.stdin.take() {
                Some(stdin) => stdin,
                None => {
                    terminate_child(&mut child);
                    return Err(CiaoError::Transport {
                        stage: command.stage.clone(),
                        message: "SSH stdin was not available".to_owned(),
                        details: String::new(),
                    });
                }
            };
            if let Err(error) = child_stdin.write_all(&stdin) {
                terminate_child(&mut child);
                return Err(CiaoError::Io(error));
            }
        }
        let output = child.wait_with_output()?;
        let result = CommandOutput::from_output_with_limit(output, command.full_output);
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

/// Check whether the SSH user can run the administrative commands used by
/// Ciao without prompting. This is deliberately separate from host
/// initialization: `host init` has a dedicated one-session TTY path, while
/// deploy/lifecycle operations use several independent SSH commands.
pub fn check_remote_sudo(transport: &OpenSshTransport) -> Result<()> {
    transport
        .exec(CommandSpec::fixed(
            "env",
            &["LC_ALL=C", "sudo", "-n", "true"],
            "check remote administrator privileges",
        ))
        .map(|_| ())
}

/// Human-readable, one-time remediation for hosts whose SSH user can use
/// sudo interactively but not from the independent sessions used by Ciao.
/// This is deliberately guidance only: the actual policy change is a separate
/// interactive operation that requires explicit user confirmation.
pub fn passwordless_sudo_instructions(transport: &OpenSshTransport) -> String {
    let user = ssh_login_user(&transport.target).unwrap_or_else(|| "<ssh-user>".to_owned());
    format!(
        "Simple one-time fix on the target host ({target}):\n\
  ssh {target}\n\
  sudo visudo\n\
\nAdd this line in the editor:\n\
  {user} ALL=(ALL) NOPASSWD: ALL\n\
\nSave and exit the editor, then validate the policy:\n\
  sudo visudo -c\n\
  exit\n\
\nBack on this computer, retry the same Ciao command.\n\
Ciao never reads or stores the password. The automatic setup changes sudoers\n\
only after your explicit confirmation. This simple policy grants the SSH account\n\
full passwordless administrator access; use a narrower policy if your environment\n\
requires least privilege.",
        target = transport.target,
        user = user,
    )
}

/// Read-only check used by the normal deploy path. It deliberately does not
/// use sudo: a host that needs bootstrap must still be able to reach the
/// interactive one-session initializer before Ciao asks for a password.
pub fn host_needs_initialization(transport: &OpenSshTransport) -> Result<bool> {
    let platform = transport.inspect()?;
    validate_host_init_platform(&platform)?;
    let output = remote_script(
        transport,
        "check host readiness",
        r#"set -eu
missing=0
for command in sudo tar curl; do
    command -v "$command" >/dev/null 2>&1 || missing=1
done
case "$(uname -s)" in
Linux)
    command -v apt-get >/dev/null 2>&1 || missing=1
    command -v gpg >/dev/null 2>&1 || missing=1
    command -v caddy >/dev/null 2>&1 || missing=1
    test -f /etc/caddy/Caddyfile || missing=1
    grep -Fqx 'import /etc/caddy/ciao/*.caddy' /etc/caddy/Caddyfile 2>/dev/null || missing=1
    ;;
Darwin)
    brew_ready=0
    for candidate in /opt/homebrew/bin/brew /usr/local/bin/brew /home/linuxbrew/.linuxbrew/bin/brew; do
        if [ -x "$candidate" ]; then brew_ready=1; break; fi
    done
    [ "$brew_ready" -eq 1 ] || missing=1
    command -v caddy >/dev/null 2>&1 || missing=1
    ;;
*) missing=1 ;;
esac
printf 'missing=%s\n' "$missing"
"#,
    )?;
    Ok(output
        .stdout
        .lines()
        .find_map(|line| line.strip_prefix("missing=")?.parse::<u8>().ok())
        .unwrap_or(1)
        != 0)
}

/// Run host readiness and, when needed, the one-session interactive bootstrap
/// before a normal terminal deployment. JSON, CI and MCP deliberately use the
/// non-interactive deploy path in the CLI instead.
pub fn prepare_host_for_deploy(
    transport: &OpenSshTransport,
    reporter: &dyn ProgressReporter,
) -> Result<()> {
    let needs_initialization = progress_step(reporter, "check host dependencies", || {
        host_needs_initialization(transport)
    })?;
    if needs_initialization {
        check_remote_sudo(transport).map_err(|error| {
            if remote_sudo_password_required(&error) {
                CiaoError::Config(format!(
                    "host dependencies are missing and this operation has no interactive terminal.\n\n{}",
                    passwordless_sudo_instructions(transport)
                ))
            } else {
                error
            }
        })?;
        progress_step(reporter, "initialize host dependencies", || {
            init_host(transport).map(|_| ())
        })?;
    }
    match check_remote_sudo(transport) {
        Ok(()) => Ok(()),
        Err(error) if remote_sudo_password_required(&error) => Err(CiaoError::Config(
            format!(
                "host preparation completed, but deployment needs passwordless sudo (`sudo -n`) across multiple SSH sessions.\n\n{}",
                passwordless_sudo_instructions(transport)
            ),
        )),
        Err(error) => Err(error),
    }
}

/// Interactive counterpart used only by the human-facing terminal command.
/// The password remains inside OpenSSH/sudo; it is never passed through this
/// API or retained by Ciao.
pub fn prepare_host_for_deploy_interactive(
    transport: &OpenSshTransport,
    reporter: &dyn ProgressReporter,
) -> Result<()> {
    let needs_initialization = progress_step(reporter, "check host dependencies", || {
        host_needs_initialization(transport)
    })?;
    if needs_initialization {
        match check_remote_sudo(transport) {
            Ok(()) => progress_step(reporter, "initialize host dependencies", || {
                init_host(transport).map(|_| ())
            })?,
            Err(error) if remote_sudo_password_required(&error) => {
                progress_step(reporter, "initialize host dependencies", || {
                    init_host_interactively(transport).map(|_| ())
                })?
            }
            Err(error) => return Err(error),
        }
    }
    match check_remote_sudo(transport) {
        Ok(()) => Ok(()),
        Err(error) if remote_sudo_password_required(&error) => Err(CiaoError::Config(
            format!(
                "host preparation completed, but deployment needs passwordless sudo (`sudo -n`) across multiple SSH sessions.\n\n{}",
                passwordless_sudo_instructions(transport)
            ),
        )),
        Err(error) => Err(error),
    }
}

fn interactive_ssh_command(transport: &OpenSshTransport) -> Command {
    let mut process = Command::new("ssh");
    process
        .arg("-tt")
        .arg("-o")
        .arg(format!(
            "ConnectTimeout={}",
            transport.connect_timeout_seconds
        ))
        .arg("-o")
        .arg("ServerAliveInterval=15")
        .arg("-o")
        .arg("ServerAliveCountMax=2")
        .args(
            transport
                .identity_file
                .iter()
                .flat_map(|_| ["-o".to_owned(), "IdentitiesOnly=yes".to_owned()]),
        )
        .args(
            transport
                .identity_file
                .iter()
                .flat_map(|path| ["-i".to_owned(), path.display().to_string()]),
        )
        .arg(&transport.target);
    own_process_group(&mut process);
    process
}

fn run_interactive_ssh_script(
    transport: &OpenSshTransport,
    stage: &str,
    script: &str,
) -> Result<()> {
    // OpenSSH joins remote command arguments and lets the login shell parse
    // them. The script is generated by Ciao from fixed templates; quoting it
    // as one argument keeps shell syntax inside the `sh -c` payload. Running
    // the script as the SSH user is important on macOS: Homebrew refuses to
    // install as root. `sudo -v` and every following `sudo -n` use this same
    // SSH TTY, so one native password prompt is enough.
    let mut process = interactive_ssh_command(transport);
    let quiet_script = format!(
        "log=$(mktemp /tmp/ciao-host-init.XXXXXX)\ntrap 'rm -f \"$log\"' EXIT\n{{\n{script}\n}} >\"$log\" 2>&1 || {{ status=$?; cat \"$log\" >&2; exit \"$status\"; }}\n"
    );
    process
        .arg("sh")
        .arg("-c")
        .arg(shell_quote(&format!("sudo -v\n{quiet_script}")))
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    let status = process.status().map_err(|error| CiaoError::Transport {
        stage: stage.to_owned(),
        message: error.to_string(),
        details: String::new(),
    })?;
    if status.success() {
        Ok(())
    } else {
        Err(CiaoError::LocalCommand {
            stage: stage.to_owned(),
            exit: exit_code(status),
            stdout: String::new(),
            stderr: "interactive SSH/sudo command was not completed".to_owned(),
        })
    }
}

/// Identify the only remote-sudo failure for which an interactive password
/// prompt is appropriate. Other failures (missing sudo, policy denial, SSH
/// errors) should remain actionable errors rather than triggering a prompt.
pub fn remote_sudo_password_required(error: &CiaoError) -> bool {
    match error {
        CiaoError::RemoteCommand {
            stage,
            stdout,
            stderr,
            ..
        } if stage == "check remote administrator privileges" => {
            let output = format!("{stdout}\n{stderr}").to_ascii_lowercase();
            output.contains("sudo") && output.contains("password")
        }
        _ => false,
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

fn command_script_with_home(
    command: &str,
    cwd: &str,
    home: &str,
    env_file: &str,
) -> Result<Vec<u8>> {
    if home.is_empty()
        || !home.starts_with('/')
        || home.contains(['\n', '\r'])
        || env_file.is_empty()
        || !env_file.starts_with('/')
        || env_file.contains(['\n', '\r'])
    {
        return Err(CiaoError::Config(
            "build home or environment directory is invalid".to_owned(),
        ));
    }
    let command = command_script(command, cwd)?;
    let command = String::from_utf8(command)
        .map_err(|_| CiaoError::Config("application command is not valid UTF-8".to_owned()))?;
    Ok(format!(
        "set -eu\nexport HOME={}\nexport npm_config_cache=\"$HOME/.npm\"\nif test -f {}; then set -a; . {}; set +a; fi\n{command}",
        shell_quote(home),
        shell_quote(env_file),
        shell_quote(env_file),
    )
    .into_bytes())
}

pub fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn env_file_line(key: &str, value: &str) -> String {
    let escaped = value
        .chars()
        .flat_map(|character| {
            if matches!(character, '\\' | '"' | '$' | '`') {
                vec!['\\', character]
            } else {
                vec![character]
            }
        })
        .collect::<String>();
    format!(r#"{key}="{escaped}""#)
}

pub fn run_local_script(script: &[u8]) -> Result<CommandOutput> {
    let mut command = Command::new("sh");
    command
        .arg("-s")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    own_process_group(&mut command);
    let mut child = command.spawn()?;
    if let Err(error) = child.stdin.take().expect("piped stdin").write_all(script) {
        terminate_child(&mut child);
        return Err(error.into());
    }
    let output = wait_with_output_cancellation(&mut child)?;
    Ok(CommandOutput::from_output(output))
}

fn run_local_interactive_script(script: &str) -> Result<CommandOutput> {
    let mut command = Command::new("sh");
    command
        .arg("-c")
        .arg(script)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    own_process_group(&mut command);
    let mut child = command.spawn()?;
    let status = match wait_child_with_cancellation(&mut child)? {
        Some(status) => status,
        None => {
            return Ok(CommandOutput {
                status: 130,
                stdout: String::new(),
                stderr: String::new(),
            })
        }
    };
    Ok(CommandOutput {
        status: status.code().unwrap_or(128),
        stdout: String::new(),
        stderr: String::new(),
    })
}

fn local_sudo_is_cached() -> bool {
    let mut command = Command::new("sudo");
    command
        .args(["-n", "true"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command.status().is_ok_and(|status| status.success())
}

fn local_privileged_script_with_sudo(script: &str, sudo_prefix: &str) -> String {
    format!(
        "set -eu\nsudo -v </dev/tty >/dev/tty 2>/dev/tty\n{}",
        script.replace("sudo -n", sudo_prefix)
    )
}

fn run_local_privileged_script(script: &str) -> Result<CommandOutput> {
    let sudo_prefix = if local_sudo_is_cached() {
        "sudo -n"
    } else {
        "sudo"
    };
    run_local_interactive_script(&local_privileged_script_with_sudo(script, sudo_prefix))
}

fn run_local_interactive_capture(script: &str) -> Result<CommandOutput> {
    let mut command = Command::new("sh");
    command
        .arg("-c")
        .arg(script)
        .stdin(Stdio::inherit())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    own_process_group(&mut command);
    let mut child = command.spawn()?;
    let output = wait_with_output_cancellation(&mut child)?;
    Ok(CommandOutput::from_output(output))
}

fn run_local_command(
    program: &str,
    args: &[String],
    stdin: Option<&[u8]>,
    stage: &str,
) -> Result<CommandOutput> {
    run_local_command_with_output_limit(program, args, stdin, stage, false)
}

fn run_local_command_full(
    program: &str,
    args: &[String],
    stdin: Option<&[u8]>,
    stage: &str,
) -> Result<CommandOutput> {
    run_local_command_with_output_limit(program, args, stdin, stage, true)
}

fn run_local_command_with_output_limit(
    program: &str,
    args: &[String],
    stdin: Option<&[u8]>,
    stage: &str,
    full_output: bool,
) -> Result<CommandOutput> {
    let mut command = Command::new(program);
    command.args(args);
    if stdin.is_some() {
        command.stdin(Stdio::piped());
    }
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    own_process_group(&mut command);
    let mut child = command.spawn()?;
    if let Some(stdin) = stdin {
        if let Err(error) = child.stdin.take().expect("piped stdin").write_all(stdin) {
            terminate_child(&mut child);
            return Err(error.into());
        }
    }
    let output = wait_with_output_cancellation(&mut child)?;
    let result = CommandOutput::from_output_with_limit(output, full_output);
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
        let caddy_bin = [prefix.join("bin/caddy"), prefix.join("opt/caddy/bin/caddy")]
            .into_iter()
            .find(|path| path.is_file())
            .unwrap_or_else(|| prefix.join("bin/caddy"));
        Ok(LocalProxyPaths {
            caddy_bin,
            caddyfile: prefix.join("etc/Caddyfile"),
            fragment_dir: prefix.join("etc/ciao"),
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
            fragment_dir: PathBuf::from("/var/lib/ciao/local"),
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
    brew_install_script=$(mktemp -t ciao-homebrew)
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
loopback_plist='/Library/LaunchDaemons/dev.ciao.local-loopback.plist'
sudo -n tee "$loopback_plist" >/dev/null <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>Label</key><string>dev.ciao.local-loopback</string>
<key>ProgramArguments</key><array><string>/sbin/ifconfig</string><string>lo0</string><string>alias</string><string>10.0.0.1</string></array>
<key>RunAtLoad</key><true/>
</dict></plist>
PLIST
sudo -n chown root:wheel "$loopback_plist"
sudo -n chmod 0644 "$loopback_plist"
sudo -n launchctl bootout system/dev.ciao.local-loopback >/dev/null 2>&1 || true
sudo -n launchctl bootstrap system "$loopback_plist"
if ! ifconfig lo0 2>/dev/null | grep -q '10.0.0.1'; then sudo -n ifconfig lo0 alias 10.0.0.1; fi
dnsmasq_conf="$brew_prefix/etc/dnsmasq.conf"
sudo -n install -d -m 0755 "$brew_prefix/etc"
dnsmasq_dir="$brew_prefix/etc/dnsmasq.d"
sudo -n install -d -m 0755 "$dnsmasq_dir"
if ! sudo -n test -f "$dnsmasq_conf"; then
    printf '%s\n' '# Ciao .ciao resolver' 'listen-address=10.0.0.1' 'bind-interfaces' 'port=53' 'address=/.ciao/127.0.0.1' "conf-dir=$dnsmasq_dir,*.conf" | sudo -n tee "$dnsmasq_conf" >/dev/null
elif ! sudo -n grep -Fq '# Ciao .ciao resolver' "$dnsmasq_conf"; then
    printf '\n%s\n' '# Ciao .ciao resolver' 'listen-address=10.0.0.1' 'bind-interfaces' 'port=53' 'address=/.ciao/127.0.0.1' | sudo -n tee -a "$dnsmasq_conf" >/dev/null
fi
if ! sudo -n grep -Fq "conf-dir=$dnsmasq_dir,*.conf" "$dnsmasq_conf"; then printf '%s\n' "conf-dir=$dnsmasq_dir,*.conf" | sudo -n tee -a "$dnsmasq_conf" >/dev/null; fi
sudo -n install -d -m 0755 /etc/resolver
printf '%s\n' 'nameserver 10.0.0.1' 'port 53' | sudo -n tee /etc/resolver/ciao >/dev/null
fragment_dir="$brew_prefix/etc/ciao"
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
printf 'ciao_local_setup=ready\n'
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
printf '%s\n' '# Ciao .ciao resolver' 'listen-address=127.0.0.1' 'bind-interfaces' 'port=53' 'address=/.ciao/127.0.0.1' | sudo -n tee /etc/dnsmasq.d/ciao-ciao.conf >/dev/null
sudo -n systemctl enable --now dnsmasq
if ! command -v resolvectl >/dev/null 2>&1; then
    echo 'systemd-resolved/resolvectl is required for automatic .ciao DNS routing' >&2
    exit 1
fi
sudo -n install -d -m 0755 /etc/systemd/resolved.conf.d
printf '%s\n' '[Resolve]' 'DNS=127.0.0.1' 'Domains=~ciao' | sudo -n tee /etc/systemd/resolved.conf.d/ciao-ciao.conf >/dev/null
sudo -n systemctl enable --now systemd-resolved
sudo -n systemctl reload-or-restart systemd-resolved
sudo -n install -d -m 0755 /var/lib/ciao/local
sudo -n chown "$(id -un)" /var/lib/ciao/local
import_line='import /var/lib/ciao/local/*.caddy'
if ! sudo -n test -f /etc/caddy/Caddyfile; then
    printf '%s\n' "$import_line" | sudo -n tee /etc/caddy/Caddyfile >/dev/null
elif ! sudo -n grep -Fqx "$import_line" /etc/caddy/Caddyfile; then
    printf '\n%s\n' "$import_line" | sudo -n tee -a /etc/caddy/Caddyfile >/dev/null
fi
sudo -n caddy validate --config /etc/caddy/Caddyfile
sudo -n systemctl enable --now caddy
sudo -n systemctl reload caddy
printf 'ciao_local_setup=ready\n'
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
    let output = run_local_privileged_script(&local_setup_script()?)?;
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

/// Install only the `.ciao` resolver. Remote deployments use this path to
/// map `app.ciao` directly to the host's Tailscale address. It deliberately
/// does not install or start Caddy on this computer.
pub fn local_resolver_setup_script() -> Result<String> {
    if cfg!(target_os = "macos") {
        Ok(r#"set -eu
command -v curl >/dev/null 2>&1 || { echo 'macOS curl is required' >&2; exit 1; }
brew_bin=''
for candidate in /opt/homebrew/bin/brew /usr/local/bin/brew /home/linuxbrew/.linuxbrew/bin/brew; do
    if [ -x "$candidate" ]; then brew_bin="$candidate"; break; fi
done
if [ -z "$brew_bin" ] && command -v brew >/dev/null 2>&1; then brew_bin=$(command -v brew); fi
if [ -z "$brew_bin" ]; then
    brew_install_script=$(mktemp -t ciao-homebrew)
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
if ! "$brew_bin" list --formula dnsmasq >/dev/null 2>&1; then "$brew_bin" install dnsmasq; fi
loopback_plist='/Library/LaunchDaemons/dev.ciao.local-loopback.plist'
sudo -n tee "$loopback_plist" >/dev/null <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>Label</key><string>dev.ciao.local-loopback</string>
<key>ProgramArguments</key><array><string>/sbin/ifconfig</string><string>lo0</string><string>alias</string><string>10.0.0.1</string></array>
<key>RunAtLoad</key><true/>
</dict></plist>
PLIST
sudo -n chown root:wheel "$loopback_plist"
sudo -n chmod 0644 "$loopback_plist"
sudo -n launchctl bootout system/dev.ciao.local-loopback >/dev/null 2>&1 || true
sudo -n launchctl bootstrap system "$loopback_plist"
if ! ifconfig lo0 2>/dev/null | grep -q '10.0.0.1'; then sudo -n ifconfig lo0 alias 10.0.0.1; fi
dnsmasq_conf="$brew_prefix/etc/dnsmasq.conf"
dnsmasq_dir="$brew_prefix/etc/dnsmasq.d"
sudo -n install -d -m 0755 "$brew_prefix/etc" "$dnsmasq_dir"
if ! sudo -n test -f "$dnsmasq_conf"; then
    printf '%s\n' '# Ciao .ciao resolver' 'listen-address=10.0.0.1' 'bind-interfaces' 'port=53' 'address=/.ciao/127.0.0.1' "conf-dir=$dnsmasq_dir,*.conf" | sudo -n tee "$dnsmasq_conf" >/dev/null
else
    if ! sudo -n grep -Fq '# Ciao .ciao resolver' "$dnsmasq_conf"; then printf '\n%s\n' '# Ciao .ciao resolver' 'listen-address=10.0.0.1' 'bind-interfaces' 'port=53' 'address=/.ciao/127.0.0.1' | sudo -n tee -a "$dnsmasq_conf" >/dev/null; fi
    if ! sudo -n grep -Fq "conf-dir=$dnsmasq_dir,*.conf" "$dnsmasq_conf"; then printf '%s\n' "conf-dir=$dnsmasq_dir,*.conf" | sudo -n tee -a "$dnsmasq_conf" >/dev/null; fi
fi
sudo -n install -d -m 0755 /etc/resolver
printf '%s\n' 'nameserver 10.0.0.1' 'port 53' | sudo -n tee /etc/resolver/ciao >/dev/null
sudo -n "$brew_bin" services restart dnsmasq >/dev/null
printf 'ciao_local_resolver=ready\n'
"#.to_owned())
    } else if cfg!(target_os = "linux") {
        Ok(r#"set -eu
command -v apt-get >/dev/null 2>&1 || { echo 'automatic Linux local setup requires apt-get' >&2; exit 1; }
sudo -n env DEBIAN_FRONTEND=noninteractive apt-get update
sudo -n env DEBIAN_FRONTEND=noninteractive apt-get install -y ca-certificates dnsmasq iproute2 systemd-resolved
sudo -n install -d -m 0755 /etc/dnsmasq.d
printf '%s\n' '# Ciao .ciao resolver' 'listen-address=127.0.0.1' 'bind-interfaces' 'port=53' 'address=/.ciao/127.0.0.1' | sudo -n tee /etc/dnsmasq.d/ciao-ciao.conf >/dev/null
sudo -n systemctl enable --now dnsmasq
if ! command -v resolvectl >/dev/null 2>&1; then
    echo 'systemd-resolved/resolvectl is required for automatic .ciao DNS routing' >&2
    exit 1
fi
sudo -n install -d -m 0755 /etc/systemd/resolved.conf.d
printf '%s\n' '[Resolve]' 'DNS=127.0.0.1' 'Domains=~ciao' | sudo -n tee /etc/systemd/resolved.conf.d/ciao-ciao.conf >/dev/null
sudo -n systemctl enable --now systemd-resolved
sudo -n systemctl reload-or-restart systemd-resolved
printf 'ciao_local_resolver=ready\n'
"#.to_owned())
    } else {
        Err(CiaoError::Config(
            "local .ciao DNS routing is supported on macOS and Linux".to_owned(),
        ))
    }
}

fn local_resolver_ready() -> bool {
    if cfg!(target_os = "macos") {
        Path::new("/etc/resolver/ciao").is_file()
            && Command::new("pgrep")
                .args(["-x", "dnsmasq"])
                .status()
                .is_ok_and(|status| status.success())
    } else if cfg!(target_os = "linux") {
        Path::new("/etc/dnsmasq.d/ciao-ciao.conf").is_file()
            && Path::new("/etc/systemd/resolved.conf.d/ciao-ciao.conf").is_file()
            && Command::new("systemctl")
                .args(["is-active", "--quiet", "dnsmasq"])
                .status()
                .is_ok_and(|status| status.success())
    } else {
        false
    }
}

fn local_remote_dns_path() -> Result<PathBuf> {
    if cfg!(target_os = "macos") {
        Ok(local_brew_prefix().join("etc/dnsmasq.d/ciao-remote.conf"))
    } else if cfg!(target_os = "linux") {
        Ok(PathBuf::from("/etc/dnsmasq.d/ciao-remote.conf"))
    } else {
        Err(CiaoError::Config(
            "remote .ciao routing is supported on macOS and Linux".to_owned(),
        ))
    }
}

pub fn local_resolver_setup() -> Result<LocalResolverResult> {
    if !local_resolver_ready() {
        let output = run_local_privileged_script(&local_resolver_setup_script()?)?;
        if output.status != 0 {
            return Err(CiaoError::LocalCommand {
                stage: "configure local .ciao resolver".to_owned(),
                exit: output.status,
                stdout: output.stdout,
                stderr: output.stderr,
            });
        }
    }
    Ok(LocalResolverResult {
        resolver: "*.ciao uses the local resolver; deployed apps use Tailscale addresses"
            .to_owned(),
        dependencies: if cfg!(target_os = "macos") {
            vec![
                "Homebrew (installed if missing)".to_owned(),
                "dnsmasq".to_owned(),
            ]
        } else {
            vec!["dnsmasq".to_owned(), "systemd-resolved".to_owned()]
        },
        message: "local .ciao resolver is ready".to_owned(),
    })
}

pub fn configure_local_remote_domain(name: &str, address: &str) -> Result<LocalResolverResult> {
    validate_local_name(name)?;
    let address = address.trim();
    let parsed = address.parse::<IpAddr>().map_err(|_| {
        CiaoError::Config(
            "the Tailscale target has no IPv4 address; Ciao cannot create the local .ciao DNS route"
                .to_owned(),
        )
    })?;
    if !parsed.is_ipv4() {
        return Err(CiaoError::Config(
            "the local .ciao DNS route requires a Tailscale IPv4 address".to_owned(),
        ));
    }
    let setup = local_resolver_setup()?;
    let path = local_remote_dns_path()?;
    let parent = path
        .parent()
        .ok_or_else(|| CiaoError::Config("local dnsmasq path has no parent".to_owned()))?;
    let domain = local_domain(name)?;
    let line = format!("address=/{domain}/{address}");
    let script = format!(
        "set -eu\nsudo -n install -d -m 0755 {}\ntmp=$(mktemp)\ntrap 'rm -f \"$tmp\"' EXIT\nif sudo -n test -f {}; then sudo -n grep -Fv -- {} {} >\"$tmp\" || true; fi\nprintf '%s\\n' {} | sudo -n tee -a \"$tmp\" >/dev/null\nsudo -n install -m 0644 \"$tmp\" {}\n",
        shell_quote(&parent.to_string_lossy()),
        shell_quote(&path.to_string_lossy()),
        shell_quote(&line),
        shell_quote(&path.to_string_lossy()),
        shell_quote(&line),
        shell_quote(&path.to_string_lossy()),
    );
    let output = run_local_privileged_script(&script)?;
    if output.status != 0 {
        return Err(CiaoError::LocalCommand {
            stage: "configure local .ciao route".to_owned(),
            exit: output.status,
            stdout: output.stdout,
            stderr: output.stderr,
        });
    }
    let reload = if cfg!(target_os = "macos") {
        format!(
            "set -eu\nbrew_bin={}\nsudo -n \"$brew_bin\" services restart dnsmasq >/dev/null\n",
            shell_quote(&local_brew_prefix().join("bin/brew").to_string_lossy())
        )
    } else {
        "set -eu\nsudo -n systemctl restart dnsmasq\n".to_owned()
    };
    let output = run_local_privileged_script(&reload)?;
    if output.status != 0 {
        return Err(CiaoError::LocalCommand {
            stage: "reload local .ciao resolver".to_owned(),
            exit: output.status,
            stdout: output.stdout,
            stderr: output.stderr,
        });
    }
    Ok(setup)
}

pub fn remove_local_remote_domain(name: &str) -> Result<()> {
    validate_local_name(name)?;
    let path = local_remote_dns_path()?;
    if !path.exists() {
        return Ok(());
    }
    let domain = local_domain(name)?;
    let line_prefix = format!("address=/{domain}/");
    let script = format!(
        "set -eu\ntmp=$(mktemp)\ntrap 'rm -f \"$tmp\"' EXIT\nif sudo -n test -f {}; then sudo -n grep -Fv -- {} {} >\"$tmp\" || true; sudo -n install -m 0644 \"$tmp\" {}; fi\n",
        shell_quote(&path.to_string_lossy()),
        shell_quote(&line_prefix),
        shell_quote(&path.to_string_lossy()),
        shell_quote(&path.to_string_lossy()),
    );
    let output = run_local_privileged_script(&script)?;
    if output.status != 0 {
        return Err(CiaoError::LocalCommand {
            stage: "remove local .ciao route".to_owned(),
            exit: output.status,
            stdout: output.stdout,
            stderr: output.stderr,
        });
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloudflareTunnelResult {
    pub app: String,
    pub domain: String,
    pub tunnel: String,
    pub message: String,
}

/// Configure one standard, locally-managed Cloudflare Tunnel.
pub fn cloudflare_tunnel_setup(
    transport: &OpenSshTransport,
    os: &HostOs,
    app: &str,
    domain: &str,
) -> Result<CloudflareTunnelResult> {
    validate_identifier("app name", app)?;
    validate_domain(domain)?;
    let cloudflared = ensure_local_cloudflared()?;
    let home = std::env::var_os("HOME").map(PathBuf::from).ok_or_else(|| {
        CiaoError::Config("HOME is not set; Ciao cannot find cloudflared credentials".to_owned())
    })?;
    let cloudflared_dir = home.join(".cloudflared");
    let cert = cloudflared_dir.join("cert.pem");
    if !cert.is_file() {
        let output = run_local_interactive_script(&format!(
            "set -eu\n{} tunnel login\n",
            shell_quote(&cloudflared)
        ))?;
        if output.status != 0 {
            return Err(CiaoError::LocalCommand {
                stage: "sign in to Cloudflare".to_owned(),
                exit: output.status,
                stdout: output.stdout,
                stderr: output.stderr,
            });
        }
    }
    if !cert.is_file() {
        return Err(CiaoError::Config(
            "Cloudflare login completed without ~/.cloudflared/cert.pem; rerun `cloudflared tunnel login` and retry".to_owned(),
        ));
    }
    let tunnel_name = format!("ciao-{app}");
    let tunnel_id = find_or_create_cloudflare_tunnel(&cloudflared, &tunnel_name)?;
    let route = Command::new(&cloudflared)
        .args(["tunnel", "route", "dns", tunnel_name.as_str(), domain])
        .output()
        .map_err(|error| CiaoError::LocalCommand {
            stage: "create Cloudflare DNS route".to_owned(),
            exit: 1,
            stdout: String::new(),
            stderr: error.to_string(),
        })?;
    if !route.status.success() {
        let stdout = String::from_utf8_lossy(&route.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&route.stderr).into_owned();
        let combined = format!("{stdout}\n{stderr}").to_ascii_lowercase();
        if !combined.contains("already exists") && !combined.contains("already configured") {
            return Err(CiaoError::LocalCommand {
                stage: "create Cloudflare DNS route".to_owned(),
                exit: route.status.code().unwrap_or(1),
                stdout,
                stderr,
            });
        }
    }
    let credential_path = cloudflared_dir.join(format!("{tunnel_id}.json"));
    let credentials = fs::read_to_string(&credential_path).map_err(|error| {
        CiaoError::Config(format!(
            "Cloudflare tunnel credentials are missing at {}; rerun `cloudflared tunnel create {tunnel_name}`: {error}",
            credential_path.display()
        ))
    })?;
    if credentials.is_empty() || credentials.len() > 1024 * 1024 {
        return Err(CiaoError::Config(
            "Cloudflare tunnel credentials are empty or unexpectedly large".to_owned(),
        ));
    }
    ensure_remote_cloudflared(transport, os)?;
    remote_script(
        transport,
        "prepare Cloudflare Tunnel directory",
        "set -eu\nsudo -n install -d -m 0700 /etc/cloudflared\n",
    )?;
    let remote_credentials = format!("/etc/cloudflared/{tunnel_id}.json");
    write_remote_file(
        transport,
        &remote_credentials,
        &credentials,
        "root",
        "install Cloudflare Tunnel credentials",
    )?;
    remote_script(
        transport,
        "protect Cloudflare Tunnel credentials",
        &format!(
            "set -eu\nsudo -n chmod 0600 {}\n",
            shell_quote(&remote_credentials)
        ),
    )?;
    let remote_config = "/etc/cloudflared/config.yml";
    let config = format!(
        "tunnel: {tunnel_id}\ncredentials-file: {remote_credentials}\ningress:\n  - hostname: {domain}\n    service: http://127.0.0.1:80\n    originRequest:\n      httpHostHeader: {domain}\n  - service: http_status:404\n"
    );
    write_remote_file(
        transport,
        remote_config,
        &config,
        "root",
        "write Cloudflare Tunnel configuration",
    )?;
    remote_script(
        transport,
        "install Cloudflare Tunnel service",
        &cloudflared_service_script(os),
    )?;
    Ok(CloudflareTunnelResult {
        app: app.to_owned(),
        domain: domain.to_owned(),
        tunnel: tunnel_name,
        message: format!("Cloudflare Tunnel is active for {domain}"),
    })
}

fn cloudflared_executable() -> Option<PathBuf> {
    find_executable("cloudflared").or_else(|| {
        [
            "/opt/homebrew/bin/cloudflared",
            "/usr/local/bin/cloudflared",
            "/usr/bin/cloudflared",
        ]
        .into_iter()
        .map(PathBuf::from)
        .find(|path| path.is_file())
    })
}

fn ensure_local_cloudflared() -> Result<String> {
    if let Some(path) = cloudflared_executable() {
        return Ok(path.display().to_string());
    }
    let script = if cfg!(target_os = "macos") {
        r#"set -eu
command -v curl >/dev/null 2>&1 || { echo 'macOS curl is required to install cloudflared' >&2; exit 1; }
brew_bin=''
for candidate in /opt/homebrew/bin/brew /usr/local/bin/brew /home/linuxbrew/.linuxbrew/bin/brew; do if [ -x "$candidate" ]; then brew_bin="$candidate"; break; fi; done
if [ -z "$brew_bin" ] && command -v brew >/dev/null 2>&1; then brew_bin=$(command -v brew); fi
if [ -z "$brew_bin" ]; then
    brew_script=$(mktemp -t ciao-homebrew)
    trap 'rm -f "$brew_script"' EXIT
    curl -fsSL 'https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh' -o "$brew_script"
    NONINTERACTIVE=1 /bin/bash "$brew_script"
    for candidate in /opt/homebrew/bin/brew /usr/local/bin/brew /home/linuxbrew/.linuxbrew/bin/brew; do if [ -x "$candidate" ]; then brew_bin="$candidate"; break; fi; done
fi
[ -n "$brew_bin" ] || { echo 'Homebrew installation finished without a usable brew executable' >&2; exit 1; }
if ! "$brew_bin" list --formula cloudflared >/dev/null 2>&1; then "$brew_bin" install cloudflared; fi
"#.to_owned()
    } else if cfg!(target_os = "linux") {
        r#"set -eu
command -v apt-get >/dev/null 2>&1 || { echo 'automatic cloudflared installation requires apt-get' >&2; exit 1; }
command -v curl >/dev/null 2>&1 || { echo 'curl is required to install cloudflared' >&2; exit 1; }
sudo -n install -d -m 0755 /usr/share/keyrings
curl -fsSL 'https://pkg.cloudflare.com/cloudflare-main.gpg' | sudo -n tee /usr/share/keyrings/cloudflare-main.gpg >/dev/null
printf '%s\n' 'deb [signed-by=/usr/share/keyrings/cloudflare-main.gpg] https://pkg.cloudflare.com/cloudflared any main' | sudo -n tee /etc/apt/sources.list.d/cloudflared.list >/dev/null
sudo -n env DEBIAN_FRONTEND=noninteractive apt-get update
sudo -n env DEBIAN_FRONTEND=noninteractive apt-get install -y cloudflared
"#.to_owned()
    } else {
        return Err(CiaoError::Config(
            "automatic cloudflared installation is supported on macOS and Linux".to_owned(),
        ));
    };
    let output = run_local_interactive_capture(&format!(
        "set -eu\nsudo -v </dev/tty >/dev/tty 2>/dev/tty\n{script}"
    ))?;
    if output.status != 0 {
        return Err(CiaoError::LocalCommand {
            stage: "install cloudflared".to_owned(),
            exit: output.status,
            stdout: output.stdout,
            stderr: output.stderr,
        });
    }
    cloudflared_executable()
        .map(|path| path.display().to_string())
        .ok_or_else(|| CiaoError::Config("cloudflared was installed but is not on PATH".to_owned()))
}

fn find_or_create_cloudflare_tunnel(cloudflared: &str, name: &str) -> Result<String> {
    if let Ok(output) = Command::new(cloudflared)
        .args(["tunnel", "list", "--output", "json"])
        .output()
    {
        if output.status.success() {
            if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&output.stdout) {
                let items = value
                    .as_array()
                    .or_else(|| value.get("tunnels").and_then(serde_json::Value::as_array));
                if let Some(id) = items.and_then(|items| {
                    items.iter().find_map(|item| {
                        (item.get("name").and_then(serde_json::Value::as_str) == Some(name))
                            .then(|| {
                                item.get("id")
                                    .or_else(|| item.get("uuid"))
                                    .and_then(serde_json::Value::as_str)
                            })
                            .flatten()
                    })
                }) {
                    return Ok(id.to_owned());
                }
            }
            let text = String::from_utf8_lossy(&output.stdout);
            if let Some(id) = text.lines().find_map(|line| {
                let fields = line.split_whitespace().collect::<Vec<_>>();
                fields.contains(&name).then(|| uuid_from_text(line))?
            }) {
                return Ok(id);
            }
        }
    }
    let output = Command::new(cloudflared)
        .args(["tunnel", "create", name])
        .output()
        .map_err(|error| CiaoError::LocalCommand {
            stage: "create Cloudflare Tunnel".to_owned(),
            exit: 1,
            stdout: String::new(),
            stderr: error.to_string(),
        })?;
    let output_text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    if !output.status.success() {
        return Err(CiaoError::LocalCommand {
            stage: "create Cloudflare Tunnel".to_owned(),
            exit: output.status.code().unwrap_or(1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    uuid_from_text(&output_text).ok_or_else(|| {
        CiaoError::Config("Cloudflare created the tunnel, but Ciao could not read its ID; run `cloudflared tunnel list` and retry".to_owned())
    })
}

fn uuid_from_text(value: &str) -> Option<String> {
    value
        .split(|character: char| !character.is_ascii_hexdigit() && character != '-')
        .find(|token| {
            token.len() == 36
                && token.chars().enumerate().all(|(index, character)| {
                    character.is_ascii_hexdigit()
                        || (matches!(index, 8 | 13 | 18 | 23) && character == '-')
                })
        })
        .map(str::to_owned)
}

fn ensure_remote_cloudflared(transport: &OpenSshTransport, os: &HostOs) -> Result<()> {
    if remote_script(
        transport,
        "detect cloudflared on target",
        "set -eu\ncommand -v cloudflared >/dev/null 2>&1\n",
    )
    .is_ok()
    {
        return Ok(());
    }
    let script = match os {
        HostOs::Linux => r#"set -eu
command -v curl >/dev/null 2>&1 || { echo 'curl is required to install cloudflared' >&2; exit 1; }
sudo -n install -d -m 0755 /usr/share/keyrings
curl -fsSL 'https://pkg.cloudflare.com/cloudflare-main.gpg' | sudo -n tee /usr/share/keyrings/cloudflare-main.gpg >/dev/null
printf '%s\n' 'deb [signed-by=/usr/share/keyrings/cloudflare-main.gpg] https://pkg.cloudflare.com/cloudflared any main' | sudo -n tee /etc/apt/sources.list.d/cloudflared.list >/dev/null
sudo -n env DEBIAN_FRONTEND=noninteractive apt-get update
sudo -n env DEBIAN_FRONTEND=noninteractive apt-get install -y cloudflared
"#.to_owned(),
        HostOs::MacOs => r#"set -eu
brew_bin=''
for candidate in /opt/homebrew/bin/brew /usr/local/bin/brew /home/linuxbrew/.linuxbrew/bin/brew; do if [ -x "$candidate" ]; then brew_bin="$candidate"; break; fi; done
if [ -z "$brew_bin" ] && command -v brew >/dev/null 2>&1; then brew_bin=$(command -v brew); fi
[ -n "$brew_bin" ] || { echo 'Homebrew is not available on the target' >&2; exit 1; }
if ! "$brew_bin" list --formula cloudflared >/dev/null 2>&1; then "$brew_bin" install cloudflared; fi
"#.to_owned(),
        HostOs::Unknown(value) => {
            return Err(CiaoError::Config(format!(
                "automatic cloudflared installation is unsupported on host OS {value}"
            )))
        }
    };
    remote_script(transport, "install cloudflared on target", &script)?;
    Ok(())
}

fn cloudflared_service_script(os: &HostOs) -> String {
    match os {
        HostOs::Linux => r#"set -eu
command -v cloudflared >/dev/null 2>&1 || { echo 'cloudflared is not installed on the target' >&2; exit 1; }
sudo -n cloudflared --config /etc/cloudflared/config.yml service install >/dev/null 2>&1 || true
sudo -n systemctl enable --now cloudflared
sudo -n systemctl restart cloudflared
"#.to_owned(),
        HostOs::MacOs => r#"set -eu
command -v cloudflared >/dev/null 2>&1 || { echo 'cloudflared is not installed on the target' >&2; exit 1; }
sudo -n cloudflared --config /etc/cloudflared/config.yml service install >/dev/null 2>&1 || true
sudo -n launchctl kickstart -k system/com.cloudflare.cloudflared 2>/dev/null || sudo -n launchctl start com.cloudflare.cloudflared
"#.to_owned(),
        HostOs::Unknown(_) => String::new(),
    }
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
            && Command::new("ifconfig")
                .args(["lo0"])
                .output()
                .is_ok_and(|output| String::from_utf8_lossy(&output.stdout).contains("10.0.0.1"))
    } else {
        Path::new("/etc/dnsmasq.d/ciao-ciao.conf").is_file()
            && Path::new("/etc/systemd/resolved.conf.d/ciao-ciao.conf").is_file()
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
        "set -eu\ntrap 'exit 130' INT TERM\ncd -- {}\nexport HOST=127.0.0.1\nexport PORT={}\n",
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

/// Build the short-lived local runner used by `ciao run`.
///
/// This path never installs Caddy and never writes the `.ciao` resolver. It
/// runs one project on loopback, then exits when the foreground process exits.
pub fn local_run_script(plan: &LocalDevPlan) -> Result<Vec<u8>> {
    let mut script = format!(
        "set -eu\ntrap 'exit 130' INT TERM\ncd -- {}\nexport HOST=127.0.0.1\nexport PORT={}\n",
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
    match plan.app_type {
        AppType::Service => {
            let run = plan
                .run_command
                .as_deref()
                .ok_or_else(|| CiaoError::Config("local service has no run command".to_owned()))?;
            if run.trim().is_empty() {
                return Err(CiaoError::Config(
                    "local service run command cannot be empty".to_owned(),
                ));
            }
            script.push_str("exec ");
            script.push_str(run);
            script.push('\n');
        }
        AppType::Static => {
            let root = plan.static_root.as_ref().ok_or_else(|| {
                CiaoError::Config("static project has no output directory".to_owned())
            })?;
            let root = shell_quote(&root.to_string_lossy());
            script.push_str(&format!(
                "test -d {root} || {{ echo 'static build did not create {}' >&2; exit 1; }}\n",
                shell_quote(
                    plan.static_root
                        .as_ref()
                        .and_then(|path| path.file_name())
                        .map(|name| name.to_string_lossy())
                        .as_deref()
                        .unwrap_or("the output directory")
                ),
                root = root
            ));
            script.push_str("if command -v python3 >/dev/null 2>&1; then exec python3 -m http.server \"$PORT\" --bind 127.0.0.1 --directory ");
            script.push_str(&root);
            script.push_str("; fi\n");
            script.push_str("if command -v python >/dev/null 2>&1; then exec python -m http.server \"$PORT\" --bind 127.0.0.1 --directory ");
            script.push_str(&root);
            script.push_str("; fi\n");
            script.push_str("if command -v npx >/dev/null 2>&1; then exec npx --yes serve --listen \"127.0.0.1:$PORT\" ");
            script.push_str(&root);
            script.push_str("; fi\n");
            script.push_str(
                "echo 'ciao run needs python3, python or npx to serve static files' >&2\nexit 127\n",
            );
        }
    }
    Ok(script.into_bytes())
}

pub fn run_local_project(plan: &LocalDevPlan) -> Result<i32> {
    run_local_project_with_reporter(plan, &NoopProgressReporter)
}

pub fn run_local_project_with_reporter(
    plan: &LocalDevPlan,
    reporter: &dyn ProgressReporter,
) -> Result<i32> {
    for (step, command) in [
        ("install dependencies", plan.install_command.as_deref()),
        ("build", plan.build_command.as_deref()),
    ]
    .into_iter()
    .filter_map(|(step, command)| command.map(|command| (step, command)))
    {
        if command.trim().is_empty() {
            return Err(CiaoError::Config(format!(
                "local {step} command cannot be empty"
            )));
        }
        progress_step(reporter, step, || {
            let script = format!(
                "set -eu\ncd -- {}\n{}\n",
                shell_quote(&plan.source.to_string_lossy()),
                command
            );
            let output = run_local_script(script.as_bytes())?;
            if output.status != 0 {
                return Err(CiaoError::LocalCommand {
                    stage: step.to_owned(),
                    exit: output.status,
                    stdout: output.stdout,
                    stderr: output.stderr,
                });
            }
            Ok(())
        })?;
    }
    let mut process = match plan.app_type {
        AppType::Service => {
            let run = plan
                .run_command
                .as_deref()
                .ok_or_else(|| CiaoError::Config("local service has no run command".to_owned()))?;
            if run.trim().is_empty() {
                return Err(CiaoError::Config(
                    "local service run command cannot be empty".to_owned(),
                ));
            }
            let script = format!(
                "set -eu\ncd -- {}\nexec {}\n",
                shell_quote(&plan.source.to_string_lossy()),
                run
            );
            let mut command = Command::new("sh");
            command
                .arg("-s")
                .env("HOST", "127.0.0.1")
                .env("PORT", plan.port.to_string())
                .stdin(Stdio::piped())
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit());
            own_process_group(&mut command);
            let mut child = command.spawn()?;
            if let Err(error) = child
                .stdin
                .take()
                .expect("piped stdin")
                .write_all(script.as_bytes())
            {
                terminate_child(&mut child);
                return Err(error.into());
            }
            child
        }
        AppType::Static => {
            let root = plan.static_root.as_ref().ok_or_else(|| {
                CiaoError::Config("static project has no output directory".to_owned())
            })?;
            if !root.is_dir() {
                return Err(CiaoError::Detection(format!(
                    "static build did not create {}",
                    root.display()
                )));
            }
            let mut command;
            if let Some(program) = ["python3", "python"]
                .into_iter()
                .find(|program| find_executable(program).is_some())
            {
                command = Command::new(program);
                command.args([
                    "-m",
                    "http.server",
                    &plan.port.to_string(),
                    "--bind",
                    "127.0.0.1",
                    "--directory",
                    &root.to_string_lossy(),
                ]);
            } else if find_executable("npx").is_some() {
                command = Command::new("npx");
                command.args([
                    "--yes",
                    "serve",
                    "--listen",
                    &format!("127.0.0.1:{}", plan.port),
                    &root.to_string_lossy(),
                ]);
            } else {
                return Err(CiaoError::Config(
                    "ciao run needs python3, python or npx to serve static files".to_owned(),
                ));
            }
            command.stdout(Stdio::inherit()).stderr(Stdio::inherit());
            own_process_group(&mut command);
            command.spawn()?
        }
    };
    reporter.started("start local server");
    let status = match wait_child_with_cancellation(&mut process)? {
        Some(status) => status.code().unwrap_or(128),
        None => {
            reporter.failed("start local server");
            return Ok(130);
        }
    };
    if status == 0 {
        reporter.finished("start local server");
    } else {
        reporter.failed("start local server");
    }
    Ok(status)
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
    let mut command = Command::new("sh");
    command
        .arg("-s")
        .stdin(Stdio::piped())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    // The shell owns the local Caddy cleanup trap; its process group also
    // contains the foreground development server.
    own_process_group(&mut command);
    let mut child = command.spawn()?;
    if let Err(error) = child.stdin.take().expect("piped stdin").write_all(&script) {
        terminate_child(&mut child);
        return Err(error.into());
    }
    Ok(match wait_child_with_cancellation(&mut child)? {
        Some(status) => status.code().unwrap_or(128),
        None => 130,
    })
}

const CADDY_IMPORT: &str = "import /etc/caddy/ciao/*.caddy";

/// Return the fixed, idempotent host bootstrap script.
///
/// The script deliberately supports only the first-class host families. It
/// installs Ciao's small set of remote prerequisites and Caddy through a
/// native package manager, then leaves service supervision to systemd,
/// launchd, or Homebrew's launchd integration.
pub fn host_init_script(os: &HostOs) -> Result<String> {
    match os {
        HostOs::Linux => Ok(format!(
            r#"set -eu
command -v sudo >/dev/null 2>&1 || {{ echo 'Ciao requires sudo on the remote host' >&2; exit 1; }}
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
sudo -n install -d -m 0755 /etc/caddy/ciao
import_line='{CADDY_IMPORT}'
if ! sudo -n test -f /etc/caddy/Caddyfile; then
    printf '%s\n' "$import_line" | sudo -n tee /etc/caddy/Caddyfile >/dev/null
elif ! sudo -n grep -Fqx "$import_line" /etc/caddy/Caddyfile; then
    printf '\n%s\n' "$import_line" | sudo -n tee -a /etc/caddy/Caddyfile >/dev/null
fi
sudo -n caddy validate --config /etc/caddy/Caddyfile
sudo -n systemctl enable --now caddy
sudo -n systemctl reload caddy
printf 'ciao_host_init=ready\n'
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
    brew_install_script=$(mktemp -t ciao-homebrew)
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
[ -x "$caddy_bin" ] || {{ echo 'Ciao could not locate the Caddy binary after installation' >&2; exit 1; }}
sudo -n install -d -m 0755 /etc/caddy/ciao
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
printf 'ciao_host_init=ready\n'
"#
        )),
        HostOs::Unknown(value) => Err(CiaoError::Config(format!(
            "host initialization is unsupported on OS `{value}`"
        ))),
    }
}

/// Install and configure the dependencies Ciao needs on a target host.
///
/// This remains available as an explicit idempotent operation; the normal
/// terminal deploy path invokes the same bootstrap when readiness fails.
pub fn init_host(transport: &OpenSshTransport) -> Result<HostInitResult> {
    let platform = transport.inspect()?;
    validate_host_init_platform(&platform)?;
    let script = host_init_script(&platform.os)?;
    remote_script(transport, "initialize host dependencies", &script)?;
    Ok(host_init_result(platform))
}

fn validate_host_init_platform(platform: &HostPlatform) -> Result<()> {
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
    Ok(())
}

fn host_init_dependencies(os: &HostOs) -> Vec<String> {
    match os {
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
    }
}

fn host_init_result(platform: HostPlatform) -> HostInitResult {
    HostInitResult {
        dependencies: host_init_dependencies(&platform.os),
        platform,
        message: "host dependencies and Caddy are ready".to_owned(),
    }
}

/// Run the complete host bootstrap in the same SSH pseudo-terminal that
/// prompts for the user's normal sudo password. This matters on systems with
/// sudo's default `timestamp_type=tty`: authorizing in one SSH connection and
/// executing the bootstrap in separate connections is not reliable.
pub fn init_host_interactively(transport: &OpenSshTransport) -> Result<HostInitResult> {
    let platform = transport.inspect()?;
    validate_host_init_platform(&platform)?;
    let script = host_init_script(&platform.os)?;
    run_interactive_ssh_script(transport, "initialize host dependencies", &script)?;
    Ok(host_init_result(platform))
}

fn passwordless_sudo_script(os: &HostOs, user: &str) -> Result<String> {
    validate_owner("SSH user", user)?;
    let rule = shell_quote(&format!("{user} ALL=(ALL) NOPASSWD: ALL"));
    match os {
        HostOs::Linux | HostOs::MacOs => Ok(format!(
            r#"set -eu
policy_line={rule}
tmp=$(mktemp)
trap 'rm -f "$tmp"' EXIT
if sudo grep -Eq '^[[:space:]]*(@|#)includedir[[:space:]]+.*sudoers[.]d' /etc/sudoers 2>/dev/null; then
    printf '%s\n' "$policy_line" >"$tmp"
    sudo visudo -cf "$tmp"
    sudo install -d -m 0755 /etc/sudoers.d
    sudo install -m 0440 "$tmp" /etc/sudoers.d/ciao
else
    sudo cat /etc/sudoers >"$tmp"
    if ! grep -Fqx "$policy_line" "$tmp"; then
        printf '\n%s\n' "$policy_line" >>"$tmp"
    fi
    sudo visudo -cf "$tmp"
    sudo sh -c 'cat "$1" > /etc/sudoers' sh "$tmp"
    sudo chmod 0440 /etc/sudoers
fi
sudo visudo -c
sudo -n true
printf 'ciao_passwordless_sudo=ready\n'
"#,
            rule = rule
        )),
        HostOs::Unknown(value) => Err(CiaoError::Config(format!(
            "cannot configure passwordless sudo on unsupported OS `{value}`"
        ))),
    }
}

/// Configure the deploy SSH user's passwordless sudo policy after the user
/// has approved it in the terminal. The password stays inside OpenSSH/sudo.
pub fn configure_passwordless_sudo_interactively(transport: &OpenSshTransport) -> Result<()> {
    let user = ssh_login_user(&transport.target).ok_or_else(|| {
        CiaoError::Config(
            "passwordless sudo setup needs an explicit user@host SSH target".to_owned(),
        )
    })?;
    let platform = transport.inspect()?;
    validate_host_init_platform(&platform)?;
    let script = passwordless_sudo_script(&platform.os, &user)?;
    run_interactive_ssh_script(transport, "configure passwordless sudo", &script)
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
        (HostOs::Linux, Runtime::Python) => {
            "set -eu\nif ! command -v python3 >/dev/null 2>&1 || ! command -v pip3 >/dev/null 2>&1; then sudo -n env DEBIAN_FRONTEND=noninteractive apt-get update; sudo -n env DEBIAN_FRONTEND=noninteractive apt-get install -y python3 python3-pip python3-venv; fi\ncommand -v python3 >/dev/null 2>&1\ncommand -v pip3 >/dev/null 2>&1\n".to_owned()
        }
        (HostOs::Linux, Runtime::Node | Runtime::Astro) => {
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
        (HostOs::MacOs, Runtime::Python) => macos_runtime_script("python", &["python3"]),
        (HostOs::MacOs, Runtime::Bun) => macos_runtime_script("bun", &["bun"]),
        (HostOs::MacOs, Runtime::Node | Runtime::Astro) => {
            macos_runtime_script("node", &["node", "npm"])
        }
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
        "set -eu\nbrew_bin=''\nfor candidate in /opt/homebrew/bin/brew /usr/local/bin/brew; do if [ -x \"$candidate\" ]; then brew_bin=\"$candidate\"; break; fi; done\n[ -n \"$brew_bin\" ] || {{ echo 'Homebrew is missing; run ciao host init first' >&2; exit 1; }}\nif {missing}; then \"$brew_bin\" install {formula}; fi\n{checks}\n",
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
    deploy_with_reporter(
        transport,
        source,
        plan,
        domain,
        dry_run,
        &NoopProgressReporter,
    )
}

pub fn deploy_with_reporter(
    transport: &OpenSshTransport,
    source: &Path,
    plan: &ProjectPlan,
    domain: Option<&str>,
    dry_run: bool,
    reporter: &dyn ProgressReporter,
) -> Result<DeployResult> {
    deploy_with_mode(
        transport,
        source,
        plan,
        domain,
        dry_run,
        reporter,
        DeployHostMode::NonInteractive,
    )
}

pub fn deploy_with_mode(
    transport: &OpenSshTransport,
    source: &Path,
    plan: &ProjectPlan,
    domain: Option<&str>,
    dry_run: bool,
    reporter: &dyn ProgressReporter,
    host_mode: DeployHostMode,
) -> Result<DeployResult> {
    if dry_run {
        return deploy_unlocked(transport, source, plan, domain, true, reporter);
    }
    let platform = progress_step(reporter, "inspect host", || transport.inspect())?;
    // A new host cannot acquire Ciao's root-owned deployment lock yet. Prepare
    // it first, then serialize the release transaction with the normal lock.
    match host_mode {
        DeployHostMode::NonInteractive => prepare_host_for_deploy(transport, reporter)?,
        DeployHostMode::Interactive => prepare_host_for_deploy_interactive(transport, reporter)?,
    }
    let root = host_app_root(&platform.os);
    let lock_owner = release_id();
    if let Err(error) = progress_step(reporter, "acquire deployment lock", || {
        acquire_deploy_lock(transport, &root, &plan.name, &lock_owner)
    }) {
        if cancellation_requested() {
            let _ = progress_step_uncancellable(reporter, "release deployment lock", || {
                release_deploy_lock(transport, &root, &plan.name, &lock_owner)
            });
        }
        return Err(error);
    }
    let result = deploy_unlocked(transport, source, plan, domain, false, reporter);
    let unlock_result = progress_step_uncancellable(reporter, "release deployment lock", || {
        release_deploy_lock(transport, &root, &plan.name, &lock_owner)
    });
    match (result, unlock_result) {
        (Ok(result), Ok(())) => Ok(result),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(error)) => Err(CiaoError::Deployment {
            stage: "release deployment lock".to_owned(),
            message: error.to_string(),
            previous_release: "unknown".to_owned(),
        }),
        (Err(error), Err(unlock_error)) => Err(CiaoError::Deployment {
            stage: "deploy".to_owned(),
            message: format!("{error}; deployment lock cleanup failed: {unlock_error}"),
            previous_release: "unknown".to_owned(),
        }),
    }
}

/// Remove only an interrupted deployment marker. This never stops processes;
/// callers must ask the human to confirm that no other deployment is active.
pub fn recover_deploy_lock(transport: &OpenSshTransport, app: &str) -> Result<()> {
    validate_identifier("app name", app)?;
    let platform = transport.inspect()?;
    let root = host_app_root(&platform.os);
    let lock = deploy_lock_path(&root, app);
    remote_script(
        transport,
        "recover interrupted deployment lock",
        &format!(
            "set -eu\nif sudo -n test -d {lock}; then sudo -n rm -f {started} {owner}; sudo -n rmdir {lock}; fi\n",
            lock = shell_quote(&lock),
            started = shell_quote(&format!("{lock}/started")),
            owner = shell_quote(&format!("{lock}/owner")),
        ),
    )
    .map(|_| ())
}

fn deploy_unlocked(
    transport: &OpenSshTransport,
    source: &Path,
    plan: &ProjectPlan,
    domain: Option<&str>,
    dry_run: bool,
    reporter: &dyn ProgressReporter,
) -> Result<DeployResult> {
    if let Some(domain) = domain {
        validate_domain(domain)?;
    }
    let release = release_id();
    validate_identifier("release", &release)?;
    if dry_run {
        let steps = if plan.app_type == AppType::Static {
            "upload, install dependencies, build, verify static output, activate current, update local Ciao route"
        } else {
            "upload, install dependencies, build, start candidate, healthcheck, activate service, update local Ciao route"
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
    let platform = progress_step(reporter, "prepare deployment", || transport.inspect())?;
    let root = host_app_root(&platform.os);
    let previous_release = read_current_release(transport, &root, &plan.name)?;
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
    progress_step(reporter, "prepare remote layout", || {
        ensure_remote_layout(transport, &platform.os, &plan.name, &release)
    })?;
    let staging = format!("/tmp/ciao-{}-{}", plan.name, release);
    let release_path = format!("{root}/{}/releases/{release}", plan.name);
    let build_home = build_cache_path(&platform.os, &plan.name)?;
    let build_env = format!("{root}/{}/shared/env", plan.name);
    let user = service_user(transport, &platform.os, &plan.name)?;
    let user_group = match &platform.os {
        HostOs::MacOs => "staff".to_owned(),
        _ => user.clone(),
    };
    if let Err(error) = progress_step(reporter, "prepare runtime", || {
        ensure_runtime(
            transport,
            &platform.os,
            &plan.runtime,
            &user,
            &format!("{root}/{}", plan.name),
        )
    }) {
        return Err(CiaoError::Deployment {
            stage: "runtime initialization".to_owned(),
            message: error.to_string(),
            previous_release: previous_release
                .clone()
                .unwrap_or_else(|| "none".to_owned()),
        });
    }
    let result = (|| {
        progress_step(reporter, "upload source", || {
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
            transport.upload_tar_with_progress(source, &staging, reporter)?;
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
            Ok::<(), CiaoError>(())
        })?;
        let mut manifest = ReleaseManifest::from_plan(release.clone(), source, plan);
        manifest.port = port;
        progress_step(reporter, "write release manifest", || {
            write_remote_file(
                transport,
                &format!("{release_path}/ciao-manifest.toml"),
                &manifest.to_toml()?,
                &user,
                "write release manifest",
            )
        })?;
        if plan.app_type == AppType::Static {
            if let Some(install) = plan.install_command.as_deref() {
                progress_step(reporter, "install dependencies", || {
                    run_as_user_script(
                        transport,
                        &user,
                        "install dependencies",
                        &command_script_with_home(install, &release_path, &build_home, &build_env)?,
                    )
                })?;
            }
            if let Some(build) = plan.build_command.as_deref() {
                progress_step(reporter, "build static site", || {
                    run_as_user_script(
                        transport,
                        &user,
                        "build static site",
                        &command_script_with_home(build, &release_path, &build_home, &build_env)?,
                    )
                })?;
            }
            let static_directory = plan.static_directory.as_deref().ok_or_else(|| {
                CiaoError::Config("static deployment has no output directory".to_owned())
            })?;
            let static_path = format!("{release_path}/{static_directory}");
            progress_step(reporter, "verify static output", || {
                remote_script(
                    transport,
                    "verify static output",
                    &format!(
                        "set -eu\nsudo -n test -d {} || {{ echo 'static build did not create {}' >&2; exit 1; }}\n",
                        shell_quote(&static_path),
                        shell_quote(static_directory)
                    ),
                )
                .map(|_| ())
            })?;
            progress_step(reporter, "activate static release", || {
                harden_release(transport, &platform.os, &release_path)?;
                remote_script(
                    transport,
                    "activate static release",
                    &switch_current_script(&platform.os, &root, &plan.name, &release_path),
                )
                .map(|_| ())
            })?;
        } else {
            let port = port.expect("service plans have a port");
            let start_script = start_script(
                &release_path,
                release_start_command(plan)?,
                port,
                &format!("{root}/{}/shared/env", plan.name),
            )?;
            progress_step(reporter, "prepare service release", || {
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
                )
                .map(|_| ())
            })?;
            if let Some(install) = plan.install_command.as_deref() {
                progress_step(reporter, "install dependencies", || {
                    run_as_user_script(
                        transport,
                        &user,
                        "install dependencies",
                        &command_script_with_home(install, &release_path, &build_home, &build_env)?,
                    )
                })?;
            }
            if let Some(build) = plan.build_command.as_deref() {
                progress_step(reporter, "build", || {
                    run_as_user_script(
                        transport,
                        &user,
                        "build",
                        &command_script_with_home(build, &release_path, &build_home, &build_env)?,
                    )
                })?;
            }
            let candidate_unit = service_unit_name(&plan.name, true);
            progress_step(reporter, "candidate healthcheck", || {
                if platform.os == HostOs::MacOs {
                    run_macos_candidate(transport, &user, &release_path, port, &plan.health)
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
                    remove_service(transport, &platform.os, &candidate_unit)
                }
            })?;
            progress_step(reporter, "activate service release", || {
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
                remote_healthcheck(transport, port, &plan.health)
            })?;
        }
        progress_step(reporter, "configure local Ciao domain", || {
            configure_remote_ciao_domain(transport, &plan.name)
        })?;
        if let Some(domain) = effective_domain {
            progress_step(reporter, "configure domain", || {
                if existing_domain_is_plain_http(transport, &plan.name)? {
                    configure_domain_for_cloudflare(transport, &plan.name, domain)
                } else {
                    configure_domain(transport, &plan.name, domain)
                }
            })?;
        }
        progress_step(reporter, "prune old releases", || {
            prune_releases(transport, &root, &plan.name, 5)
        })?;
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
                    let _ = if existing_domain_is_plain_http(transport, &plan.name).unwrap_or(false)
                    {
                        configure_domain_for_cloudflare(transport, &plan.name, domain)
                    } else {
                        configure_domain(transport, &plan.name, domain)
                    };
                }
                let _ = configure_remote_ciao_domain(transport, &plan.name);
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
            let _ = remote_script(
                transport,
                "remove failed local Ciao route",
                &format!(
                    "set -eu\nsudo -n rm -f {}\n",
                    shell_quote(&format!("/etc/caddy/ciao/{}.local.caddy", plan.name))
                ),
            );
            let _ = remove_service(
                transport,
                &platform.os,
                &service_unit_name(&plan.name, false),
            );
        }
        // Cleanup performs its own remote current-release check. Keeping that
        // check and the removal in one SSH command avoids a stale local read
        // leaving a partial release behind when cancellation interrupts a
        // preceding SSH session.
        let _ = cleanup_release(transport, &platform.os, &plan.name, &release);
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
                full_output: false,
            };
            transport.exec(command.clone())?
        }
        HostOs::MacOs => {
            if let Some(since) = since {
                validate_since(since)?;
                return Err(CiaoError::Config(
                    "`logs --since` is not available for macOS file-backed logs; omit it for a bounded snapshot".to_owned(),
                ));
            }
            let command = CommandSpec::fixed("sh", &["-s"], "read logs").with_stdin(
                format!(
                    "set -eu\nstdout=/Library/Ciao/logs/{app}.out\nstderr=/Library/Ciao/logs/{app}.err\nif test -f \"$stdout\"; then cat \"$stdout\"; fi\nif test -f \"$stderr\"; then cat \"$stderr\" >&2; fi\n",
                    app = shell_quote(app),
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
            if existing_domain_is_plain_http(transport, app)? {
                configure_domain_for_cloudflare(transport, app, domain)?;
            } else {
                add_domain(transport, app, domain)?;
            }
        }
        configure_remote_ciao_domain(transport, app)?;
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
                if existing_domain_is_plain_http(transport, app)? {
                    configure_domain_for_cloudflare(transport, app, domain)?;
                } else {
                    add_domain(transport, app, domain)?;
                }
            }
            configure_remote_ciao_domain(transport, app)?;
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
    let line = env_file_line(key, value);
    let script = format!(
        "set -eu\nroot={}\nfile={}\nsudo -n install -d -m 0755 \"$root\"\nsudo -n touch \"$file\"\nsudo -n chmod 0600 \"$file\"\nsudo -n sed -i.bak '/^{}=/d' \"$file\"\nprintf '%s\\n' {} | sudo -n tee -a \"$file\" >/dev/null\nsudo -n rm -f \"$file.bak\"\nsudo -n chown {} \"$file\"\n",
        shell_quote(&format!("{root}/{app}/shared")),
        shell_quote(&path),
        regex_literal(key),
        shell_quote(&line),
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
    let fragment_path = format!("/etc/caddy/ciao/{app}.caddy");
    remote_script(
        transport,
        "prepare Caddy directory",
        "set -eu\nsudo -n install -d -m 0755 /etc/caddy/ciao\n",
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

/// Use plain HTTP between a Cloudflare Tunnel and Caddy. Cloudflare handles
/// the public TLS connection; keeping the origin local avoids a certificate
/// challenge loop through the tunnel.
pub fn configure_domain_for_cloudflare(
    transport: &OpenSshTransport,
    app: &str,
    domain: &str,
) -> Result<()> {
    validate_identifier("app name", app)?;
    validate_domain(domain)?;
    let platform = transport.inspect()?;
    let root = host_app_root(&platform.os);
    let release = read_current_release(transport, &root, app)?
        .ok_or_else(|| CiaoError::Config(format!("app `{app}` has no active release")))?;
    let fragment = caddy_fragment_with_scheme(transport, &root, app, &release, domain, true)?;
    let fragment_path = format!("/etc/caddy/ciao/{app}.caddy");
    write_remote_file(
        transport,
        &fragment_path,
        &fragment,
        "root",
        "write Cloudflare Caddy route",
    )?;
    remote_script(
        transport,
        "reload Caddy for Cloudflare",
        &caddy_reload_script(&platform.os),
    )?;
    Ok(())
}

/// Add the stable local `.ciao` host on the remote Caddy instance. It uses a
/// separate fragment, so a public domain configured for the same app remains
/// active at the same time.
pub fn configure_remote_ciao_domain(transport: &OpenSshTransport, app: &str) -> Result<()> {
    validate_identifier("app name", app)?;
    let domain = local_domain(app)?;
    let platform = transport.inspect()?;
    let root = host_app_root(&platform.os);
    let release = read_current_release(transport, &root, app)?
        .ok_or_else(|| CiaoError::Config(format!("app `{app}` has no active release")))?;
    let fragment = caddy_fragment_with_scheme(transport, &root, app, &release, &domain, true)?;
    let fragment_path = format!("/etc/caddy/ciao/{app}.local.caddy");
    remote_script(
        transport,
        "prepare local Ciao Caddy directory",
        "set -eu\nsudo -n install -d -m 0755 /etc/caddy/ciao\n",
    )?;
    write_remote_file(
        transport,
        &fragment_path,
        &fragment,
        "root",
        "write local Ciao Caddy route",
    )?;
    remote_script(
        transport,
        "reload Caddy for local Ciao domain",
        &caddy_reload_script(&platform.os),
    )?;
    Ok(())
}

fn remove_domain_fragment(transport: &OpenSshTransport, app: &str) -> Result<()> {
    validate_identifier("app name", app)?;
    let path = format!("/etc/caddy/ciao/{app}.caddy");
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
    caddy_fragment_with_scheme(transport, root, app, release, domain, false)
}

fn caddy_fragment_with_scheme(
    transport: &OpenSshTransport,
    root: &str,
    app: &str,
    release: &str,
    domain: &str,
    cloudflare_origin: bool,
) -> Result<String> {
    let site = if cloudflare_origin {
        format!("http://{domain}")
    } else {
        domain.to_owned()
    };
    if let Some(static_directory) = read_release_static_directory(transport, root, app, release)? {
        let static_root = format!("{root}/{app}/releases/{release}/{static_directory}");
        Ok(format!(
            "{site} {{\n    root * {}\n    file_server\n}}\n",
            static_root
        ))
    } else {
        let port = read_release_port(transport, root, app, release)?.unwrap_or(PORT_START);
        Ok(format!(
            "{site} {{\n    reverse_proxy 127.0.0.1:{port}\n}}\n"
        ))
    }
}

fn read_existing_domain(transport: &OpenSshTransport, app: &str) -> Result<Option<String>> {
    validate_identifier("app name", app)?;
    let path = format!("/etc/caddy/ciao/{app}.caddy");
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
        let normalized = value
            .strip_prefix("http://")
            .or_else(|| value.strip_prefix("https://"))
            .unwrap_or(value)
            .trim_end_matches('/');
        validate_domain(normalized)?;
        Ok(Some(normalized.to_owned()))
    }
}

fn existing_domain_is_plain_http(transport: &OpenSshTransport, app: &str) -> Result<bool> {
    validate_identifier("app name", app)?;
    let path = format!("/etc/caddy/ciao/{app}.caddy");
    let output = remote_script(
        transport,
        "read existing domain scheme",
        &format!(
            "set -eu\nif sudo -n test -f {}; then sudo -n awk 'NR == 1 {{print $1; exit}}' {}; fi\n",
            shell_quote(&path),
            shell_quote(&path)
        ),
    )?;
    Ok(output.stdout.trim_start().starts_with("http://"))
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
    let path = format!("/etc/caddy/ciao/{app}.caddy");
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
        "set -eu\n{setup}sudo -n test -f {config}\nif ! sudo -n grep -Fq 'import /etc/caddy/ciao/*.caddy' {config}; then echo 'Caddyfile must import /etc/caddy/ciao/*.caddy' >&2; exit 1; fi\ncaddy_bin=$(command -v caddy || true)\nfor candidate in /opt/homebrew/bin/caddy /usr/local/bin/caddy /opt/homebrew/opt/caddy/bin/caddy /usr/bin/caddy; do if [ -z \"$caddy_bin\" ] && [ -x \"$candidate\" ]; then caddy_bin=\"$candidate\"; fi; done\nif [ -z \"$caddy_bin\" ]; then echo 'Caddy is not installed; run host initialization' >&2; exit 1; fi\nsudo -n \"$caddy_bin\" validate --config {config} && {reload}\n",
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
        HostOs::MacOs => "/Library/Ciao/apps".to_owned(),
        _ => APP_ROOT.to_owned(),
    }
}

fn build_cache_path(os: &HostOs, app: &str) -> Result<String> {
    validate_identifier("app name", app)?;
    let root = match os {
        HostOs::Linux => "/var/cache/ciao",
        HostOs::MacOs => "/Library/Caches/Ciao",
        HostOs::Unknown(value) => {
            return Err(CiaoError::Config(format!(
                "build cache is unsupported on OS `{value}`"
            )))
        }
    };
    Ok(format!("{root}/{app}"))
}

fn service_user(transport: &OpenSshTransport, os: &HostOs, app: &str) -> Result<String> {
    let user = match os {
        HostOs::MacOs => ssh_login_user(&transport.target).ok_or_else(|| {
            CiaoError::Config(
                "macOS LaunchDaemon requires an explicit user@host SSH target".to_owned(),
            )
        })?,
        _ => format!("ciao-{app}"),
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

fn deploy_lock_path(root: &str, app: &str) -> String {
    format!("{root}/{app}/.deploy-lock")
}

fn acquire_deploy_lock(
    transport: &OpenSshTransport,
    root: &str,
    app: &str,
    owner: &str,
) -> Result<()> {
    validate_identifier("app name", app)?;
    validate_identifier("deployment owner", owner)?;
    remote_script(
        transport,
        "acquire deployment lock",
        &deploy_lock_script(root, app, owner)?,
    )
    .map(|_| ())
}

fn deploy_lock_script(root: &str, app: &str, owner: &str) -> Result<String> {
    validate_identifier("app name", app)?;
    validate_identifier("deployment owner", owner)?;
    let lock = deploy_lock_path(root, app);
    Ok(format!(
        "set -eu\napp_root={app_root}\nlock={lock}\nowner={owner}\nsudo -n install -d -m 0755 \"$app_root\"\nnow=$(date +%s)\nstarted_value=''\nif sudo -n test -f \"$lock/started\"; then started_value=$(sudo -n cat \"$lock/started\" || true); fi\nlock_ready=0\ncleanup_lock() {{ status=$?; if [ \"$lock_ready\" -eq 1 ]; then sudo -n rm -rf \"$lock\"; fi; trap - 0 1 2 3 15; exit \"$status\"; }}\ntrap cleanup_lock 0 1 2 3 15\nif ! sudo -n mkdir -m 0755 \"$lock\" 2>/dev/null; then age='unknown'; case \"$started_value\" in ''|*[!0-9]*) ;; *) age=$((now - started_value));; esac; echo \"another Ciao deployment is already running for this app (lock age: $age seconds)\" >&2; exit 73; fi\nlock_ready=1\nprintf '%s\\n' \"$now\" | sudo -n tee \"$lock/started\" >/dev/null\nprintf '%s\\n' \"$owner\" | sudo -n tee \"$lock/owner\" >/dev/null\ntrap - 0 1 2 3 15\n",
        app_root = shell_quote(&format!("{root}/{app}")),
        lock = shell_quote(&lock),
        owner = shell_quote(owner),
    ))
}

fn release_deploy_lock(
    transport: &OpenSshTransport,
    root: &str,
    app: &str,
    owner: &str,
) -> Result<()> {
    validate_identifier("app name", app)?;
    validate_identifier("deployment owner", owner)?;
    let lock = deploy_lock_path(root, app);
    let script = format!(
        "set -eu\nif ! sudo -n test -d {lock}; then exit 0; fi\nif ! sudo -n test -f {owner_file} || [ \"$(sudo -n cat {owner_file})\" != {owner} ]; then echo 'deployment lock is owned by another process' >&2; exit 73; fi\nsudo -n rm -f {started} {owner_file}\nsudo -n rmdir {lock}\n",
        started = shell_quote(&format!("{lock}/started")),
        owner_file = shell_quote(&format!("{lock}/owner")),
        owner = shell_quote(owner),
        lock = shell_quote(&lock),
    );
    remote_script(transport, "release deployment lock", &script).map(|_| ())
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
    let build_cache = build_cache_path(os, app)?;
    let script = match os {
        HostOs::Linux => format!(
            "set -eu\nif ! id -u {user} >/dev/null 2>&1; then sudo -n useradd --system --user-group --home-dir {app_root} --shell /usr/sbin/nologin {user}; fi\nsudo -n install -d -m 0755 {root}/{app}/releases {root}/{app}/shared /var/cache/ciao\nsudo -n install -d -m 0750 {cache}\nsudo -n chown root:root {root}/{app} {root}/{app}/releases /var/cache/ciao\nsudo -n chown {user}:{user} {root}/{app}/shared {cache}\nsudo -n chmod 0755 {root}/{app} {root}/{app}/releases /var/cache/ciao\nsudo -n chmod 0750 {root}/{app}/shared {cache}\n",
            user = shell_quote(&user),
            app_root = shell_quote(&format!("{root}/{app}")),
            root = shell_quote(&root),
            app = shell_quote(app),
            cache = shell_quote(&build_cache),
        ),
        HostOs::MacOs => format!(
            "set -eu\nsudo -n install -d -m 0755 {root}/{app}/releases {root}/{app}/shared /Library/Caches/Ciao /Library/Ciao/logs\nsudo -n install -d -m 0750 {cache}\nsudo -n chown root:wheel {root}/{app} {root}/{app}/releases /Library/Caches/Ciao\nsudo -n chown {user}:staff {root}/{app}/shared {cache}\nsudo -n touch /Library/Ciao/logs/{app}.out /Library/Ciao/logs/{app}.err\nsudo -n chown {user}:staff /Library/Ciao/logs/{app}.out /Library/Ciao/logs/{app}.err\nsudo -n chmod 0644 /Library/Ciao/logs/{app}.out /Library/Ciao/logs/{app}.err\nsudo -n chmod 0755 {root}/{app} {root}/{app}/releases /Library/Caches/Ciao\nsudo -n chmod 0750 {root}/{app}/shared {cache}\n",
            root = shell_quote(&root),
            app = shell_quote(app),
            user = shell_quote(&user),
            cache = shell_quote(&build_cache),
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
    let path = format!("{root}/{app}/releases/{release}/ciao-manifest.toml");
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
        (Some(port), Some(active)) if port == active => {
            if port >= PORT_END {
                PORT_START
            } else {
                port.saturating_add(1)
            }
        }
        (Some(port), _) if (PORT_START..=PORT_END).contains(&port) => port,
        _ => current_port
            .map(|port| {
                if port >= PORT_END {
                    PORT_START
                } else {
                    port.saturating_add(1)
                }
            })
            .unwrap_or(PORT_START),
    };
    let port_range = u32::from(PORT_END) - u32::from(PORT_START) + 1;
    let output = remote_script(
        transport,
        "allocate internal port",
        &format!(
            "set -eu\nif command -v ss >/dev/null 2>&1; then ss -ltnH 2>/dev/null | awk '{{print $4}}' > /tmp/ciao-ports.$$; elif command -v lsof >/dev/null 2>&1; then lsof -nP -iTCP -sTCP:LISTEN 2>/dev/null | awk 'NR > 1 {{print $9}}' > /tmp/ciao-ports.$$; elif command -v netstat >/dev/null 2>&1; then netstat -ltn 2>/dev/null | awk 'NR > 2 {{print $4}}' > /tmp/ciao-ports.$$; else echo 'port allocation requires ss, lsof or netstat' >&2; exit 1; fi\ntrap 'rm -f /tmp/ciao-ports.$$' EXIT\nfor offset in $(seq 0 {last_offset}); do\n  port=$(( (({start} - {PORT_START} + offset) % {port_range}) + {PORT_START} ))\n  if grep -Eq \"([.:])$port$\" /tmp/ciao-ports.$$; then continue; fi\n  printf '%s\\n' \"$port\"\n  exit 0\ndone\nexit 1\n",
            last_offset = port_range - 1,
            port_range = port_range,
            PORT_START = PORT_START,
            start = start,
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
    let pid_file = format!("/tmp/ciao-candidate-{}.pid", port);
    let script = format!(
        "set -eu\ncd -- {}\nnohup ./start > /tmp/ciao-candidate-{}.log 2>&1 &\nprintf '%s\\n' \"$!\" | tee {} >/dev/null\n",
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
    let app_root = format!("{root}/{app}");
    format!(
        "set -eu\napp_root={}\ncurrent=\"$app_root/current\"\ntmp=\"$app_root/.current-$$\"\ntrap 'sudo -n rm -f \"$tmp\"' EXIT\nsudo -n test -d {}\nif sudo -n test -e \"$current\" && ! sudo -n test -L \"$current\"; then echo 'current exists but is not a symlink' >&2; exit 1; fi\nsudo -n rm -f \"$tmp\"\nsudo -n ln -s {} \"$tmp\"\nsudo -n chown -h root:{group} \"$tmp\"\nsudo -n mv -f \"$tmp\" \"$current\"\ntrap - EXIT\n",
        shell_quote(&app_root),
        shell_quote(release_path),
        shell_quote(release_path),
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
        full_output: false,
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
        full_output: false,
    };
    transport.exec(command).map(|_| ())
}

fn remote_script(transport: &OpenSshTransport, stage: &str, script: &str) -> Result<CommandOutput> {
    transport.exec(CommandSpec::fixed("sh", &["-s"], stage).with_stdin(script.as_bytes().to_vec()))
}

fn service_unit_name(app: &str, candidate: bool) -> String {
    if candidate {
        format!("ciao-{app}-candidate.service")
    } else {
        format!("ciao-{app}.service")
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
        .strip_prefix("ciao-")
        .unwrap_or(unit)
        .trim_end_matches("-candidate.service")
        .trim_end_matches(".service");
    validate_identifier("app name", app)?;
    let unit_contents = match os {
        HostOs::Linux => format!(
            "[Unit]\nDescription=Ciao app {app}\nAfter=network.target\n\n[Service]\nUser={user}\nWorkingDirectory={working_directory}\nEnvironmentFile=-{env_file}\nExecStart=/bin/sh -lc {exec}\nRestart={}\nRestartSec=2\n\n[Install]\nWantedBy=multi-user.target\n",
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
            "/Library/LaunchDaemons/dev.ciao.{}.plist",
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
                .replace("-candidate", ".candidate")
                .trim_start_matches("ciao-")
                .to_owned();
            let plist = format!("/Library/LaunchDaemons/dev.ciao.{}.plist", label);
            let script = match action {
                LifecycleAction::Start => format!("set -eu\nsudo -n launchctl bootstrap system {} 2>/dev/null || sudo -n launchctl kickstart -k system/dev.ciao.{}\n", shell_quote(&plist), shell_quote(&label)),
                LifecycleAction::Stop => format!("set -eu\nsudo -n launchctl bootout system/dev.ciao.{} 2>/dev/null || true\n", shell_quote(&label)),
                LifecycleAction::Restart => format!("set -eu\nsudo -n launchctl bootout system/dev.ciao.{} 2>/dev/null || true\nsudo -n launchctl bootstrap system {}\n", shell_quote(&label), shell_quote(&plist)),
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
                .replace("-candidate", ".candidate")
                .trim_start_matches("ciao-")
                .to_owned();
            let plist = format!(
                "/Library/LaunchDaemons/dev.ciao.{}.plist",
                label
            );
            remote_script(
                transport,
                "remove candidate service",
                &format!(
                    "set -eu\nsudo -n launchctl bootout system/dev.ciao.{} 2>/dev/null || true\nsudo -n rm -f {}\n",
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
                .trim_start_matches("ciao-");
            remote_script(
                transport,
                "read service status",
                &format!(
                    "set -eu\nif sudo -n launchctl print system/dev.ciao.{} >/dev/null 2>&1; then printf 'active\\n'; else printf 'inactive\\n'; fi\n",
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
    let current_path = format!("{root}/{app}/current");
    let release_path = format!("{root}/{app}/releases/{release}");
    let staging_path = format!("/tmp/ciao-{app}-{release}");
    remote_script(
        transport,
        "cleanup failed release",
        &format!(
            "set -eu\ncurrent=''\nif sudo -n test -L {current_path}; then current=$(sudo -n readlink {current_path}); fi\ncase \"$(basename \"$current\")\" in {release}) echo 'refusing to clean a release currently selected by current' >&2; exit 1;; esac\nsudo -n rm -rf {release_path} {staging_path}\n",
            current_path = shell_quote(&current_path),
            release = shell_quote(release),
            release_path = shell_quote(&release_path),
            staging_path = shell_quote(&staging_path),
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
    if !unit.starts_with("ciao-")
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
        "[Unit]\nDescription=Ciao app {app}\nAfter=network.target\n\n[Service]\nUser={user}\nWorkingDirectory={working_directory}\nEnvironmentFile={APP_ROOT}/{app}/shared/env\nExecStart=/bin/sh -lc {command}\nRestart=on-failure\nRestartSec=2\n\n[Install]\nWantedBy=multi-user.target\n"
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
    let label = format!("dev.ciao.{app}");
    Ok(format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\"><dict>\n<key>Label</key><string>{label}</string>\n<key>UserName</key><string>{user}</string>\n<key>EnvironmentVariables</key><dict><key>PATH</key><string>/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin</string></dict>\n<key>ProgramArguments</key><array><string>/bin/sh</string><string>-lc</string><string>{}</string></array>\n<key>WorkingDirectory</key><string>{working_directory}</string>\n<key>KeepAlive</key><true/>\n<key>RunAtLoad</key><true/>\n</dict></plist>\n",
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
        return Err(CiaoError::Config("invalid remote Ciao path".to_owned()));
    }
    Ok(format!("{APP_ROOT}/{app}{suffix}"))
}

fn validate_upload_destination(destination: &str) -> Result<()> {
    if !destination.starts_with("/tmp/ciao-")
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
            &value[..(0..=LIMIT)
                .rev()
                .find(|index| value.is_char_boundary(*index))
                .unwrap_or(0)]
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
    fn remote_sudo_password_detection_only_matches_the_sudo_probe() {
        let password_error = CiaoError::RemoteCommand {
            stage: "check remote administrator privileges".to_owned(),
            exit: 1,
            stdout: String::new(),
            stderr: "sudo: a password is required".to_owned(),
        };
        assert!(remote_sudo_password_required(&password_error));

        let ssh_error = CiaoError::RemoteCommand {
            stage: "remote source extraction".to_owned(),
            exit: 1,
            stdout: String::new(),
            stderr: "sudo: a password is required".to_owned(),
        };
        assert!(!remote_sudo_password_required(&ssh_error));

        let policy_error = CiaoError::RemoteCommand {
            stage: "check remote administrator privileges".to_owned(),
            exit: 1,
            stdout: String::new(),
            stderr: "sudo: user is not allowed to run sudo".to_owned(),
        };
        assert!(!remote_sudo_password_required(&policy_error));
    }

    #[test]
    fn passwordless_sudo_instructions_name_the_target_user_and_validation_steps() {
        let transport = OpenSshTransport::new("luca@example.test").unwrap();
        let instructions = passwordless_sudo_instructions(&transport);
        assert!(instructions.contains("ssh luca@example.test"));
        assert!(instructions.contains("luca ALL=(ALL) NOPASSWD: ALL"));
        assert!(instructions.contains("sudo visudo"));
        assert!(instructions.contains("sudo visudo -c"));
        assert!(instructions.contains("explicit confirmation"));
    }

    #[test]
    fn passwordless_sudo_script_validates_before_installing_on_both_host_families() {
        let linux = passwordless_sudo_script(&HostOs::Linux, "luca").unwrap();
        assert!(linux.contains("sudo visudo -cf \"$tmp\""));
        assert!(linux.contains("/etc/sudoers.d/ciao"));
        assert!(linux.contains("sudo -n true"));

        let macos = passwordless_sudo_script(&HostOs::MacOs, "luca").unwrap();
        assert!(macos.contains("/etc/sudoers"));
        assert!(macos.contains("sudo visudo -c"));
    }

    #[test]
    fn upload_stream_reports_progress_and_preserves_bytes() {
        let input = vec![b'x'; 150_000];
        let mut reader = io::Cursor::new(input.clone());
        let mut output = Vec::new();
        let reporter = RecordingReporter::default();
        let copied = copy_upload_stream(&mut reader, &mut output, &reporter).unwrap();
        assert_eq!(copied, input.len() as u64);
        assert_eq!(output, input);
        assert!(reporter
            .messages
            .lock()
            .unwrap()
            .iter()
            .any(|message| message.contains("upload source")));
    }

    #[test]
    fn build_cache_stays_outside_release_and_user_home_layouts() {
        assert_eq!(
            build_cache_path(&HostOs::Linux, "demo").unwrap(),
            "/var/cache/ciao/demo"
        );
        assert_eq!(
            build_cache_path(&HostOs::MacOs, "demo").unwrap(),
            "/Library/Caches/Ciao/demo"
        );
    }

    #[derive(Default)]
    struct RecordingReporter {
        messages: std::sync::Mutex<Vec<String>>,
    }

    impl ProgressReporter for RecordingReporter {
        fn updated(&self, message: &str) {
            self.messages.lock().unwrap().push(message.to_owned());
        }
    }

    #[test]
    fn deploy_lock_script_reports_lock_age_without_automatic_removal() {
        let script = deploy_lock_script("/var/lib/ciao/apps", "demo", "owner").unwrap();
        assert!(script.contains("started_value=''"));
        assert!(script.contains("lock age: $age seconds"));
        assert!(script.contains("exit 73"));
        assert!(script.contains("tee \"$lock/started\""));
        assert!(script.contains("tee \"$lock/owner\""));
        assert!(script.contains("lock_ready=0"));
        assert!(script.contains("if [ \"$lock_ready\" -eq 1 ]"));
        assert!(!script.contains("21600"));
    }

    #[test]
    fn command_output_capture_isolated_from_parent_stdio() {
        let output =
            run_local_script(b"printf 'stdout-value\\n'; printf 'stderr-value\\n' >&2").unwrap();
        assert_eq!(output.status, 0);
        assert_eq!(output.stdout.trim(), "stdout-value");
        assert_eq!(output.stderr.trim(), "stderr-value");
    }

    #[test]
    fn host_config_accepts_legacy_entries_without_identity_file() {
        let mut file = NamedTempFile::new().unwrap();
        fs::write(file.path(), "[hosts.home]\nssh = \"user@server\"\n").unwrap();
        file.flush().unwrap();
        let config = Config::load(file.path()).unwrap();
        assert_eq!(config.hosts["home"].identity_file, None);
    }

    #[test]
    fn configured_identity_is_explicit_and_not_a_shell_argument() {
        let directory = tempfile::tempdir().unwrap();
        let identity = directory.path().join("id_ed25519");
        let transport = OpenSshTransport::new("user@server")
            .unwrap()
            .with_identity_file(Some(identity.clone()))
            .unwrap();
        let args = ssh_command_arguments_for_test(&transport);
        let identity_arg = identity.display().to_string();
        assert!(args
            .windows(2)
            .any(|pair| pair[0] == "-i" && pair[1] == identity_arg));
        assert!(args
            .windows(2)
            .any(|pair| pair == ["-o", "IdentitiesOnly=yes"]));
        assert!(!args.iter().any(|arg| arg.contains("ProxyCommand")));
    }

    #[test]
    fn trusted_known_host_parser_discards_scan_headers_and_relabels_key() {
        let found = "# Host server found: line 1\nserver ssh-ed25519 AAAAkey comment\nserver ecdsa-sha2-nistp256 BBBBkey";
        let keys = trusted_known_host_lines(found);
        assert_eq!(
            keys,
            vec!["ssh-ed25519 AAAAkey comment", "ecdsa-sha2-nistp256 BBBBkey",]
        );
        assert_eq!(
            format!("ts.example {}", keys[0]),
            "ts.example ssh-ed25519 AAAAkey comment"
        );
    }

    #[test]
    fn detection_is_deterministic() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("go.mod"), "module example.com/app\n").unwrap();
        fs::write(
            directory.path().join("ciao.toml"),
            "[app]\nname = \"go-demo\"\n",
        )
        .unwrap();
        let plan = detect_project(directory.path()).unwrap();
        assert_eq!(plan.runtime, Runtime::Go);
        assert_eq!(plan.run_command.as_deref(), Some("./app"));
    }

    #[test]
    fn detects_flask_service_with_a_safe_python_environment() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(
            directory.path().join("requirements.txt"),
            "Flask==3.1.0\ngunicorn==23.0.0\n",
        )
        .unwrap();
        fs::write(
            directory.path().join("ciao.toml"),
            "[app]\nname = \"flask-demo\"\n",
        )
        .unwrap();
        fs::write(directory.path().join("app.py"), "from flask import Flask\n").unwrap();
        let plan = detect_project(directory.path()).unwrap();
        assert_eq!(plan.runtime, Runtime::Python);
        assert_eq!(plan.app_type, AppType::Service);
        assert_eq!(plan.port, Some(8000));
        assert!(plan
            .install_command
            .as_deref()
            .unwrap()
            .contains("python3 -m venv .venv"));
        assert_eq!(
            plan.run_command.as_deref(),
            Some(".venv/bin/gunicorn --bind \"$HOST:$PORT\" app:app")
        );
    }

    #[test]
    fn detects_backend_and_frontend_components_without_project_config() {
        let directory = tempfile::tempdir().unwrap();
        let backend = directory.path().join("backend");
        let frontend = directory.path().join("frontend");
        fs::create_dir_all(&backend).unwrap();
        fs::create_dir_all(&frontend).unwrap();
        fs::write(
            directory.path().join("ciao.toml"),
            "[app]\nname = \"full-stack\"\n",
        )
        .unwrap();
        fs::write(backend.join("requirements.txt"), "flask\n").unwrap();
        fs::write(backend.join("app.py"), "from flask import Flask\n").unwrap();
        fs::write(
            frontend.join("package.json"),
            r#"{"scripts":{"build":"next build","start":"next start"},"dependencies":{"next":"^15.0.0","react":"^19.0.0"}}"#,
        )
        .unwrap();
        let components = detect_project_components(directory.path()).unwrap();
        assert_eq!(components.len(), 2);
        assert_eq!(components[0].role, ProjectRole::Backend);
        assert_eq!(components[0].plan.runtime, Runtime::Python);
        assert_eq!(components[1].role, ProjectRole::Frontend);
        assert_eq!(components[1].plan.runtime, Runtime::Node);
        assert_eq!(components[1].name, "full-stack-frontend");
    }

    #[test]
    fn custom_config_overrides_detected_plan() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("package.json"), "{}\n").unwrap();
        fs::write(
            directory.path().join("ciao.toml"),
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
    fn astro_is_detected_as_static_with_a_build_output_before_dist_exists() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(
            directory.path().join("package.json"),
            r#"{"scripts":{"build":"astro build"},"devDependencies":{"astro":"^5.0.0"}}"#,
        )
        .unwrap();
        fs::write(
            directory.path().join("ciao.toml"),
            "[app]\nname = \"astro-site\"\n",
        )
        .unwrap();
        fs::write(
            directory.path().join("astro.config.mjs"),
            "export default {};\n",
        )
        .unwrap();
        let plan = detect_project(directory.path()).unwrap();
        assert_eq!(plan.runtime, Runtime::Astro);
        assert_eq!(plan.app_type, AppType::Static);
        assert_eq!(plan.static_directory.as_deref(), Some("dist"));
        assert_eq!(plan.build_command.as_deref(), Some("npm run build"));
        assert!(plan.run_command.is_none());
    }

    #[test]
    fn astro_server_output_remains_a_service() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(
            directory.path().join("package.json"),
            r#"{"scripts":{"build":"astro build","start":"node ./dist/server/entry.mjs"},"dependencies":{"astro":"^5.0.0"}}"#,
        )
        .unwrap();
        fs::write(
            directory.path().join("ciao.toml"),
            "[app]\nname = \"astro-server\"\n",
        )
        .unwrap();
        fs::write(
            directory.path().join("astro.config.mjs"),
            "export default { output: 'server' };\n",
        )
        .unwrap();
        let plan = detect_project(directory.path()).unwrap();
        assert_eq!(plan.runtime, Runtime::Astro);
        assert_eq!(plan.app_type, AppType::Service);
        assert_eq!(plan.run_command.as_deref(), Some("npm start"));
    }

    #[test]
    fn local_dev_keeps_name_and_mapping_stable_while_avoiding_collisions() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("package.json"), "{}\n").unwrap();
        fs::write(
            directory.path().join("ciao.toml"),
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
    fn local_run_serves_static_output_without_caddy() {
        let directory = tempfile::tempdir().unwrap();
        let plan = LocalDevPlan {
            name: "site".to_owned(),
            domain: "site.ciao".to_owned(),
            port: 41001,
            source: directory.path().to_path_buf(),
            runtime: Runtime::Astro,
            app_type: AppType::Static,
            install_command: Some("npm ci".to_owned()),
            build_command: Some("npm run build".to_owned()),
            run_command: None,
            static_root: Some(directory.path().join("dist")),
        };
        let script = String::from_utf8(local_run_script(&plan).unwrap()).unwrap();
        assert!(script.contains("npm ci"));
        assert!(script.contains("npm run build"));
        assert!(script.contains("python3 -m http.server"));
        assert!(!script.contains("caddy"));
    }

    #[test]
    fn local_setup_script_installs_native_dependencies_without_hosts_entries() {
        let script = local_setup_script().unwrap();
        assert!(script.contains("dnsmasq"));
        assert!(script.contains("Caddy"));
        assert!(!script.contains("/etc/hosts"));
        assert!(script.contains("address=/.ciao/127.0.0.1"));
        if cfg!(target_os = "macos") {
            assert!(script.contains("dev.ciao.local-loopback"));
            assert!(script.contains("/Library/LaunchDaemons"));
        }
    }

    #[test]
    fn local_privileged_script_uses_interactive_sudo_when_cache_is_missing() {
        let script = local_privileged_script_with_sudo("sudo -n install -d /etc/ciao", "sudo");
        assert!(script.starts_with("set -eu\nsudo -v </dev/tty"));
        assert!(script.contains("sudo install -d /etc/ciao"));
        assert!(!script.contains("sudo -n install"));
    }

    #[test]
    fn remote_resolver_setup_does_not_install_a_local_proxy() {
        let script = local_resolver_setup_script().unwrap();
        assert!(script.contains("dnsmasq"));
        assert!(!script.contains("Caddy"));
        assert!(!script.contains("caddy"));
        assert!(!script.contains("/etc/hosts"));
    }

    #[test]
    fn cloudflare_tunnel_ids_are_read_only_from_uuid_tokens() {
        assert_eq!(
            uuid_from_text("Created tunnel ciao-demo with id 123e4567-e89b-12d3-a456-426614174000"),
            Some("123e4567-e89b-12d3-a456-426614174000".to_owned())
        );
        assert!(uuid_from_text("not a tunnel id").is_none());
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
            directory.path().join("ciao.toml"),
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
        assert!(systemd_unit("bad;rm", "ciao-bad", "/tmp", "./app").is_err());
        let plist = launchd_plist("good", "/tmp", "./app", "luca").unwrap();
        assert!(plist.contains("<key>UserName</key><string>luca</string>"));
        assert!(plist.contains("<key>PATH</key>"));
    }

    #[test]
    fn macos_service_labels_do_not_duplicate_the_ciao_prefix() {
        let label = "ciao-demo";
        let normalized = label
            .trim_end_matches(".service")
            .replace("-candidate", ".candidate")
            .trim_start_matches("ciao-")
            .to_owned();
        assert_eq!(normalized, "demo");
        assert_eq!(format!("dev.ciao.{normalized}"), "dev.ciao.demo");
    }

    #[test]
    fn shell_script_uses_fixed_cwd_quote() {
        let script = command_script("echo ok", "/tmp/a'b").unwrap();
        assert!(String::from_utf8(script).unwrap().contains("'\\''"));
    }

    #[test]
    fn environment_file_line_survives_shell_parsing() {
        let value = "a'b $HOME \"quoted\" \\ slash `command`";
        let line = env_file_line("SECRET", value);
        let script = format!("set -eu\n{line}\nprintf '%s' \"$SECRET\"\n");
        let output = run_local_script(script.as_bytes()).unwrap();
        assert_eq!(output.status, 0);
        assert_eq!(output.stdout, value);
    }

    #[test]
    fn activation_replaces_current_with_a_root_owned_symlink_swap() {
        let script = switch_current_script(
            &HostOs::Linux,
            APP_ROOT,
            "demo",
            "/var/lib/ciao/apps/demo/releases/r1",
        );
        assert!(script.contains(".current-$$"));
        assert!(script.contains("mv -f"));
        assert!(script.contains("chown -h root:root"));
        assert!(!script.contains("ln -sfn"));
    }

    #[test]
    fn truncate_preserves_utf8_at_the_limit() {
        let value = format!("{}é", "a".repeat(16_383));
        let truncated = truncate(&value);
        assert!(truncated.starts_with(&"a".repeat(16_383)));
        assert!(truncated.contains("output truncated"));
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
            "ciao-demo",
            "/var/lib/ciao/apps/demo",
        )
        .unwrap()
        .unwrap();
        assert!(node.contains("apt-get install -y nodejs npm"));
        assert!(!node.contains("caddy"));

        let bun = runtime_init_script(
            &HostOs::Linux,
            &Runtime::Bun,
            "ciao-demo",
            "/var/lib/ciao/apps/demo",
        )
        .unwrap()
        .unwrap();
        assert!(bun.contains("https://bun.sh/install"));
        assert!(bun.contains("BUN_INSTALL"));
    }

    #[test]
    fn github_remote_detection_supports_common_urls_and_dots() {
        assert_eq!(
            parse_github_remote("https://github.com/acme/my.app.git")
                .unwrap()
                .unwrap()
                .full_name(),
            "acme/my.app"
        );
        assert_eq!(
            parse_github_remote("git@github.com:acme/my-app").unwrap(),
            Some(GitHubRepoRef {
                owner: "acme".to_owned(),
                repo: "my-app".to_owned(),
            })
        );
        assert!(parse_github_remote("https://gitlab.com/acme/app")
            .unwrap()
            .is_none());
    }

    #[test]
    fn tailscale_federated_identity_description_has_safe_characters() {
        let repository = GitHubRepository {
            owner: "acme".to_owned(),
            repo: "demo-site".to_owned(),
            owner_id: "42".to_owned(),
            repository_id: "123".to_owned(),
            default_branch: "main".to_owned(),
            remote: "https://github.com/acme/demo-site".to_owned(),
            private: false,
        };
        let request = tailscale_federated_identity_request(&repository).unwrap();
        assert_eq!(request["description"], "Ciao GitHub CI - acme-demo-site");
        assert!(!request["description"].as_str().unwrap().contains('/'));
        assert_eq!(
            request["subject"],
            "repo:acme@42/demo-site@123:ref:refs/heads/main"
        );
    }

    #[test]
    fn ci_environment_requires_only_safe_target_values() {
        let mut env = BTreeMap::new();
        env.insert("CIAO_HOST".to_owned(), "target.corp".to_owned());
        env.insert("CIAO_USER".to_owned(), "deploy".to_owned());
        env.insert("CIAO_APP".to_owned(), "my-app".to_owned());
        let target = ci_target_from_env(&env).unwrap();
        assert_eq!(target.host, "target.corp");
        env.insert("CIAO_HOST".to_owned(), "target;id".to_owned());
        assert!(ci_target_from_env(&env).is_err());
    }

    #[test]
    fn tailscale_policy_patch_preserves_existing_rules_and_is_idempotent() {
        let policy = serde_json::json!({
            "acls": [{"action": "accept", "src": ["autogroup:members"], "dst": ["*:*"]}],
            "grants": []
        });
        let first = tailscale_policy_patch(&policy, "100.64.0.7").unwrap();
        let second = tailscale_policy_patch(&first, "100.64.0.7").unwrap();
        assert_eq!(first, second);
        assert_eq!(first["acls"], policy["acls"]);
        assert_eq!(
            first["tagOwners"]["tag:ciao-ci"],
            serde_json::json!(["autogroup:admin"])
        );
        assert_eq!(first["grants"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn tailscale_preview_query_escapes_ip_port() {
        assert_eq!(
            percent_encode_query_component("100.64.0.7:22"),
            "100.64.0.7%3A22"
        );
    }

    #[test]
    fn tailscale_browser_url_parser_accepts_only_login_origin() {
        assert_eq!(
            tailscale_auth_url_from_output(
                "To authenticate, visit https://login.tailscale.com/a/example?redirect=1."
            ),
            Some("https://login.tailscale.com/a/example?redirect=1".to_owned())
        );
        assert!(tailscale_auth_url_from_output("https://evil.example/login").is_none());
    }

    #[test]
    fn guided_tailscale_auth_records_a_pid_for_cleanup() {
        let command = tailscale_auth_start_command();
        let script = String::from_utf8(command.stdin.unwrap()).unwrap();
        assert!(script.contains("printf '%s\\n' \"$!\" > \"$state/pid\""));
        assert!(script.contains("state=\"/tmp/ciao-tailscale-auth-$(id -u)\""));
    }

    #[test]
    fn tailscale_status_parser_ignores_transport_text() {
        let status = r#"remote banner
{"BackendState":"Running","TailscaleIPs":["100.64.0.7"]}
remote footer"#;
        let parsed = tailscale_status_value(status, "").unwrap();
        assert_eq!(parsed["BackendState"], "Running");
        assert_eq!(parsed["TailscaleIPs"][0], "100.64.0.7");
    }

    #[test]
    fn json_parser_keeps_top_level_object() {
        let parsed =
            parse_json_object(r#"{"BackendState":"Running","Self":{"Online":true}}"#).unwrap();
        assert_eq!(parsed["BackendState"], "Running");
    }

    #[test]
    fn tailscale_preferred_address_uses_magic_dns_before_ip() {
        let target = TailscaleTarget {
            hostname: Some("server.example.ts.net".to_owned()),
            ipv4: Some("100.64.0.7".to_owned()),
            online: true,
        };
        assert_eq!(target.preferred_address().unwrap(), "server.example.ts.net");
    }

    #[test]
    fn tailscale_target_detection_recognizes_tailnet_addresses() {
        assert!(ssh_target_uses_tailscale("deploy@100.121.27.41"));
        assert!(ssh_target_uses_tailscale("deploy@server.example.ts.net"));
        assert!(!ssh_target_uses_tailscale("deploy@[fd7a:115c:a1e0::1]"));
        assert!(!ssh_target_uses_tailscale("deploy@192.168.1.20"));
    }

    #[test]
    fn generated_workflow_is_pinned_and_strict() {
        let workflow = render_github_workflow(&workflow_spec_for(
            "main",
            "0123456789012345678901234567890123456789",
        ))
        .unwrap();
        assert!(workflow.contains("id-token: write"));
        assert!(workflow.contains("tailscale/github-action@v4"));
        assert!(workflow.contains("ping: ${{ vars.CIAO_HOST }}"));
        assert!(workflow.contains("dtolnay/rust-toolchain@stable"));
        assert!(workflow.contains("StrictHostKeyChecking yes"));
        assert!(workflow.contains("ciao deploy --ci"));
        assert!(workflow.contains("ref: '0123456789012345678901234567890123456789'"));
        assert!(!workflow.contains("ref: 'latest'"));
    }
}
