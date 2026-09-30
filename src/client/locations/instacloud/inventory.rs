//! CLI discovery. These operations run only on workers, never while drawing a pane.
use super::{cli, run, CloudTarget};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct Project {
    pub id: String,
    pub name: String,
    pub org_id: String,
    #[serde(default)]
    pub org_name: String,
}

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct Compute {
    pub id: String,
    pub name: String,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub status: String,
}

#[derive(Clone, Debug)]
pub(crate) struct Projects {
    pub items: Vec<Project>,
    pub preferred: Option<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct Computes {
    pub project: String,
    pub branch: String,
    pub items: Vec<Compute>,
}

pub(crate) fn projects() -> Result<Projects, String> {
    let status = run(cli(&["status", "--json"], None))?;
    if status.get("user").is_none_or(serde_json::Value::is_null) {
        return Err("Sign in to Instacloud with `insta login`, then reopen Add remote.".into());
    }
    if status.get("tokenScope").is_some_and(|v| !v.is_null()) {
        return Err("Instacloud SSH requires a browser login; API-token logins cannot open remote sessions. Run `insta login`.".into());
    }
    let linked = status
        .pointer("/project/projectId")
        .and_then(|v| v.as_str());
    let orgs = run(cli(&["org", "list", "--json"], None))?;
    let orgs = orgs
        .as_array()
        .ok_or("Invalid Instacloud organization list")?;
    let mut items = Vec::new();
    for org in orgs.iter().take(64) {
        let id = org
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or("Organization has no ID")?;
        let value = run(cli(&["project", "list", "--org", id, "--json"], None))?;
        let mut found: Vec<Project> = serde_json::from_value(value).map_err(|e| e.to_string())?;
        for project in &mut found {
            project.org_name = org
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or(id)
                .into();
        }
        items.extend(found);
    }
    let preferred = preferred_project(&items, linked);
    Ok(Projects { items, preferred })
}

pub(crate) fn preferred_project(items: &[Project], linked: Option<&str>) -> Option<String> {
    let named = items
        .iter()
        .filter(|p| p.name == "herdr-remote")
        .collect::<Vec<_>>();
    if named.len() == 1 {
        return Some(named[0].id.clone());
    }
    items
        .iter()
        .find(|p| Some(p.id.as_str()) == linked)
        .or_else(|| (items.len() == 1).then(|| &items[0]))
        .map(|p| p.id.clone())
}

pub(crate) fn computes(project: &str) -> Result<Computes, String> {
    let branches = run(cli(&["branch", "list", "--json"], Some(project)))?;
    let branches = branches.as_array().ok_or("Invalid branch list")?;
    let branch = branches
        .iter()
        .find(|b| b.get("is_default").and_then(|v| v.as_bool()) == Some(true))
        .and_then(|b| b.get("name"))
        .and_then(|n| n.as_str())
        .ok_or("Project has no default branch")?
        .to_owned();
    Ok(Computes {
        project: project.into(),
        items: services(project, &branch)?,
        branch,
    })
}

pub(crate) fn services(project: &str, branch: &str) -> Result<Vec<Compute>, String> {
    let value = run(cli(
        &["service", "list", "--branch", branch, "--json"],
        Some(project),
    ))?;
    let rows: Vec<Compute> = serde_json::from_value(value).map_err(|e| e.to_string())?;
    Ok(rows.into_iter().filter(|c| c.kind == "compute").collect())
}

pub(crate) fn matching_identity<'a>(
    target: &CloudTarget,
    services: &'a [Compute],
) -> Result<&'a Compute, String> {
    let current = services
        .iter()
        .find(|s| s.name == target.service)
        .ok_or("Compute was removed or renamed; refresh the remote list")?;
    if target
        .service_id
        .as_ref()
        .is_some_and(|id| id != &current.id)
    {
        return Err(
            "Compute identity changed. Refusing to operate on a replacement with the same name."
                .into(),
        );
    }
    Ok(current)
}

pub(crate) fn verify_identity(target: &CloudTarget) -> Result<(), String> {
    target.validate()?;
    if target.service_id.is_some() {
        matching_identity(target, &services(&target.project, &target.branch)?)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn project(id: &str, name: &str) -> Project {
        Project {
            id: id.into(),
            name: name.into(),
            org_id: "org".into(),
            org_name: "Org".into(),
        }
    }
    #[test]
    fn project_default_never_guesses_between_duplicate_names() {
        let projects = vec![project("a", "herdr-remote"), project("b", "herdr-remote")];
        assert_eq!(preferred_project(&projects, None), None);
        assert_eq!(preferred_project(&projects, Some("b")), Some("b".into()));
        assert_eq!(
            preferred_project(
                &[project("a", "herdr-remote"), project("b", "other")],
                Some("b")
            ),
            Some("a".into())
        );
    }
    #[test]
    fn a_same_named_replacement_is_not_the_original_compute() {
        let target = CloudTarget {
            project: "p".into(),
            branch: "main".into(),
            service: "remote".into(),
            service_id: Some("original".into()),
        };
        let rows = vec![Compute {
            id: "replacement".into(),
            name: "remote".into(),
            kind: "compute".into(),
            status: "running".into(),
        }];
        assert!(matching_identity(&target, &rows).is_err());
    }
}
