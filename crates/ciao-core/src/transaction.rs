//! Coordinated operations for projects with more than one deployable app.

use super::*;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FullStackDeployResult {
    pub project: String,
    pub components: Vec<DeployResult>,
    pub compensated: bool,
    pub message: String,
}

/// Deploy backend and frontend as one compensating transaction.
///
/// Each component still uses the normal Ciao deploy path. If a later
/// component fails, every component that was activated earlier is restored to
/// the release it had before this transaction. A component that did not exist
/// before the transaction is removed again.
pub fn deploy_full_stack_with_mode(
    transport: &OpenSshTransport,
    project_root: &Path,
    components: &[ProjectComponent],
    domain: Option<&str>,
    dry_run: bool,
    reporter: &dyn ProgressReporter,
    host_mode: DeployHostMode,
) -> Result<FullStackDeployResult> {
    deploy_full_stack_with_mode_options(
        transport,
        project_root,
        components,
        domain,
        dry_run,
        reporter,
        host_mode,
        DeployOptions::default(),
    )
}

#[allow(clippy::too_many_arguments)]
pub fn deploy_full_stack_with_mode_options(
    transport: &OpenSshTransport,
    project_root: &Path,
    components: &[ProjectComponent],
    domain: Option<&str>,
    dry_run: bool,
    reporter: &dyn ProgressReporter,
    host_mode: DeployHostMode,
    options: DeployOptions,
) -> Result<FullStackDeployResult> {
    if components.len() < 2
        || !components
            .iter()
            .any(|component| component.role == ProjectRole::Backend)
        || !components
            .iter()
            .any(|component| component.role == ProjectRole::Frontend)
    {
        return Err(CiaoError::Detection(
            "full-stack deployment requires both backend and frontend components".to_owned(),
        ));
    }
    let mut ordered = components.to_vec();
    ordered.sort_by_key(|component| match component.role {
        ProjectRole::Backend => 0,
        ProjectRole::Frontend => 1,
    });

    let mut deployed = Vec::with_capacity(ordered.len());
    for component in &ordered {
        let component_domain = (component.role == ProjectRole::Frontend)
            .then_some(domain)
            .flatten();
        reporter.updated(&format!(
            "deploy {} component `{}`",
            component.role, component.name
        ));
        match deploy_with_mode_options(
            transport,
            &component.path,
            &component.plan,
            component_domain,
            dry_run,
            reporter,
            host_mode,
            options,
        ) {
            Ok(result) => deployed.push(result),
            Err(error) if deployed.is_empty() || dry_run => return Err(error),
            Err(error) => {
                let compensation = compensate(transport, &deployed);
                let message = match compensation {
                    Ok(()) => format!(
                        "component `{}` failed: {error}; earlier components were restored",
                        component.name
                    ),
                    Err(compensation_error) => format!(
                        "component `{}` failed: {error}; compensating rollback failed: {compensation_error}",
                        component.name
                    ),
                };
                return Err(CiaoError::Deployment {
                    stage: "full-stack transaction".to_owned(),
                    message,
                    previous_release: deployed
                        .first()
                        .and_then(|result| result.previous_release.clone())
                        .unwrap_or_else(|| "none".to_owned()),
                });
            }
        }
    }

    let project = project_root
        .file_name()
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_else(|| "full-stack".to_owned());
    Ok(FullStackDeployResult {
        project,
        components: deployed,
        compensated: false,
        message: if dry_run {
            "✓ full-stack dry-run complete".to_owned()
        } else {
            "✓ backend and frontend active as one transaction".to_owned()
        },
    })
}

fn compensate(transport: &OpenSshTransport, deployed: &[DeployResult]) -> Result<()> {
    let mut errors = Vec::new();
    for result in deployed.iter().rev() {
        let operation = match result.previous_release.as_deref() {
            Some(previous) => rollback_to(transport, &result.app, Some(previous)),
            None => remove_app(transport, &result.app),
        };
        if let Err(error) = operation {
            errors.push(format!("{}: {error}", result.app));
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(CiaoError::Config(errors.join("; ")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_partial_component_lists_before_opening_ssh() {
        let transport = OpenSshTransport::new("user@example.com").unwrap();
        let error = deploy_full_stack_with_mode(
            &transport,
            Path::new("."),
            &[],
            None,
            true,
            &NoopProgressReporter,
            DeployHostMode::NonInteractive,
        )
        .unwrap_err();
        assert!(error.to_string().contains("backend and frontend"));
    }
}
