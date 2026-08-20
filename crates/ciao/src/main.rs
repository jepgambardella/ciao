use ciao_core::*;
use clap::{Args, Parser, Subcommand};
use indicatif::{ProgressBar, ProgressStyle};
use serde::Serialize;
use serde_json::json;
use std::collections::BTreeMap;
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::sync::Mutex;
use std::time::Duration;

#[derive(Debug, Parser)]
#[command(name = "ciao", version, about = "Ship apps. Skip the ops.")]
struct Cli {
    /// Emit machine-readable JSON where supported.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Host {
        #[command(subcommand)]
        command: HostCommand,
    },
    /// Detect the project in a directory and print its deployment plan.
    Inspect {
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Deploy the current project to a configured host.
    Deploy(DeployArgs),
    /// Run the current project behind the local *.ciao resolver and Caddy.
    Dev(DevArgs),
    /// Run the detected project on loopback for a temporary local test.
    Run(RunArgs),
    Status(AppArgs),
    /// List applications managed on a host.
    Apps {
        host: String,
    },
    /// List immutable releases for an application.
    Releases {
        host: String,
        app: String,
    },
    Logs(LogsArgs),
    Restart(AppArgs),
    Start(AppArgs),
    Stop(AppArgs),
    Rollback(AppArgs),
    /// Open a temporary local dashboard backed by the shared core.
    Ui {
        host: String,
        #[arg(long, default_value_t = 7843)]
        port: u16,
    },
    Env {
        #[command(subcommand)]
        command: EnvCommand,
    },
    Domain {
        #[command(subcommand)]
        command: DomainCommand,
    },
    Github {
        #[command(subcommand)]
        command: GithubCommand,
    },
    /// Run the local stdio MCP server.
    Mcp,
}

#[derive(Debug, Subcommand)]
enum HostCommand {
    Add {
        name: String,
        ssh: String,
        /// Create and install a local Ed25519 key when passwordless SSH is not ready.
        #[arg(long)]
        setup_key: bool,
        /// Never prompt; fail with the exact SSH bootstrap command instead.
        #[arg(long)]
        non_interactive: bool,
    },
    List,
    Inspect {
        name: String,
    },
    /// Install Ciao's remote prerequisites and configure Caddy.
    Init {
        name: String,
    },
}

#[derive(Debug, Args)]
struct DeployArgs {
    host: String,
    #[arg(long)]
    domain: Option<String>,
    #[arg(long, default_value = ".")]
    path: PathBuf,
    #[arg(long)]
    dry_run: bool,
    /// Run without the local Ciao config, using CIAO_HOST/CIAO_USER/CIAO_APP.
    #[arg(long)]
    ci: bool,
}

#[derive(Debug, Subcommand)]
enum GithubCommand {
    /// Configure GitHub Actions, Tailscale OIDC and a dedicated SSH key.
    Setup(GithubSetupArgs),
    Status,
    /// Remove only the resources owned by the local Ciao link.
    Unlink(GithubUnlinkArgs),
    /// Re-render the generated workflow from the saved non-secret link state.
    Regenerate(GithubRegenerateArgs),
}

#[derive(Debug, Args)]
struct GithubSetupArgs {
    #[arg(long)]
    host: String,
    #[arg(long, default_value = ".")]
    path: PathBuf,
    #[arg(long)]
    app: Option<String>,
    #[arg(long)]
    branch: Option<String>,
    /// Read a temporary Tailscale admin token from stdin instead of prompting.
    #[arg(long)]
    tailscale_token_stdin: bool,
    /// Read a temporary read-only token for the private Ciao source repository from stdin.
    #[arg(long)]
    ciao_github_token_stdin: bool,
    /// Reuse an already-created federated identity without an admin token.
    #[arg(long)]
    tailscale_client_id: Option<String>,
    #[arg(long)]
    tailscale_audience: Option<String>,
    /// Accept replacing the generated workflow without prompting.
    #[arg(long)]
    yes: bool,
}

#[derive(Debug, Args)]
struct GithubRegenerateArgs {
    #[arg(long, default_value = ".")]
    path: PathBuf,
    #[arg(long)]
    yes: bool,
}

#[derive(Debug, Args)]
struct GithubUnlinkArgs {
    #[arg(long, default_value = ".")]
    path: PathBuf,
    #[arg(long)]
    app: Option<String>,
    /// Required because this removes GitHub resources and the target key.
    #[arg(long)]
    yes: bool,
    /// Read the temporary Tailscale token from stdin to remove the federated identity too.
    #[arg(long)]
    tailscale_token_stdin: bool,
}

#[derive(Debug, Args)]
struct DevArgs {
    #[arg(default_value = ".")]
    path: PathBuf,
    #[arg(long)]
    name: Option<String>,
    #[arg(long)]
    dry_run: bool,
}

#[derive(Debug, Args)]
struct RunArgs {
    #[arg(default_value = ".")]
    path: PathBuf,
    /// Use a fixed loopback port instead of the automatic Ciao port.
    #[arg(long)]
    port: Option<u16>,
}

#[derive(Debug, Args)]
struct AppArgs {
    host: String,
    app: String,
}

#[derive(Debug, Args)]
struct LogsArgs {
    host: String,
    app: String,
    #[arg(long)]
    follow: bool,
    #[arg(long)]
    since: Option<String>,
}

#[derive(Debug, Subcommand)]
enum EnvCommand {
    Set {
        host: String,
        app: String,
        key: String,
    },
    Unset {
        host: String,
        app: String,
        key: String,
    },
}

#[derive(Debug, Subcommand)]
enum DomainCommand {
    Add {
        host: String,
        app: String,
        domain: String,
    },
    Remove {
        host: String,
        app: String,
        domain: String,
    },
}

struct TerminalProgress {
    current: Mutex<Option<ProgressBar>>,
}

impl TerminalProgress {
    fn new() -> Self {
        Self {
            current: Mutex::new(None),
        }
    }

    fn finish(&self, step: &str, success: bool) {
        if let Ok(mut current) = self.current.lock() {
            if let Some(bar) = current.take() {
                bar.finish_with_message(format!("{} {step}", if success { "✓" } else { "✗" }));
            }
        }
    }
}

impl ProgressReporter for TerminalProgress {
    fn started(&self, step: &str) {
        if let Ok(mut current) = self.current.lock() {
            if let Some(previous) = current.take() {
                previous.finish_and_clear();
            }
            let bar = ProgressBar::new_spinner();
            bar.set_style(
                ProgressStyle::with_template("{spinner:.green} {msg}")
                    .expect("static progress template"),
            );
            bar.enable_steady_tick(Duration::from_millis(90));
            bar.set_message(step.to_owned());
            *current = Some(bar);
        }
    }

    fn finished(&self, step: &str) {
        self.finish(step, true);
    }

    fn updated(&self, message: &str) {
        if let Ok(current) = self.current.lock() {
            if let Some(bar) = current.as_ref() {
                bar.set_message(message.to_owned());
            }
        }
    }

    fn failed(&self, step: &str) {
        self.finish(step, false);
    }
}

