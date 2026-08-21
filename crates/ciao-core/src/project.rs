//! Project detection and component discovery.

use super::*;

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
            package_script(root, "start").map(|_| "bun start".to_owned()),
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
            let start_script = package_script(root, "start").map(|_| format!("{runner} start"));
            if astro_is_server_output(root)? {
                (
                    Runtime::Astro,
                    AppType::Service,
                    Some(install.to_owned()),
                    Some(format!("{runner} run build")),
                    start_script,
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
            let start_script = package_script(root, "start").map(|_| format!("{runner} start"));
            (
                Runtime::Node,
                AppType::Service,
                Some(install.to_owned()),
                Some(format!("{runner} run build")),
                start_script,
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
    let hooks = config.hooks.unwrap_or_default();
    validate_hooks(&hooks)?;
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
        release_keep: config
            .releases
            .as_ref()
            .and_then(|releases| releases.keep)
            .unwrap_or(5)
            .max(1),
        hooks,
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

fn package_script(root: &Path, name: &str) -> Option<String> {
    let contents = fs::read_to_string(root.join("package.json")).ok()?;
    let package: serde_json::Value = serde_json::from_str(&contents).ok()?;
    package
        .get("scripts")
        .and_then(|scripts| scripts.get(name))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
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

fn validate_hooks(hooks: &HooksConfig) -> Result<()> {
    for (name, command) in [
        ("pre_upload", hooks.pre_upload.as_deref()),
        ("pre_activate", hooks.pre_activate.as_deref()),
        ("post_activate", hooks.post_activate.as_deref()),
        ("on_rollback", hooks.on_rollback.as_deref()),
    ] {
        if let Some(command) = command {
            if command.trim().is_empty() || command.contains('\0') {
                return Err(CiaoError::Config(format!(
                    "hooks.{name} cannot be empty or contain NUL"
                )));
            }
        }
    }
    Ok(())
}
