use ciaoship_core::*;
use clap::{Args, Parser, Subcommand};
use serde::Serialize;
use serde_json::json;
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(name = "ciaoship", version, about = "Ship apps. Skip the ops.")]
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
    /// Run the local stdio MCP server.
    Mcp,
}

#[derive(Debug, Subcommand)]
enum HostCommand {
    Add {
        name: String,
        ssh: String,
    },
    List,
    Inspect {
        name: String,
    },
    /// Install CiaoShip's remote prerequisites and configure Caddy.
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

fn main() {
    if let Err(error) = run(Cli::parse()) {
        eprintln!("✗ {error}");
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
            let plan = detect_project(&path)?;
            let config = load_config()?;
            let host = configured_host(&config, &args.host)?;
            let transport = OpenSshTransport::new(host.ssh.clone())?;
            let result = deploy(
                &transport,
                &path,
                &plan,
                args.domain.as_deref(),
                args.dry_run,
            )?;
            output(&result, cli.json, || result.message.clone());
            Ok(())
        }
        Command::Dev(args) => local_dev_command(args, cli.json),
        Command::Status(args) => {
            let result = app_status(&transport_for(&args.host)?, &args.app)?;
            output(&result, cli.json, || result.message.clone());
            Ok(())
        }
        Command::Apps { host } => {
            let result = list_apps(&transport_for(&host)?)?;
            output(&result, cli.json, || {
                if result.is_empty() {
                    "No CiaoShip applications found.".to_owned()
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
            let result = list_releases(&transport_for(&host)?, &app)?;
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
            let result = app_logs(
                &transport_for(&args.host)?,
                &args.app,
                args.follow,
                args.since.as_deref(),
            )?;
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
        Command::Restart(args) => {
            lifecycle(&args, ciaoship_core::LifecycleAction::Restart, cli.json)
        }
        Command::Start(args) => lifecycle(&args, ciaoship_core::LifecycleAction::Start, cli.json),
        Command::Stop(args) => lifecycle(&args, ciaoship_core::LifecycleAction::Stop, cli.json),
        Command::Rollback(args) => {
            let result = rollback(&transport_for(&args.host)?, &args.app)?;
            output(&result, cli.json, || result.message.clone());
            Ok(())
        }
        Command::Ui { host, port } => run_ui(&host, port),
        Command::Env { command } => match command {
            EnvCommand::Set { host, app, key } => {
                let value = read_secret_value()?;
                set_env(&transport_for(&host)?, &app, &key, &value)?;
                if cli.json {
                    println!("{}", json!({"app": app, "key": key, "changed": true}));
                } else {
                    println!("✓ environment updated for {app}: {key}");
                }
                Ok(())
            }
            EnvCommand::Unset { host, app, key } => {
                unset_env(&transport_for(&host)?, &app, &key)?;
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
                add_domain(&transport_for(&host)?, &app, &domain)?;
                if cli.json {
                    println!("{}", json!({"app": app, "domain": domain, "changed": true}));
                } else {
                    println!("✓ domain configured: {domain}");
                }
                Ok(())
            }
            DomainCommand::Remove { host, app, domain } => {
                remove_domain(&transport_for(&host)?, &app, &domain)?;
                if cli.json {
                    println!("{}", json!({"app": app, "domain": domain, "changed": true}));
                } else {
                    println!("✓ domain removed: {domain}");
                }
                Ok(())
            }
        },
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

fn load_config() -> Result<Config> {
    Config::load(&default_config_path())
}

fn configured_host(config: &Config, name: &str) -> Result<Host> {
    config.hosts.get(name).cloned().ok_or_else(|| {
        CiaoError::Config(format!(
            "host `{name}` is not configured; run `ciaoship host add`"
        ))
    })
}

fn transport_for(name: &str) -> Result<OpenSshTransport> {
    let config = load_config()?;
    let host = configured_host(&config, name)?;
    OpenSshTransport::new(host.ssh)
}

fn host_command(command: HostCommand, json_output: bool) -> Result<()> {
    match command {
        HostCommand::Add { name, ssh } => {
            let host = Host::new(&name, &ssh)?;
            let path = default_config_path();
            let mut config = Config::load(&path)?;
            let transport = OpenSshTransport::new(host.ssh.clone())?;
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
            let result = init_host(&transport_for(&name)?)?;
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

fn lifecycle(
    args: &AppArgs,
    action: ciaoship_core::LifecycleAction,
    json_output: bool,
) -> Result<()> {
    let transport = transport_for(&args.host)?;
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
                "result": {"protocolVersion": "2024-11-05", "capabilities": {"tools": {}}, "serverInfo": {"name": "ciaoship", "version": env!("CARGO_PKG_VERSION")}}
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
        {"name":"initialize_host","description":"Install CiaoShip prerequisites and configure Caddy on a host","inputSchema":{"type":"object","properties":{"host":{"type":"string"}},"required":["host"]}},
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
        "list_apps" => Ok(
            serde_json::to_value(list_apps(&transport_for(required_string(args, "host")?)?)?)
                .map_err(ser_error)?,
        ),
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
            Ok(serde_json::to_value(deploy(
                &transport_for(host_name)?,
                &path,
                &plan,
                domain,
                dry_run,
            )?)
            .map_err(ser_error)?)
        }
        "get_status" => {
            let transport = transport_for(required_string(args, "host")?)?;
            Ok(
                serde_json::to_value(app_status(&transport, required_string(args, "app")?)?)
                    .map_err(ser_error)?,
            )
        }
        "list_releases" => {
            let transport = transport_for(required_string(args, "host")?)?;
            Ok(
                serde_json::to_value(list_releases(&transport, required_string(args, "app")?)?)
                    .map_err(ser_error)?,
            )
        }
        "get_logs" => {
            let transport = transport_for(required_string(args, "host")?)?;
            Ok(serde_json::to_value(app_logs(
                &transport,
                required_string(args, "app")?,
                false,
                optional_string(args, "since")?,
            )?)
            .map_err(ser_error)?)
        }
        "restart_app" => {
            let transport = transport_for(required_string(args, "host")?)?;
            Ok(serde_json::to_value(lifecycle_action(
                &transport,
                required_string(args, "app")?,
                ciaoship_core::LifecycleAction::Restart,
            )?)
            .map_err(ser_error)?)
        }
        "rollback_app" => {
            let transport = transport_for(required_string(args, "host")?)?;
            Ok(
                serde_json::to_value(rollback(&transport, required_string(args, "app")?)?)
                    .map_err(ser_error)?,
            )
        }
        "start_app" => {
            let transport = transport_for(required_string(args, "host")?)?;
            Ok(serde_json::to_value(lifecycle_action(
                &transport,
                required_string(args, "app")?,
                ciaoship_core::LifecycleAction::Start,
            )?)
            .map_err(ser_error)?)
        }
        "stop_app" => {
            let transport = transport_for(required_string(args, "host")?)?;
            Ok(serde_json::to_value(lifecycle_action(
                &transport,
                required_string(args, "app")?,
                ciaoship_core::LifecycleAction::Stop,
            )?)
            .map_err(ser_error)?)
        }
        "set_environment_variable" => {
            let transport = transport_for(required_string(args, "host")?)?;
            set_env(
                &transport,
                required_string(args, "app")?,
                required_string(args, "key")?,
                required_string(args, "value")?,
            )?;
            Ok(json!({"changed": true}))
        }
        "remove_environment_variable" => {
            let transport = transport_for(required_string(args, "host")?)?;
            unset_env(
                &transport,
                required_string(args, "app")?,
                required_string(args, "key")?,
            )?;
            Ok(json!({"changed": true}))
        }
        "initialize_host" => {
            let transport = transport_for(required_string(args, "host")?)?;
            Ok(serde_json::to_value(init_host(&transport)?).map_err(ser_error)?)
        }
        "add_domain" => {
            let transport = transport_for(required_string(args, "host")?)?;
            add_domain(
                &transport,
                required_string(args, "app")?,
                required_string(args, "domain")?,
            )?;
            Ok(json!({"changed": true}))
        }
        "remove_domain" => {
            let transport = transport_for(required_string(args, "host")?)?;
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
    println!("CiaoShip UI: http://127.0.0.1:{port} (Ctrl-C to stop)");
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
        "<!doctype html><meta charset=utf-8><meta name=viewport content='width=device-width'><title>CiaoShip</title><style>body{{font:16px system-ui;max-width:900px;margin:40px auto;padding:0 20px}}table{{border-collapse:collapse;width:100%}}td,th{{padding:8px;border-bottom:1px solid #ddd;text-align:left}}code{{font-family:ui-monospace}}button{{padding:7px 10px}}</style><h1>CiaoShip</h1><p>Host: <code>{}</code> <button onclick='load()'>Refresh</button></p><table><thead><tr><th>App</th><th>Status</th><th>Release</th><th>Type</th></tr></thead><tbody id=apps><tr><td colspan=4>Loading…</td></tr></tbody></table><script>const esc=s=>String(s??'-').replace(/[&<>\"']/g,c=>({{'&':'&amp;','<':'&lt;','>':'&gt;','\"':'&quot;',\"'\":'&#39;'}}[c]));async function load(){{const a=await (await fetch('/api/apps')).json();document.querySelector('#apps').innerHTML=a.map(x=>`<tr><td>${{esc(x.app)}}</td><td>${{esc(x.status)}}</td><td>${{esc(x.release)}}</td><td>${{esc(x.app_type)}}</td></tr>`).join('')||'<tr><td colspan=4>No apps</td></tr>';}}load();</script>",
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