fn main() {
    if let Err(error) = ctrlc::set_handler(|| {
        ciao_core::request_cancellation();
    }) {
        eprintln!("✗ could not install the Ctrl-C handler: {error}");
        std::process::exit(1);
    }
    ciao_core::reset_cancellation();
    let cli = Cli::parse();
    let json_output = cli.json;
    if let Err(error) = run(cli) {
        if json_output {
            println!("{}", json!({"error": error.to_string(), "exit_code": 1}));
        } else {
            eprintln!("✗ {error}");
        }
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Host { command } => host_command(command, cli.json),
        Command::Inspect { path } => {
            let plan = detect_project(&path.canonicalize()?)?;
            output(&plan, cli.json, || {
                format!(
                    "✓ project detected: {}\n  app: {}\n  command: {}",
                    plan.runtime,
                    plan.name,
                    plan.run_command.as_deref().unwrap_or("static files")
                )
            });
            Ok(())
        }
        Command::Deploy(args) => {
            let path = args.path.canonicalize()?;
            let mut plan = detect_project(&path)?;
            let (transport, app_override) = if args.ci {
                let target =
                    ci_target_from_env(&std::env::vars().collect::<BTreeMap<String, String>>())?;
                validate_ssh_target(&format!("{}@{}", target.user, args.host))?;
                (
                    OpenSshTransport::new(format!("{}@{}", target.user, args.host))?,
                    Some(target.app),
                )
            } else {
                let config = load_config()?;
                let host = configured_host(&config, &args.host)?;
                (
                    OpenSshTransport::new(host.ssh)?
                        .with_identity_file(host.identity_file.clone())?,
                    None,
                )
            };
            if let Some(app) = app_override {
                validate_identifier("app name", &app)?;
                plan.name = app;
            }
            let progress = TerminalProgress::new();
            let interactive_output =
                !cli.json && !args.ci && io::stdin().is_terminal() && io::stderr().is_terminal();
            if !args.dry_run && args.ci {
                require_noninteractive_sudo(&transport, "CI deployment")?;
            }
            if interactive_output && !args.dry_run {
                offer_passwordless_sudo_setup(&transport)?;
            }
            let deploy = || {
                deploy_with_mode(
                    &transport,
                    &path,
                    &plan,
                    args.domain.as_deref(),
                    args.dry_run,
                    if !interactive_output {
                        &NoopProgressReporter
                    } else {
                        &progress
                    },
                    if interactive_output {
                        DeployHostMode::Interactive
                    } else {
                        DeployHostMode::NonInteractive
                    },
                )
            };
            let result = match deploy() {
                Err(error) if interactive_output && is_deploy_lock_error(&error) => {
                    recover_interrupted_deploy(&transport, &plan.name)?;
                    deploy()
                }
                result => result,
            }
            .map_err(|error| actionable_deploy_error(error, &args.host))?;
            if !cli.json && !args.ci && args.dry_run {
                eprintln!("✓ dry-run complete");
            }
            output(&result, cli.json, || result.message.clone());
            if !args.ci && !args.dry_run && interactive_output {
                if let Err(error) = offer_remote_local_domain(&transport, &result.app) {
                    eprintln!(
                        "! deployment succeeded, but local .ciao routing was not configured: {error}"
                    );
                }
                if let Some(domain) = args.domain.as_deref() {
                    if let Err(error) = setup_cloudflare_tunnel(&transport, &result.app, domain) {
                        eprintln!(
                            "! deployment succeeded, but Cloudflare Tunnel was not configured: {error}"
                        );
                    }
                }
                if let Err(error) = offer_github_setup(&path, &args.host, &result.app, cli.json) {
                    eprintln!(
                        "! deployment succeeded, but GitHub auto-deploy was not configured: {error}"
                    );
                }
            }
            Ok(())
        }
        Command::Dev(args) => local_dev_command(args, cli.json),
        Command::Run(args) => local_run_command(args, cli.json),
        Command::Status(args) => {
            let transport =
                authorized_transport_for(&args.host, cli.json, "reading application status")?;
            let result = app_status(&transport, &args.app)?;
            output(&result, cli.json, || result.message.clone());
            Ok(())
        }
        Command::Apps { host } => {
            let transport = authorized_transport_for(&host, cli.json, "listing applications")?;
            let result = list_apps(&transport)?;
            output(&result, cli.json, || {
                if result.is_empty() {
                    "No Ciao applications found.".to_owned()
                } else {
                    result
                        .iter()
                        .map(|app| {
                            format!(
                                "{}\t{}\t{}",
                                app.app,
                                app.status,
                                app.release.as_deref().unwrap_or("-")
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                }
            });
            Ok(())
        }
        Command::Releases { host, app } => {
            let transport = authorized_transport_for(&host, cli.json, "listing releases")?;
            let result = list_releases(&transport, &app)?;
            output(&result, cli.json, || {
                result
                    .iter()
                    .map(|release| {
                        format!(
                            "{}\t{}",
                            release.release,
                            if release.active { "active" } else { "" }
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            });
            Ok(())
        }
        Command::Logs(args) => {
            let transport =
                authorized_transport_for(&args.host, cli.json, "reading application logs")?;
            let result = app_logs(&transport, &args.app, args.follow, args.since.as_deref())?;
            if cli.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&result).map_err(ser_error)?
                );
            } else {
                print!("{}", result.logs);
            }
            Ok(())
        }
        Command::Restart(args) => lifecycle(&args, ciao_core::LifecycleAction::Restart, cli.json),
        Command::Start(args) => lifecycle(&args, ciao_core::LifecycleAction::Start, cli.json),
        Command::Stop(args) => lifecycle(&args, ciao_core::LifecycleAction::Stop, cli.json),
        Command::Rollback(args) => {
            let transport =
                authorized_transport_for(&args.host, cli.json, "rolling back the application")?;
            let result = rollback(&transport, &args.app)?;
            output(&result, cli.json, || result.message.clone());
            Ok(())
        }
        Command::Ui { host, port } => run_ui(&host, port),
        Command::Env { command } => match command {
            EnvCommand::Set { host, app, key } => {
                let value = read_secret_value()?;
                let transport =
                    authorized_transport_for(&host, cli.json, "setting an environment variable")?;
                set_env(&transport, &app, &key, &value)?;
                if cli.json {
                    println!("{}", json!({"app": app, "key": key, "changed": true}));
                } else {
                    println!("✓ environment updated for {app}: {key}");
                }
                Ok(())
            }
            EnvCommand::Unset { host, app, key } => {
                let transport =
                    authorized_transport_for(&host, cli.json, "removing an environment variable")?;
                unset_env(&transport, &app, &key)?;
                if cli.json {
                    println!("{}", json!({"app": app, "key": key, "changed": true}));
                } else {
                    println!("✓ environment key removed for {app}: {key}");
                }
                Ok(())
            }
        },
        Command::Domain { command } => match command {
            DomainCommand::Add { host, app, domain } => {
                let transport = authorized_transport_for(&host, cli.json, "configuring a domain")?;
                add_domain(&transport, &app, &domain)?;
                if cli.json {
                    println!("{}", json!({"app": app, "domain": domain, "changed": true}));
                } else {
                    println!("✓ domain configured: {domain}");
                }
                Ok(())
            }
            DomainCommand::Remove { host, app, domain } => {
                let transport = authorized_transport_for(&host, cli.json, "removing a domain")?;
                remove_domain(&transport, &app, &domain)?;
                if cli.json {
                    println!("{}", json!({"app": app, "domain": domain, "changed": true}));
                } else {
                    println!("✓ domain removed: {domain}");
                }
                Ok(())
            }
        },
        Command::Github { command } => github_command(command, cli.json),
        Command::Mcp => mcp_stdio(),
    }
}

fn local_dev_command(args: DevArgs, json_output: bool) -> Result<()> {
    let source = args.path.canonicalize()?;
    let detected = detect_project(&source)?;
    let config_path = default_config_path();
    let mut config = Config::load(&config_path)?;
    let plan = local_dev_plan(&source, &detected, args.name.as_deref(), &config.local)?;
    if args.dry_run {
        let result = json!({
            "project": plan.name,
            "runtime": plan.runtime,
            "domain": plan.domain,
            "port": plan.port,
            "source": plan.source,
            "app_type": plan.app_type,
            "command": plan.run_command,
            "dry_run": true,
        });
        output(&result, json_output, || {
            format!(
                "Detected runtime: {}\nWould: configure resolver, Caddy and run http://{} on 127.0.0.1:{}",
                plan.runtime, plan.domain, plan.port
            )
        });
        return Ok(());
    }

    let setup = local_setup()?;
    let paths = write_local_caddy_fragment(&plan)?;
    if let Err(error) = reload_local_caddy(&paths) {
        let _ = remove_local_caddy_fragment(&plan.name).and_then(|_| reload_local_caddy(&paths));
        return Err(error);
    }
    config.local.projects.insert(
        plan.name.clone(),
        LocalProject {
            domain: plan.domain.clone(),
            port: plan.port,
            source: plan.source.display().to_string(),
            app_type: Some(plan.app_type.clone()),
            static_root: plan
                .static_root
                .as_ref()
                .map(|path| path.display().to_string()),
        },
    );
    if let Err(error) = config.save(&config_path) {
        let _ = remove_local_caddy_fragment(&plan.name).and_then(|_| reload_local_caddy(&paths));
        return Err(error);
    }
    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "project": plan.name,
                "runtime": plan.runtime,
                "domain": plan.domain,
                "port": plan.port,
                "resolver": setup.resolver,
                "proxy": setup.proxy,
                "dependencies": setup.dependencies,
                "url": format!("http://{}", plan.domain),
                "ready": true,
            }))
            .map_err(ser_error)?
        );
    } else {
        println!("✓ project detected: {}", plan.runtime);
        println!("✓ local domain: {}", plan.domain);
        println!("✓ internal port: {}", plan.port);
        println!("✓ resolver: .ciao active");
        println!();
        println!("http://{}", plan.domain);
        println!();
        println!("Ready.");
    }

    if plan.app_type == AppType::Static {
        return Ok(());
    }
    let status = match run_local_dev(&plan) {
        Ok(status) => status,
        Err(error) => {
            let _ =
                remove_local_caddy_fragment(&plan.name).and_then(|_| reload_local_caddy(&paths));
            return Err(error);
        }
    };
    let cleanup_result =
        remove_local_caddy_fragment(&plan.name).and_then(|_| reload_local_caddy(&paths));
    if let Err(error) = cleanup_result {
        return Err(CiaoError::LocalCommand {
            stage: "clean up local Caddy route".to_owned(),
            exit: 1,
            stdout: String::new(),
            stderr: error.to_string(),
        });
    }
    if status != 0 {
        return Err(CiaoError::LocalCommand {
            stage: "run local development process".to_owned(),
            exit: status,
            stdout: "process output was streamed to the terminal".to_owned(),
            stderr: String::new(),
        });
    }
    Ok(())
}

fn local_run_command(args: RunArgs, json_output: bool) -> Result<()> {
    let source = args.path.canonicalize()?;
    let mut detected = detect_project(&source)?;
    if let Some(port) = args.port {
        detected.local_port = Some(port);
        detected.port_explicit = true;
    }
    let plan = local_dev_plan(&source, &detected, None, &LocalConfig::default())?;
    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "project": plan.name,
                "runtime": plan.runtime,
                "app_type": plan.app_type,
                "source": plan.source,
                "port": plan.port,
                "url": format!("http://127.0.0.1:{}", plan.port),
                "temporary": true,
            }))
            .map_err(ser_error)?
        );
    } else {
        println!("✓ project detected: {}", plan.runtime);
        println!("✓ temporary local server: http://127.0.0.1:{}", plan.port);
        println!("  Press Ctrl-C to stop.");
        println!();
    }
    let progress = TerminalProgress::new();
    let status = run_local_project_with_reporter(&plan, &progress)?;
    if status != 0 {
        return Err(CiaoError::LocalCommand {
            stage: "run temporary local project".to_owned(),
            exit: status,
            stdout: "process output was streamed to the terminal".to_owned(),
            stderr: String::new(),
        });
    }
    Ok(())
}

fn offer_remote_local_domain(transport: &OpenSshTransport, app: &str) -> Result<()> {
    ensure_local_tailscale()?;
    if local_tailscale_target().is_err() {
        let url = start_local_tailscale_auth()?.ok_or_else(|| {
            CiaoError::Config(
                "local Tailscale authentication finished without a connected device".to_owned(),
            )
        })?;
        eprintln!("Ciao is opening the local Tailscale sign-in page in your browser.");
        if let Err(error) = open_tailscale_auth_url(&url) {
            eprintln!("{error}");
        }
        eprintln!("Complete sign-in. Ciao will continue automatically.");
        wait_for_local_tailscale_auth(Duration::from_secs(300))?;
    }
    let platform = transport.inspect()?;
    let _ = ensure_tailscale_target(transport, &platform.os)?;
    let remote_tailscale = if let Some(url) = start_tailscale_auth(transport)? {
        eprintln!("Ciao is opening the target Tailscale sign-in page in your browser.");
        if let Err(error) = open_tailscale_auth_url(&url) {
            eprintln!("{error}");
        }
        eprintln!("Complete sign-in. Ciao will continue automatically.");
        wait_for_tailscale_auth(transport, Duration::from_secs(300))?
    } else {
        tailscale_target(transport)?
    };
    let address = remote_tailscale.ipv4.clone().ok_or_else(|| {
        CiaoError::Config(
            "Tailscale is connected, but the target has no IPv4 address for local .ciao DNS"
                .to_owned(),
        )
    })?;
    let result = configure_local_remote_domain(app, &address)?;
    println!(
        "✓ local route: http://{}.ciao → {} via Tailscale",
        app, address
    );
    if !result.dependencies.is_empty() {
        println!("  resolver ready (no local Caddy)");
    }
    Ok(())
}

fn setup_cloudflare_tunnel(transport: &OpenSshTransport, app: &str, domain: &str) -> Result<()> {
    let platform = transport.inspect()?;
    let result = cloudflare_tunnel_setup(transport, &platform.os, app, domain)?;
    configure_domain_for_cloudflare(transport, app, domain)?;
    println!("✓ public domain: https://{}", result.domain);
    println!("  {}", result.message);
    Ok(())
}

fn load_config() -> Result<Config> {
    Config::load(&default_config_path())
}

fn configured_host(config: &Config, name: &str) -> Result<Host> {
    config.hosts.get(name).cloned().ok_or_else(|| {
        CiaoError::Config(format!(
            "host `{name}` is not configured; run `ciao host add`"
        ))
    })
}

fn github_command(command: GithubCommand, json_output: bool) -> Result<()> {
    match command {
        GithubCommand::Setup(args) => github_setup(args, json_output),
        GithubCommand::Status => github_status(json_output),
        GithubCommand::Unlink(args) => github_unlink(args, json_output),
        GithubCommand::Regenerate(args) => github_regenerate(args, json_output),
    }
}

fn offer_github_setup(path: &Path, host: &str, app: &str, json_output: bool) -> Result<()> {
    if json_output || !io::stdin().is_terminal() || !io::stderr().is_terminal() {
        return Ok(());
    }
    let Some(repository) = detect_github_repository(path)? else {
        return Ok(());
    };
    let config = GitHubConfig::load(&github_config_path())?;
    if config
        .links
        .contains_key(&github_link_key(&repository.full_name(), app))
    {
        return Ok(());
    }
    println!();
    println!("GitHub repository detected: {}", repository.full_name());
    print!(
        "Enable automatic deploys from `{}`? [Y/n] ",
        repository.default_branch
    );
    io::stdout().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    if matches!(answer.trim().to_ascii_lowercase().as_str(), "n" | "no") {
        return Ok(());
    }
    github_setup(
        GithubSetupArgs {
            host: host.to_owned(),
            path: path.to_path_buf(),
            app: Some(app.to_owned()),
            branch: None,
            tailscale_token_stdin: false,
            ciao_github_token_stdin: false,
            tailscale_client_id: None,
            tailscale_audience: None,
            yes: false,
        },
        false,
    )
}

fn github_setup(args: GithubSetupArgs, json_output: bool) -> Result<()> {
    let root = args.path.canonicalize()?;
    let detected = detect_github_repository(&root)?.ok_or_else(|| {
        CiaoError::Config(
            "no GitHub origin found; configure origin and rerun `ciao github setup`".to_owned(),
        )
    })?;
    github_auth_status()?;
    let mut repository = github_repository_metadata(&detected.reference())?;
    let branch = args
        .branch
        .unwrap_or_else(|| repository.default_branch.clone());
    validate_github_branch(&branch)?;
    repository.default_branch = branch.clone();
    let plan = detect_project(&root)?;
    let app = args.app.unwrap_or(plan.name);
    validate_identifier("app name", &app)?;

    let mut github = GitHubConfig::load(&github_config_path())?;
    let link_key = github_link_key(&repository.full_name(), &app);
    if github.links.contains_key(&link_key) {
        return Err(CiaoError::Config(format!(
            "GitHub auto-deploy is already configured for {} / {}; run `ciao github unlink --yes` before setting it up again",
            repository.full_name(), app
        )));
    }
    if github
        .links
        .values()
        .any(|link| link.repository == repository.full_name())
    {
        return Err(CiaoError::Config(format!(
            "repository {} already has a Ciao auto-deploy link; v1 supports one app per repository, unlink it before choosing another app",
            repository.full_name()
        )));
    }

    let ciao_version = env!("CIAO_BUILD_REVISION").to_owned();
    if ciao_version == "unknown" {
        return Err(CiaoError::Config(
            "Ciao was not built from a Git checkout; build it from the pinned Ciao repository revision before generating a workflow".to_owned(),
        ));
    }
    let workflow = render_github_workflow(&workflow_spec_for(&branch, &ciao_version))?;
    let workflow_path = github_workflow_path(&root);
    validate_workflow_writable(&workflow_path, &workflow, args.yes)?;

    let config = load_config()?;
    let host = configured_host(&config, &args.host)?;
    let ssh_user = ssh_user_from_target(&host.ssh).ok_or_else(|| {
        CiaoError::Config(
            "GitHub setup requires the configured host to use user@host SSH syntax".to_owned(),
        )
    })?;
    let transport =
        OpenSshTransport::new(host.ssh.clone())?.with_identity_file(host.identity_file.clone())?;
    require_noninteractive_sudo(&transport, "setting up GitHub/Tailscale auto-deploy")?;
    ensure_local_tailscale()?;
    if ssh_target_uses_tailscale(&host.ssh) && local_tailscale_target().is_err() {
        let url = start_local_tailscale_auth()?.ok_or_else(|| {
            CiaoError::Config(
                "local Tailscale authentication finished without a connected node".to_owned(),
            )
        })?;
        eprintln!("Ciao is opening the local Tailscale sign-in page in your browser.");
        if let Err(error) = open_tailscale_auth_url(&url) {
            eprintln!("{error}");
        }
        eprintln!("Complete sign-in in the browser; Ciao will continue automatically.");
        wait_for_local_tailscale_auth(Duration::from_secs(300))?;
    }
    let target_platform = transport.inspect()?;
    let _target_tailscale = ensure_tailscale_target(&transport, &target_platform.os)?;
    let tailscale = if let Some(url) = start_tailscale_auth(&transport)? {
        eprintln!("Tailscale is not connected on {ssh_user}@the target.");
        eprintln!("Ciao is opening the Tailscale sign-in page in your browser.");
        if let Err(error) = open_tailscale_auth_url(&url) {
            eprintln!("{error}");
        }
        eprintln!("Complete sign-in in the browser; Ciao will continue automatically.");
        wait_for_tailscale_auth(&transport, Duration::from_secs(300))?
    } else {
        tailscale_target(&transport)?
    };
    let tailscale_host = tailscale.preferred_address()?;

    let ciao_token = if args.ciao_github_token_stdin {
        Some(read_secret_value()?)
    } else {
        std::env::var("CIAO_GITHUB_TOKEN").ok()
    };
    let ciao_repository = GitHubRepoRef {
        owner: "jepgambardella".to_owned(),
        repo: "ciao".to_owned(),
    };
    let ciao_metadata = github_repository_metadata(&ciao_repository)?;
    if ciao_metadata.private
        && ciao_token.is_none()
        && repository.full_name() != ciao_metadata.full_name()
    {
        return Err(CiaoError::Config(
            "the Ciao source repository is private; set CIAO_GITHUB_TOKEN to a read-only GitHub token before setup so Actions can install the pinned CLI".to_owned(),
        ));
    }

    let (client_id, audience, identity_id) = if let (Some(client_id), Some(audience)) =
        (args.tailscale_client_id, args.tailscale_audience)
    {
        if client_id.trim().is_empty() || audience.trim().is_empty() {
            return Err(CiaoError::Config(
                "Tailscale client id and audience must both be non-empty".to_owned(),
            ));
        }
        (client_id, audience, None)
    } else {
        let token = read_tailscale_token(args.tailscale_token_stdin)?;
        ensure_tailscale_policy(&token, &tailscale_host, args.yes)?;
        let request = tailscale_federated_identity_request(&repository)?;
        let identity = tailscale_create_federated_identity(&token, &request)?;
        let client_id = identity.id.clone();
        if client_id.is_empty() {
            return Err(CiaoError::Config(
                "Tailscale did not return a federated identity client id".to_owned(),
            ));
        }
        let audience = if identity.audience.is_empty() {
            request
                .get("audience")
                .and_then(serde_json::Value::as_str)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
                .ok_or_else(|| {
                    CiaoError::Config(
                        "Tailscale did not return an audience and the identity request had none"
                            .to_owned(),
                    )
                })?
        } else {
            identity.audience
        };
        (
            client_id,
            audience,
            (!identity.id.is_empty()).then_some(identity.id),
        )
    };

    let known_hosts = capture_known_hosts(&transport, &tailscale_host)?;
    let generated_key =
        generate_ed25519_key(&format!("ciao GitHub CI {}", repository.full_name()))?;
    install_ci_public_key(&transport, &generated_key.public_key)?;
    github_set_secret(
        &repository.reference(),
        "CIAO_SSH_KEY",
        &generated_key.private_key,
    )?;
    github_set_secret(
        &repository.reference(),
        "CIAO_SSH_KNOWN_HOSTS",
        &known_hosts,
    )?;
    let source_token_configured = ciao_token.is_some();
    if let Some(token) = ciao_token {
        github_set_secret(&repository.reference(), "CIAO_GITHUB_TOKEN", &token)?;
    }
    github_set_variable(&repository.reference(), "CIAO_HOST", &tailscale_host)?;
    github_set_variable(&repository.reference(), "CIAO_USER", &ssh_user)?;
    github_set_variable(&repository.reference(), "CIAO_APP", &app)?;
    github_set_variable(&repository.reference(), "TS_OAUTH_CLIENT_ID", &client_id)?;
    github_set_variable(&repository.reference(), "TS_AUDIENCE", &audience)?;

    write_generated_workflow(&workflow_path, &workflow, args.yes)?;

    let link = GitHubDeploymentLink {
        repository: repository.full_name(),
        repository_id: repository.repository_id,
        branch,
        host: args.host,
        tailscale_host,
        ssh_user,
        federated_identity_id: identity_id,
        workflow_path: PathBuf::from(".github/workflows/ciao-deploy.yml"),
        public_key: Some(generated_key.public_key),
        ciao_version,
        source_token_configured,
    };
    github.links.insert(link_key, link.clone());
    github.save(&github_config_path())?;

    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "repository": link.repository,
                "branch": link.branch,
                "host": link.host,
                "tailscale_host": link.tailscale_host,
                "app": app,
                "workflow": link.workflow_path,
                "enabled": true,
            }))
            .map_err(ser_error)?
        );
    } else {
        println!("✓ GitHub authenticated");
        println!("✓ Tailscale target: {}", link.tailscale_host);
        println!("✓ dedicated CI SSH key installed");
        println!("✓ GitHub secrets and variables configured");
        println!("✓ workflow: {}", workflow_path.display());
        println!("\nAuto deploy enabled: push to `{}`.", link.branch);
    }
    Ok(())
}

fn github_status(json_output: bool) -> Result<()> {
    let config = GitHubConfig::load(&github_config_path())?;
    let repository = detect_github_repository(Path::new("."))?;
    let current = repository.as_ref().map(GitHubRepository::full_name);
    let links = config
        .links
        .iter()
        .filter(|(_, link)| match current.as_deref() {
            None => true,
            Some(name) => name == link.repository,
        })
        .collect::<Vec<_>>();
    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&links).map_err(ser_error)?
        );
    } else if links.is_empty() {
        println!("No Ciao GitHub link configured for this repository.");
    } else {
        for (app, link) in links {
            println!("Repository:     {}", link.repository);
            println!("Branch:         {}", link.branch);
            println!("App:            {}", app.rsplit("::").next().unwrap_or(app));
            println!("Target:         {} ({})", link.host, link.tailscale_host);
            println!("Network:        Tailscale");
            println!(
                "CI identity:    {}",
                if link.federated_identity_id.is_some() {
                    "configured"
                } else {
                    "external"
                }
            );
            println!("Workflow:       {}", link.workflow_path.display());
            println!("Auto deploy:    enabled");
        }
    }
    Ok(())
}

fn github_unlink(args: GithubUnlinkArgs, json_output: bool) -> Result<()> {
    if !args.yes {
        return Err(CiaoError::Config(
            "unlink removes GitHub secrets, the CI key and the generated workflow; rerun with --yes".to_owned(),
        ));
    }
    let root = args.path.canonicalize()?;
    let repository = github_repository_metadata(
        &detect_github_repository(&root)?
            .ok_or_else(|| CiaoError::Config("no GitHub origin found".to_owned()))?
            .reference(),
    )?;
    let plan = detect_project(&root)?;
    let app = args.app.unwrap_or(plan.name);
    let key = github_link_key(&repository.full_name(), &app);
    let mut config = GitHubConfig::load(&github_config_path())?;
    let link = config
        .links
        .get(&key)
        .cloned()
        .ok_or_else(|| CiaoError::Config(format!("no Ciao GitHub link found for {app}")))?;
    if link.federated_identity_id.is_some() && !args.tailscale_token_stdin {
        return Err(CiaoError::Config(
            "this link owns a Tailscale identity; rerun with --tailscale-token-stdin".to_owned(),
        ));
    }
    let transport = transport_for(&link.host)?;
    if let Some(identity_id) = &link.federated_identity_id {
        let token = read_tailscale_token(true)?;
        tailscale_delete_federated_identity(&token, identity_id)?;
    }
    github_delete_secret(&repository.reference(), "CIAO_SSH_KEY")?;
    github_delete_secret(&repository.reference(), "CIAO_SSH_KNOWN_HOSTS")?;
    for variable in [
        "CIAO_HOST",
        "CIAO_USER",
        "CIAO_APP",
        "TS_OAUTH_CLIENT_ID",
        "TS_AUDIENCE",
    ] {
        github_delete_variable(&repository.reference(), variable)?;
    }
    if link.source_token_configured {
        github_delete_secret(&repository.reference(), "CIAO_GITHUB_TOKEN")?;
    }
    if let Some(public_key) = &link.public_key {
        remove_ci_public_key(&transport, public_key)?;
    }
    let workflow_path = github_workflow_path(&root);
    if workflow_path.exists() {
        let expected =
            render_github_workflow(&workflow_spec_for(&link.branch, &link.ciao_version))?;
        if fs::read_to_string(&workflow_path)? == expected {
            fs::remove_file(&workflow_path)?;
        } else {
            eprintln!(
                "Ciao left a modified workflow in place: {}",
                workflow_path.display()
            );
        }
    }
    config.links.remove(&key);
    config.save(&github_config_path())?;
    if json_output {
        println!(
            "{}",
            json!({"repository": repository.full_name(), "app": app, "removed": true})
        );
    } else {
        println!("✓ GitHub link removed for {app}");
    }
    Ok(())
}

fn github_regenerate(args: GithubRegenerateArgs, json_output: bool) -> Result<()> {
    let root = args.path.canonicalize()?;
    let repository = detect_github_repository(&root)?
        .ok_or_else(|| CiaoError::Config("no GitHub origin found".to_owned()))?;
    let config = GitHubConfig::load(&github_config_path())?;
    let (_, link) = config
        .links
        .iter()
        .find(|(_, link)| link.repository == repository.full_name())
        .ok_or_else(|| {
            CiaoError::Config("no Ciao GitHub link is saved for this repository".to_owned())
        })?;
    let workflow = render_github_workflow(&workflow_spec_for(&link.branch, &link.ciao_version))?;
    let path = github_workflow_path(&root);
    write_generated_workflow(&path, &workflow, args.yes)?;
    if json_output {
        println!("{}", json!({"workflow": path, "regenerated": true}));
    } else {
        println!("✓ workflow regenerated: {}", path.display());
    }
    Ok(())
}

fn github_link_key(repository: &str, app: &str) -> String {
    format!("{repository}::{app}")
}

fn validate_github_branch(branch: &str) -> Result<()> {
    if branch.is_empty() || branch.len() > 255 || branch.contains(['\n', '\r', '\'']) {
        return Err(CiaoError::Config("GitHub branch is invalid".to_owned()));
    }
    Ok(())
}

fn write_generated_workflow(path: &Path, contents: &str, yes: bool) -> Result<()> {
    validate_workflow_writable(path, contents, yes)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, contents)?;
    Ok(())
}

fn open_tailscale_auth_url(url: &str) -> Result<()> {
    if !url.starts_with("https://login.tailscale.com/") {
        return Err(CiaoError::Config(
            "refusing to open a non-Tailscale authentication URL".to_owned(),
        ));
    }
    let launcher = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(target_os = "linux") {
        "xdg-open"
    } else {
        return Err(CiaoError::Config(format!(
            "open this Tailscale sign-in URL in a browser: {url}"
        )));
    };
    let status = ProcessCommand::new(launcher).arg(url).status()?;
    if status.success() {
        Ok(())
    } else {
        Err(CiaoError::Config(format!(
            "could not open the browser automatically; open this Tailscale sign-in URL manually: {url}"
        )))
    }
}

fn validate_workflow_writable(path: &Path, contents: &str, yes: bool) -> Result<()> {
    if path.exists() && fs::read_to_string(path)? != contents && !yes {
        return Err(CiaoError::Config(format!(
            "workflow already exists and differs: {}; rerun with --yes to replace it",
            path.display()
        )));
    }
    Ok(())
}

fn read_tailscale_token(from_stdin: bool) -> Result<String> {
    if let Ok(token) = std::env::var("CIAO_TAILSCALE_BOOTSTRAP_TOKEN") {
        return Ok(token);
    }
    if from_stdin {
        return read_secret_value();
    }
    if !io::stdin().is_terminal() {
        return Err(CiaoError::Config(
            "Tailscale setup needs a temporary admin token: pipe it with --tailscale-token-stdin or set CIAO_TAILSCALE_BOOTSTRAP_TOKEN".to_owned(),
        ));
    }
    print!("Paste temporary Tailscale admin token (not stored): ");
    io::stdout().flush()?;
    let mut token = String::new();
    io::stdin().read_line(&mut token)?;
    let token = token.trim().to_owned();
    if token.is_empty() {
        return Err(CiaoError::Config(
            "Tailscale token cannot be empty".to_owned(),
        ));
    }
    Ok(token)
}

fn ensure_tailscale_policy(token: &str, target: &str, yes: bool) -> Result<()> {
    let current = tailscale_fetch_policy(token)?;
    let policy: serde_json::Value = serde_json::from_str(&current).map_err(|_| {
        CiaoError::Config(format!(
            "Tailscale policy is HuJSON or otherwise not safely editable as JSON; add this rule manually, then rerun setup:\n{{\"src\":[\"tag:ciao-ci\"],\"dst\":[\"{target}\"],\"ip\":[\"tcp:22\"]}}"
        ))
    })?;
    let patched = tailscale_policy_patch(&policy, target)?;
    if patched == policy {
        return Ok(());
    }
    let patched_text = serde_json::to_string_pretty(&patched)
        .map_err(|error| CiaoError::Serialization(error.to_string()))?;
    let validation = tailscale_validate_policy(token, &patched_text)?;
    if let Some(error) = tailscale_policy_api_error(&validation) {
        return Err(CiaoError::Config(format!(
            "Tailscale rejected the policy during validation: {error}; no policy change was applied"
        )));
    }
    let preview = tailscale_preview_policy(token, &patched_text)?;
    if let Some(error) = tailscale_policy_api_error(&preview) {
        return Err(CiaoError::Config(format!(
            "Tailscale rejected the policy preview: {error}; no policy change was applied"
        )));
    }
    if !yes {
        println!("Tailscale will add only tag:ciao-ci -> {target} on TCP/22.");
        if !validation.trim().is_empty() {
            println!("Validation: {}", validation.trim());
        }
        if !preview.trim().is_empty() {
            println!("Preview: {}", preview.trim());
        }
        print!("Apply this Tailscale policy change? [y/N] ");
        io::stdout().flush()?;
        let mut answer = String::new();
        io::stdin().read_line(&mut answer)?;
        if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
            return Err(CiaoError::Config(
                "Tailscale policy was not changed; automatic deploy setup stopped".to_owned(),
            ));
        }
    }
    tailscale_apply_policy(token, &patched_text)
}

fn tailscale_policy_api_error(body: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    for key in ["error", "message"] {
        if let Some(message) = value.get(key).and_then(serde_json::Value::as_str) {
            if !message.trim().is_empty() {
                return Some(message.trim().to_owned());
            }
        }
    }
    value
        .get("errors")
        .and_then(serde_json::Value::as_array)
        .map(|errors| {
            errors
                .iter()
                .filter_map(serde_json::Value::as_str)
                .collect::<Vec<_>>()
                .join("; ")
        })
        .filter(|message| !message.is_empty())
}

fn transport_for(name: &str) -> Result<OpenSshTransport> {
    let config = load_config()?;
    let host = configured_host(&config, name)?;
    OpenSshTransport::new(host.ssh)?.with_identity_file(host.identity_file)
}

fn require_noninteractive_sudo(transport: &OpenSshTransport, operation: &str) -> Result<()> {
    match check_remote_sudo(transport) {
        Ok(()) => Ok(()),
        Err(error) if remote_sudo_password_required(&error) => Err(CiaoError::Config(
            format!(
                "{operation} needs remote administrator privileges, but sudo requires a password.\n\n{}",
                passwordless_sudo_instructions(transport)
            ),
        )),
        Err(error) => Err(error),
    }
}

fn actionable_deploy_error(error: CiaoError, host: &str) -> CiaoError {
    match error {
        CiaoError::Config(message) if message.contains("passwordless sudo") => CiaoError::Config(
            format!("{message}\n\nThen retry on this computer:\n  ciao deploy {host}"),
        ),
        other => other,
    }
}

fn is_deploy_lock_error(error: &CiaoError) -> bool {
    matches!(
        error,
        CiaoError::RemoteCommand {
            stage,
            exit: 73,
            ..
        } if stage == "acquire deployment lock"
    )
}

fn recover_interrupted_deploy(transport: &OpenSshTransport, app: &str) -> Result<()> {
    eprintln!();
    eprintln!("Ciao found an existing deployment lock for `{app}`.");
    eprintln!(
        "Ciao cannot tell whether another deployment is still active. If you started one, wait for it; otherwise this lock was probably left by an interrupted deploy."
    );
    eprint!("Remove the lock and resume this deploy? [Y/n] ");
    io::stderr().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    if !matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "" | "y" | "yes"
    ) {
        return Err(CiaoError::Config(
            "deployment lock kept; retry after the other deployment finishes, or run this deploy from a terminal to recover an interrupted lock"
                .to_owned(),
        ));
    }
    recover_deploy_lock(transport, app)?;
    eprintln!("✓ interrupted deployment lock removed; resuming deploy");
    Ok(())
}

fn offer_passwordless_sudo_setup(transport: &OpenSshTransport) -> Result<()> {
    match check_remote_sudo(transport) {
        Ok(()) => Ok(()),
        Err(error) if remote_sudo_password_required(&error) => {
            eprintln!();
            eprintln!(
                "Ciao needs passwordless sudo for the SSH user to finish this deploy and future deploys."
            );
            eprintln!(
                "It will ask for the host password once and configure the policy automatically."
            );
            eprintln!("This grants that SSH account full administrator access without a password.");
            eprint!("Enable it now? [Y/n] ");
            io::stderr().flush()?;
            let mut answer = String::new();
            io::stdin().read_line(&mut answer)?;
            if matches!(
                answer.trim().to_ascii_lowercase().as_str(),
                "" | "y" | "yes"
            ) {
                eprintln!(
                    "Opening one SSH session; enter the host password at the remote sudo prompt."
                );
                configure_passwordless_sudo_interactively(transport)?;
                eprintln!("✓ passwordless sudo configured");
                Ok(())
            } else {
                Err(CiaoError::Config(format!(
                    "deployment needs passwordless sudo; rerun from a terminal and accept the one-time setup, or configure it manually.\n\n{}",
                    passwordless_sudo_instructions(transport)
                )))
            }
        }
        Err(error) => Err(error),
    }
}

fn ensure_remote_sudo(
    transport: &OpenSshTransport,
    _json_output: bool,
    operation: &str,
) -> Result<()> {
    match check_remote_sudo(transport) {
        Ok(()) => Ok(()),
        Err(error) if remote_sudo_password_required(&error) => Err(CiaoError::Config(format!(
            "{operation} needs sudo without a prompt across multiple SSH commands.\n\n{}",
            passwordless_sudo_instructions(transport)
        ))),
        Err(error) => Err(error),
    }
}

fn init_host_from_terminal(
    transport: &OpenSshTransport,
    json_output: bool,
) -> Result<HostInitResult> {
    match check_remote_sudo(transport) {
        Ok(()) => init_host(transport),
        Err(error) if remote_sudo_password_required(&error) => {
            if json_output || !io::stdin().is_terminal() || !io::stderr().is_terminal() {
                return Err(CiaoError::Config(
                    "host initialization needs the remote administrator password; run it from a terminal so Ciao can open one standard SSH sudo prompt"
                        .to_owned(),
                ));
            }
            eprintln!(
                "Ciao is opening one SSH session for host initialization; type the host password at the remote sudo prompt."
            );
            eprintln!("The password stays inside OpenSSH/sudo; Ciao never receives or stores it.");
            init_host_interactively(transport)
        }
        Err(error) => Err(error),
    }
}

fn authorized_transport_for(
    name: &str,
    json_output: bool,
    operation: &str,
) -> Result<OpenSshTransport> {
    let transport = transport_for(name)?;
    ensure_remote_sudo(&transport, json_output, operation)?;
    Ok(transport)
}

fn mcp_transport_for(name: &str, operation: &str) -> Result<OpenSshTransport> {
    let transport = transport_for(name)?;
    require_noninteractive_sudo(&transport, operation)?;
    Ok(transport)
}

fn maybe_setup_ssh_key(
    host: &Host,
    forced: bool,
    non_interactive: bool,
) -> Result<Option<PathBuf>> {
    let existing_transport =
        OpenSshTransport::new(host.ssh.clone())?.with_identity_file(host.identity_file.clone())?;
    if existing_transport.inspect().is_ok() && !forced {
        return Ok(None);
    }
    if non_interactive {
        return Err(CiaoError::Config(format!(
            "SSH key authentication is not ready for {}; configure OpenSSH keys/agent or rerun ciao host add {} {} --setup-key interactively",
            host.ssh, host.name, host.ssh
        )));
    }
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(CiaoError::Config(format!(
            "SSH authentication for {} needs one interactive bootstrap; rerun from a terminal or configure OpenSSH keys/agent first",
            host.ssh
        )));
    }
    let identity_path = host
        .identity_file
        .clone()
        .unwrap_or(default_ssh_identity_path(&host.name)?);
    if !forced {
        eprintln!(
            "Ciao could not authenticate {} with the existing OpenSSH configuration.",
            host.ssh
        );
        eprintln!(
            "It can create {} and install only its public key using one normal SSH login.",
            identity_path.display()
        );
        eprint!("Continue with the one-time SSH key setup? [Y/n] ");
        io::stderr().flush()?;
        let mut answer = String::new();
        io::stdin().read_line(&mut answer)?;
        if matches!(answer.trim().to_ascii_lowercase().as_str(), "n" | "no") {
            return Err(CiaoError::Config(
                "SSH key setup cancelled; no key or host configuration was changed".to_owned(),
            ));
        }
    }
    let public_key = ensure_ssh_identity(&identity_path, &format!("ciao host {}", host.name))?;
    eprintln!("Ciao is opening one normal SSH session to install the public key.");
    eprintln!("OpenSSH may ask for the server password and host-key confirmation.");
    install_public_key_interactively(&host.ssh, &public_key, Some(&identity_path))?;
    let configured_transport =
        OpenSshTransport::new(host.ssh.clone())?.with_identity_file(Some(identity_path.clone()))?;
    configured_transport.inspect().map_err(|error| {
        CiaoError::Config(format!(
            "the public key was sent, but non-interactive SSH verification failed: {error}"
        ))
    })?;
    eprintln!(
        "✓ SSH key authentication verified; private key remains at {}",
        identity_path.display()
    );
    Ok(Some(identity_path))
}

fn host_command(command: HostCommand, json_output: bool) -> Result<()> {
    match command {
        HostCommand::Add {
            name,
            ssh,
            setup_key,
            non_interactive,
        } => {
            let path = default_config_path();
            let mut config = Config::load(&path)?;
            let mut host = Host::new(&name, &ssh)?;
            if !setup_key {
                if let Some(identity_file) = config
                    .hosts
                    .get(&name)
                    .filter(|existing| existing.ssh == ssh)
                    .and_then(|existing| existing.identity_file.clone())
                {
                    host = host.with_identity_file(identity_file)?;
                }
            }
            if let Some(identity) = maybe_setup_ssh_key(&host, setup_key, non_interactive)? {
                host = host.with_identity_file(identity)?;
            }
            let initial_transport = OpenSshTransport::new(host.ssh.clone())?
                .with_identity_file(host.identity_file.clone())?;
            if !setup_key {
                initial_transport.inspect().map_err(|error| {
                    CiaoError::Config(format!(
                        "cannot connect to {ssh} with the existing OpenSSH configuration: {error}; rerun with --setup-key from a terminal for the guided key bootstrap"
                    ))
                })?;
            }
            let transport = OpenSshTransport::new(host.ssh.clone())?
                .with_identity_file(host.identity_file.clone())?;
            let platform = transport.inspect()?;
            config.add_host(host);
            config.save(&path)?;
            if json_output {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&platform).map_err(ser_error)?
                );
            } else {
                println!("✓ SSH connection");
                println!("✓ OS: {:?}", platform.os);
                println!("✓ service manager: {}", platform.service_manager);
                println!("✓ architecture: {:?}", platform.arch);
                println!("Host `{name}` ready.");
            }
            Ok(())
        }
        HostCommand::List => {
            let config = load_config()?;
            if json_output {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&config.hosts).map_err(ser_error)?
                );
            } else if config.hosts.is_empty() {
                println!("No hosts configured.");
            } else {
                for host in config.hosts.values() {
                    println!("{}\t{}", host.name, host.ssh);
                }
            }
            Ok(())
        }
        HostCommand::Inspect { name } => {
            let transport = transport_for(&name)?;
            let platform = transport.inspect()?;
            output(&platform, json_output, || {
                format!(
                    "{name}: {:?} {:?} / {}",
                    platform.os, platform.arch, platform.service_manager
                )
            });
            Ok(())
        }
        HostCommand::Init { name } => {
            let transport = transport_for(&name)?;
            let result = init_host_from_terminal(&transport, json_output)?;
            output(&result, json_output, || {
                format!(
                    "✓ {}\n  dependencies: {}",
                    result.message,
                    result.dependencies.join(", ")
                )
            });
            Ok(())
        }
    }
}

fn lifecycle(args: &AppArgs, action: ciao_core::LifecycleAction, json_output: bool) -> Result<()> {
    let transport =
        authorized_transport_for(&args.host, json_output, "changing application lifecycle")?;
    let result = lifecycle_action(&transport, &args.app, action)?;
    output(&result, json_output, || result.message.clone());
    Ok(())
}

fn output<T: Serialize>(value: &T, json_output: bool, human: impl FnOnce() -> String) {
    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(value).unwrap_or_else(|_| "{}".to_owned())
        );
    } else {
        println!("{}", human());
    }
}

fn ser_error(error: serde_json::Error) -> CiaoError {
    CiaoError::Serialization(error.to_string())
}

fn mcp_stdio() -> Result<()> {
    use std::io::{self, BufRead, Write};
    let stdin = io::stdin();
    let mut stdout = io::BufWriter::new(io::stdout());
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let request: serde_json::Value = match serde_json::from_str(&line) {
            Ok(request) => request,
            Err(error) => {
                let response = json!({
                    "jsonrpc": "2.0",
                    "id": serde_json::Value::Null,
                    "error": {"code": -32700, "message": format!("invalid JSON: {error}")}
                });
                writeln!(stdout, "{}", response)?;
                stdout.flush()?;
                continue;
            }
        };
        let id = request
            .get("id")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let method = request
            .get("method")
            .and_then(|value| value.as_str())
            .unwrap_or_default();
        if method == "notifications/initialized" {
            continue;
        }
        let response = match method {
            "initialize" => json!({
                "jsonrpc": "2.0", "id": id,
                "result": {"protocolVersion": "2024-11-05", "capabilities": {"tools": {}}, "serverInfo": {"name": "ciao", "version": env!("CARGO_PKG_VERSION")}}
            }),
            "tools/list" => {
                let profile = load_config()
                    .map(|config| config.mcp.profile)
                    .unwrap_or_else(|_| "read-only".to_owned());
                json!({
                    "jsonrpc": "2.0", "id": id,
                    "result": {"tools": mcp_tools(&profile)}
                })
            }
            "tools/call" => {
                let params = request.get("params").cloned().unwrap_or_default();
                let name = params
                    .get("name")
                    .and_then(|value| value.as_str())
                    .unwrap_or_default();
                let arguments = params
                    .get("arguments")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                match mcp_call(name, &arguments) {
                    Ok(value) => {
                        json!({"jsonrpc": "2.0", "id": id, "result": {"content": [{"type": "text", "text": serde_json::to_string_pretty(&value).unwrap_or_else(|_| "{}".to_owned())}]}})
                    }
                    Err(error) => {
                        json!({"jsonrpc": "2.0", "id": id, "result": {"isError": true, "content": [{"type": "text", "text": error.to_string()}]}})
                    }
                }
            }
            _ => {
                json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": format!("unknown MCP method `{method}`")}})
            }
        };
        writeln!(
            stdout,
            "{}",
            serde_json::to_string(&response).map_err(ser_error)?
        )?;
        stdout.flush()?;
    }
    Ok(())
}

fn mcp_tools(profile: &str) -> serde_json::Value {
    let tools = json!([
        {"name":"list_hosts","description":"List configured SSH hosts","inputSchema":{"type":"object"}},
        {"name":"inspect_host","description":"Inspect remote OS and architecture","inputSchema":{"type":"object","properties":{"host":{"type":"string"}},"required":["host"]}},
        {"name":"inspect_app","description":"Detect the current local project","inputSchema":{"type":"object","properties":{"path":{"type":"string"}}}},
        {"name":"list_apps","description":"List managed applications on a host","inputSchema":{"type":"object","properties":{"host":{"type":"string"}},"required":["host"]}},
        {"name":"list_releases","description":"List immutable releases for an application","inputSchema":{"type":"object","properties":{"host":{"type":"string"},"app":{"type":"string"}},"required":["host","app"]}},
        {"name":"deploy_app","description":"Deploy a local project to a configured host","inputSchema":{"type":"object","properties":{"host":{"type":"string"},"path":{"type":"string"},"domain":{"type":"string"},"dry_run":{"type":"boolean"}},"required":["host"]}},
        {"name":"get_status","description":"Get managed application status","inputSchema":{"type":"object","properties":{"host":{"type":"string"},"app":{"type":"string"}},"required":["host","app"]}},
        {"name":"get_logs","description":"Read managed application logs","inputSchema":{"type":"object","properties":{"host":{"type":"string"},"app":{"type":"string"},"since":{"type":"string"}},"required":["host","app"]}},
        {"name":"restart_app","description":"Restart a managed application","inputSchema":{"type":"object","properties":{"host":{"type":"string"},"app":{"type":"string"}},"required":["host","app"]}},
        {"name":"rollback_app","description":"Rollback to the previous immutable release","inputSchema":{"type":"object","properties":{"host":{"type":"string"},"app":{"type":"string"}},"required":["host","app"]}},
        {"name":"start_app","description":"Start a managed application","inputSchema":{"type":"object","properties":{"host":{"type":"string"},"app":{"type":"string"}},"required":["host","app"]}},
        {"name":"stop_app","description":"Stop a managed application","inputSchema":{"type":"object","properties":{"host":{"type":"string"},"app":{"type":"string"}},"required":["host","app"]}},
        {"name":"set_environment_variable","description":"Set a managed application environment variable","inputSchema":{"type":"object","properties":{"host":{"type":"string"},"app":{"type":"string"},"key":{"type":"string"},"value":{"type":"string"}},"required":["host","app","key","value"]}},
        {"name":"remove_environment_variable","description":"Remove a managed application environment variable","inputSchema":{"type":"object","properties":{"host":{"type":"string"},"app":{"type":"string"},"key":{"type":"string"}},"required":["host","app","key"]}},
        {"name":"initialize_host","description":"Install Ciao prerequisites and configure Caddy on a host","inputSchema":{"type":"object","properties":{"host":{"type":"string"}},"required":["host"]}},
        {"name":"add_domain","description":"Configure a Caddy domain for a managed application","inputSchema":{"type":"object","properties":{"host":{"type":"string"},"app":{"type":"string"},"domain":{"type":"string"}},"required":["host","app","domain"]}},
        {"name":"remove_domain","description":"Remove a Caddy domain for a managed application","inputSchema":{"type":"object","properties":{"host":{"type":"string"},"app":{"type":"string"},"domain":{"type":"string"}},"required":["host","app","domain"]}}
    ]);
    tools
        .as_array()
        .into_iter()
        .flatten()
        .filter(|tool| {
            tool.get("name")
                .and_then(|value| value.as_str())
                .is_some_and(|name| tool_allowed(profile, name))
        })
        .cloned()
        .collect::<Vec<_>>()
        .into()
}

fn mcp_call(name: &str, args: &serde_json::Value) -> Result<serde_json::Value> {
    let profile = load_config()
        .map(|config| config.mcp.profile)
        .unwrap_or_else(|_| "read-only".to_owned());
    if !tool_allowed(&profile, name) {
        return Err(CiaoError::Config(format!(
            "MCP profile `{profile}` does not allow `{name}`"
        )));
    }
    match name {
        "list_hosts" => {
            let config = load_config()?;
            Ok(serde_json::to_value(config.hosts).map_err(ser_error)?)
        }
        "inspect_host" => {
            let host = required_string(args, "host")?;
            Ok(serde_json::to_value(transport_for(host)?.inspect()?).map_err(ser_error)?)
        }
        "list_apps" => Ok(serde_json::to_value(list_apps(&mcp_transport_for(
            required_string(args, "host")?,
            "MCP list_apps",
        )?)?)
        .map_err(ser_error)?),
        "inspect_app" => {
            let path = optional_string(args, "path")?.unwrap_or(".");
            Ok(
                serde_json::to_value(detect_project(&PathBuf::from(path).canonicalize()?)?)
                    .map_err(ser_error)?,
            )
        }
        "deploy_app" => {
            let host_name = required_string(args, "host")?;
            let path =
                PathBuf::from(optional_string(args, "path")?.unwrap_or(".")).canonicalize()?;
            let plan = detect_project(&path)?;
            let domain = optional_string(args, "domain")?;
            let dry_run = optional_bool(args, "dry_run")?.unwrap_or(false);
            let transport = transport_for(host_name)?;
            if !dry_run {
                require_noninteractive_sudo(&transport, "MCP deploy_app")?;
            }
            Ok(serde_json::to_value(deploy_with_reporter(
                &transport,
                &path,
                &plan,
                domain,
                dry_run,
                &NoopProgressReporter,
            )?)
            .map_err(ser_error)?)
        }
        "get_status" => {
            let transport = mcp_transport_for(required_string(args, "host")?, "MCP get_status")?;
            Ok(
                serde_json::to_value(app_status(&transport, required_string(args, "app")?)?)
                    .map_err(ser_error)?,
            )
        }
        "list_releases" => {
            let transport = mcp_transport_for(required_string(args, "host")?, "MCP list_releases")?;
            Ok(
                serde_json::to_value(list_releases(&transport, required_string(args, "app")?)?)
                    .map_err(ser_error)?,
            )
        }
        "get_logs" => {
            let transport = mcp_transport_for(required_string(args, "host")?, "MCP get_logs")?;
            Ok(serde_json::to_value(app_logs(
                &transport,
                required_string(args, "app")?,
                false,
                optional_string(args, "since")?,
            )?)
            .map_err(ser_error)?)
        }
        "restart_app" => {
            let transport = mcp_transport_for(required_string(args, "host")?, "MCP restart_app")?;
            Ok(serde_json::to_value(lifecycle_action(
                &transport,
                required_string(args, "app")?,
                ciao_core::LifecycleAction::Restart,
            )?)
            .map_err(ser_error)?)
        }
        "rollback_app" => {
            let transport = mcp_transport_for(required_string(args, "host")?, "MCP rollback_app")?;
            Ok(
                serde_json::to_value(rollback(&transport, required_string(args, "app")?)?)
                    .map_err(ser_error)?,
            )
        }
        "start_app" => {
            let transport = mcp_transport_for(required_string(args, "host")?, "MCP start_app")?;
            Ok(serde_json::to_value(lifecycle_action(
                &transport,
                required_string(args, "app")?,
                ciao_core::LifecycleAction::Start,
            )?)
            .map_err(ser_error)?)
        }
        "stop_app" => {
            let transport = mcp_transport_for(required_string(args, "host")?, "MCP stop_app")?;
            Ok(serde_json::to_value(lifecycle_action(
                &transport,
                required_string(args, "app")?,
                ciao_core::LifecycleAction::Stop,
            )?)
            .map_err(ser_error)?)
        }
        "set_environment_variable" => {
            let transport = mcp_transport_for(
                required_string(args, "host")?,
                "MCP set_environment_variable",
            )?;
            set_env(
                &transport,
                required_string(args, "app")?,
                required_string(args, "key")?,
                required_string(args, "value")?,
            )?;
            Ok(json!({"changed": true}))
        }
        "remove_environment_variable" => {
            let transport = mcp_transport_for(
                required_string(args, "host")?,
                "MCP remove_environment_variable",
            )?;
            unset_env(
                &transport,
                required_string(args, "app")?,
                required_string(args, "key")?,
            )?;
            Ok(json!({"changed": true}))
        }
        "initialize_host" => {
            let transport =
                mcp_transport_for(required_string(args, "host")?, "MCP initialize_host")?;
            Ok(serde_json::to_value(init_host(&transport)?).map_err(ser_error)?)
        }
        "add_domain" => {
            let transport = mcp_transport_for(required_string(args, "host")?, "MCP add_domain")?;
            add_domain(
                &transport,
                required_string(args, "app")?,
                required_string(args, "domain")?,
            )?;
            Ok(json!({"changed": true}))
        }
        "remove_domain" => {
            let transport = mcp_transport_for(required_string(args, "host")?, "MCP remove_domain")?;
            remove_domain(
                &transport,
                required_string(args, "app")?,
                required_string(args, "domain")?,
            )?;
            Ok(json!({"changed": true}))
        }
        _ => Err(CiaoError::Config(format!("unknown MCP tool `{name}`"))),
    }
}

fn tool_allowed(profile: &str, name: &str) -> bool {
    let read_only = matches!(
        name,
        "list_hosts"
            | "inspect_host"
            | "inspect_app"
            | "list_apps"
            | "list_releases"
            | "get_status"
            | "get_logs"
    );
    let operator = matches!(
        name,
        "deploy_app" | "restart_app" | "start_app" | "stop_app" | "rollback_app"
    );
    let admin = matches!(
        name,
        "set_environment_variable"
            | "remove_environment_variable"
            | "initialize_host"
            | "add_domain"
            | "remove_domain"
    );
    match profile {
        "read-only" => read_only,
        "operator" => read_only || operator,
        "admin" => read_only || operator || admin,
        _ => false,
    }
}

fn run_ui(host_name: &str, port: u16) -> Result<()> {
    if !(1024..=65535).contains(&port) {
        return Err(CiaoError::Config(
            "UI port must be between 1024 and 65535".to_owned(),
        ));
    }
    let transport = transport_for(host_name)?;
    let listener = std::net::TcpListener::bind(("127.0.0.1", port))
        .map_err(|error| CiaoError::Config(format!("cannot bind local UI on {port}: {error}")))?;
    println!("Ciao UI: http://127.0.0.1:{port} (Ctrl-C to stop)");
    for stream in listener.incoming() {
        let mut stream = stream?;
        use std::io::Write;
        let request = read_http_request(&mut stream)?;
        let (content_type, response_body) = if request.starts_with("GET /api/apps ") {
            (
                "application/json",
                serde_json::to_string(&list_apps(&transport)?).map_err(ser_error)?,
            )
        } else {
            ("text/html; charset=utf-8", ui_html(host_name))
        };
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            response_body.len(), response_body
        );
        stream.write_all(response.as_bytes())?;
    }
    Ok(())
}

fn read_http_request(stream: &mut std::net::TcpStream) -> Result<String> {
    use std::io::Read;
    let mut request = [0_u8; 4096];
    let size = stream.read(&mut request)?;
    Ok(String::from_utf8_lossy(&request[..size]).into_owned())
}

fn ui_html(host_name: &str) -> String {
    format!(
        "<!doctype html><meta charset=utf-8><meta name=viewport content='width=device-width'><title>Ciao</title><style>body{{font:16px system-ui;max-width:900px;margin:40px auto;padding:0 20px}}table{{border-collapse:collapse;width:100%}}td,th{{padding:8px;border-bottom:1px solid #ddd;text-align:left}}code{{font-family:ui-monospace}}button{{padding:7px 10px}}</style><h1>Ciao</h1><p>Host: <code>{}</code> <button onclick='load()'>Refresh</button></p><table><thead><tr><th>App</th><th>Status</th><th>Release</th><th>Type</th></tr></thead><tbody id=apps><tr><td colspan=4>Loading…</td></tr></tbody></table><script>const esc=s=>String(s??'-').replace(/[&<>\"']/g,c=>({{'&':'&amp;','<':'&lt;','>':'&gt;','\"':'&quot;',\"'\":'&#39;'}}[c]));async function load(){{const a=await (await fetch('/api/apps')).json();document.querySelector('#apps').innerHTML=a.map(x=>`<tr><td>${{esc(x.app)}}</td><td>${{esc(x.status)}}</td><td>${{esc(x.release)}}</td><td>${{esc(x.app_type)}}</td></tr>`).join('')||'<tr><td colspan=4>No apps</td></tr>';}}load();</script>",
        html_escape(host_name)
    )
}

fn html_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn optional_string<'a>(value: &'a serde_json::Value, key: &str) -> Result<Option<&'a str>> {
    match value.get(key) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(value) => value
            .as_str()
            .map(Some)
            .ok_or_else(|| CiaoError::Config(format!("MCP argument `{key}` must be a string"))),
    }
}

fn optional_bool(value: &serde_json::Value, key: &str) -> Result<Option<bool>> {
    match value.get(key) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(value) => value
            .as_bool()
            .map(Some)
            .ok_or_else(|| CiaoError::Config(format!("MCP argument `{key}` must be a boolean"))),
    }
}

fn required_string<'a>(value: &'a serde_json::Value, key: &str) -> Result<&'a str> {
    value
        .get(key)
        .and_then(|value| value.as_str())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| CiaoError::Config(format!("MCP argument `{key}` is required")))
}

fn read_secret_value() -> Result<String> {
    use std::io::{self, Read};
    let mut value = String::new();
    io::stdin().read_to_string(&mut value)?;
    let value = value.trim_end_matches(['\r', '\n']).to_owned();
    if value.is_empty() {
        return Err(CiaoError::Config(
            "environment value must be supplied on stdin".to_owned(),
        ));
    }
    Ok(value)
}
