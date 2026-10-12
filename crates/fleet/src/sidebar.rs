//! The sidebar's projection of the fleet: pinned workspaces, then projects
//! merged across hosts by project key, each with its workspaces.

use std::collections::BTreeMap;

use paseo_client::proto::{Bucket, Workspace};

use crate::Fleet;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Grouping {
    #[default]
    Project,
    Status,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SidebarRow {
    pub host: String,
    pub workspace_id: String,
    pub title: String,
    pub branch: Option<String>,
    pub project_name: String,
    pub project_key: String,
    pub bucket: Bucket,
    pub archiving: bool,
    pub pinned_at: Option<String>,
    pub labels: Vec<(String, String)>,
    pub additions: u64,
    pub deletions: u64,
    pub workspace_kind: String,
    pub pull_request: Option<(u64, String)>,
    pub status_entered_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SidebarProject {
    pub key: String,
    pub name: String,
    pub hosts: Vec<String>,
    /// (host, projectId, rootPath, kind) for creating workspaces.
    pub placements: Vec<(String, String, String, String)>,
    pub rows: Vec<SidebarRow>,
}

impl SidebarProject {
    /// Collapsed projects show the most urgent bucket among their rows.
    pub fn aggregate(&self) -> Bucket {
        self.rows
            .iter()
            .map(|row| row.bucket)
            .min()
            .unwrap_or(Bucket::Done)
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct SidebarView {
    pub pinned: Vec<SidebarRow>,
    pub projects: Vec<SidebarProject>,
    pub groups: Vec<(Bucket, Vec<SidebarRow>)>,
    pub multi_host: bool,
}

#[derive(Debug, Clone, Default)]
pub struct Filters {
    pub hosts: Vec<String>,
    pub labels: Vec<String>,
}

impl SidebarView {
    pub fn build(fleet: &Fleet, grouping: Grouping, filters: &Filters) -> Self {
        let mut projects: BTreeMap<String, SidebarProject> = BTreeMap::new();
        let mut pinned = Vec::new();
        let mut all_rows = Vec::new();
        let mut hosts_with_projects = std::collections::BTreeSet::new();

        for host in &fleet.hosts {
            if !filters.hosts.is_empty() && !filters.hosts.contains(&host.config.name) {
                continue;
            }
            let dir = &host.directory;
            for project in dir.projects.values() {
                let key = project
                    .project_key
                    .clone()
                    .filter(|key| !key.is_empty())
                    .unwrap_or_else(|| format!("{}\u{0}{}", host.config.name, project.project_id));
                let entry = projects
                    .entry(key.clone())
                    .or_insert_with(|| SidebarProject {
                        key: key.clone(),
                        name: project.name(),
                        hosts: Vec::new(),
                        placements: Vec::new(),
                        rows: Vec::new(),
                    });
                if !entry.hosts.contains(&host.config.name) {
                    entry.hosts.push(host.config.name.clone());
                }
                entry.placements.push((
                    host.config.name.clone(),
                    project.project_id.clone(),
                    project.project_root_path.clone(),
                    project.project_kind.clone(),
                ));
                hosts_with_projects.insert(host.config.name.clone());
            }
            let newest_roots = dir.newest_roots();
            for workspace in dir.workspaces.values() {
                let project = dir.projects.get(&workspace.project_id);
                let key = project
                    .and_then(|p| p.project_key.clone())
                    .or_else(|| workspace.project_key().map(str::to_string))
                    .filter(|key| !key.is_empty())
                    .unwrap_or_else(|| {
                        format!("{}\u{0}{}", host.config.name, workspace.project_id)
                    });
                let row = row(
                    host.config.name.as_str(),
                    dir,
                    workspace,
                    project.map(|p| p.name()),
                    &newest_roots,
                );
                if !filters.labels.is_empty() && !label_match(&row, &filters.labels) {
                    continue;
                }
                hosts_with_projects.insert(host.config.name.clone());
                all_rows.push(row.clone());
                if row.pinned_at.is_some() {
                    pinned.push(row);
                    projects
                        .entry(key.clone())
                        .or_insert_with(|| empty_project(&key, workspace));
                    continue;
                }
                projects
                    .entry(key.clone())
                    .or_insert_with(|| empty_project(&key, workspace))
                    .rows
                    .push(row);
            }
        }

        let mut projects: Vec<SidebarProject> = projects.into_values().collect();
        if !filters.labels.is_empty() {
            projects.retain(|project| !project.rows.is_empty());
        }
        projects.sort_by(|a, b| natural(&a.name, &b.name).then(a.key.cmp(&b.key)));
        for project in &mut projects {
            project.rows.sort_by(|a, b| {
                natural(&a.title, &b.title).then(a.workspace_id.cmp(&b.workspace_id))
            });
        }
        pinned.sort_by(|a, b| b.pinned_at.cmp(&a.pinned_at));

        let groups = if grouping == Grouping::Status {
            Bucket::GROUP_ORDER
                .iter()
                .filter_map(|bucket| {
                    let mut rows: Vec<SidebarRow> = all_rows
                        .iter()
                        .filter(|row| row.bucket == *bucket && row.pinned_at.is_none())
                        .cloned()
                        .collect();
                    rows.sort_by(|a, b| {
                        b.status_entered_at
                            .cmp(&a.status_entered_at)
                            .then(natural(&a.project_name, &b.project_name))
                            .then(natural(&a.title, &b.title))
                    });
                    (!rows.is_empty()).then_some((*bucket, rows))
                })
                .collect()
        } else {
            Vec::new()
        };

        Self {
            pinned,
            projects,
            groups,
            multi_host: hosts_with_projects.len() > 1,
        }
    }

    /// Rows in visual order, for Cmd+1…9 and next/previous workspace.
    pub fn visible_rows<'a>(
        &'a self,
        grouping: Grouping,
        collapsed: &'a dyn Fn(&str) -> bool,
    ) -> Vec<&'a SidebarRow> {
        let mut out: Vec<&SidebarRow> = self.pinned.iter().collect();
        match grouping {
            Grouping::Project => {
                for project in &self.projects {
                    if !collapsed(&project.key) {
                        out.extend(project.rows.iter());
                    }
                }
            }
            Grouping::Status => {
                for (bucket, rows) in &self.groups {
                    if !collapsed(bucket.label()) {
                        out.extend(rows.iter());
                    }
                }
            }
        }
        out
    }
}

fn empty_project(key: &str, workspace: &Workspace) -> SidebarProject {
    SidebarProject {
        key: key.to_string(),
        name: workspace
            .project_custom_name
            .clone()
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| workspace.project_display_name.clone()),
        hosts: Vec::new(),
        placements: Vec::new(),
        rows: Vec::new(),
    }
}

fn row(
    host: &str,
    dir: &crate::Directory,
    workspace: &Workspace,
    project_name: Option<String>,
    newest_roots: &std::collections::HashMap<&str, &crate::Agent>,
) -> SidebarRow {
    let pull_request = workspace
        .github_runtime
        .as_ref()
        .and_then(|github| github.pull_request.as_ref())
        .and_then(|pr| pr.number.map(|n| (n, pr_state(pr))));
    SidebarRow {
        host: host.to_string(),
        workspace_id: workspace.id.clone(),
        title: if workspace.name.is_empty() {
            workspace.id.clone()
        } else {
            workspace.name.clone()
        },
        branch: workspace.branch().map(str::to_string),
        project_name: project_name.unwrap_or_else(|| workspace.project_display_name.clone()),
        project_key: workspace.project_key().unwrap_or_default().to_string(),
        bucket: dir.effective_bucket_with(workspace, newest_roots),
        archiving: workspace.archiving_at.is_some(),
        pinned_at: workspace.pinned_at.clone(),
        labels: workspace
            .labels
            .iter()
            .filter_map(|name| {
                dir.label(name)
                    .map(|label| (label.name.clone(), label.color.clone()))
            })
            .collect(),
        additions: workspace
            .diff_stat
            .as_ref()
            .map(|d| d.additions)
            .unwrap_or(0),
        deletions: workspace
            .diff_stat
            .as_ref()
            .map(|d| d.deletions)
            .unwrap_or(0),
        workspace_kind: workspace.workspace_kind.clone(),
        pull_request,
        status_entered_at: workspace.status_entered_at.clone(),
    }
}

fn pr_state(pr: &paseo_client::proto::PullRequest) -> String {
    if pr.is_merged {
        "merged".into()
    } else {
        pr.state.to_lowercase()
    }
}

fn label_match(row: &SidebarRow, wanted: &[String]) -> bool {
    wanted.iter().any(|want| {
        if want.is_empty() {
            row.labels.is_empty()
        } else {
            row.labels.iter().any(|(name, _)| {
                crate::directory::label_key(name) == crate::directory::label_key(want)
            })
        }
    })
}

/// Case-insensitive compare that orders digit runs numerically.
pub fn natural(a: &str, b: &str) -> std::cmp::Ordering {
    let mut a = a.chars().peekable();
    let mut b = b.chars().peekable();
    loop {
        match (a.peek().copied(), b.peek().copied()) {
            (None, None) => return std::cmp::Ordering::Equal,
            (None, Some(_)) => return std::cmp::Ordering::Less,
            (Some(_), None) => return std::cmp::Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let mut na = String::new();
                while let Some(c) = a.peek().copied().filter(char::is_ascii_digit) {
                    na.push(c);
                    a.next();
                }
                let mut nb = String::new();
                while let Some(c) = b.peek().copied().filter(char::is_ascii_digit) {
                    nb.push(c);
                    b.next();
                }
                let ord = na
                    .trim_start_matches('0')
                    .len()
                    .cmp(&nb.trim_start_matches('0').len())
                    .then(na.trim_start_matches('0').cmp(nb.trim_start_matches('0')));
                if ord != std::cmp::Ordering::Equal {
                    return ord;
                }
            }
            (Some(x), Some(y)) => {
                let ord = x.to_lowercase().cmp(y.to_lowercase());
                if ord != std::cmp::Ordering::Equal {
                    return ord;
                }
                a.next();
                b.next();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Fleet, HostConfig};
    use serde_json::json;

    #[test]
    fn merges_projects_across_hosts_and_hoists_pins() {
        let mut fleet = Fleet::new([
            HostConfig {
                name: "a".into(),
                endpoint: None,
                password: None,
            },
            HostConfig {
                name: "b".into(),
                endpoint: None,
                password: None,
            },
        ]);
        let ws = |id: &str, pinned: Option<&str>| {
            json!({"type": "workspace_update", "payload": {"kind": "upsert", "workspace": {
                "id": id, "projectId": "p1", "projectDisplayName": "demo", "projectRootPath": "/z",
                "projectKind": "git", "name": id, "status": "done", "pinnedAt": pinned,
                "project": {"projectKey": "remote:github.com/x/demo"}}}})
        };
        fleet
            .host_mut("a")
            .unwrap()
            .directory
            .apply_push(&ws("w10", None));
        fleet
            .host_mut("a")
            .unwrap()
            .directory
            .apply_push(&ws("w9", Some("2026-10-01T00:00:00Z")));
        fleet
            .host_mut("b")
            .unwrap()
            .directory
            .apply_push(&ws("w2", None));
        let view = SidebarView::build(&fleet, Grouping::Project, &Filters::default());
        assert_eq!(view.pinned.len(), 1);
        assert_eq!(view.projects.len(), 1);
        let titles: Vec<_> = view.projects[0]
            .rows
            .iter()
            .map(|r| r.title.as_str())
            .collect();
        assert_eq!(titles, vec!["w2", "w10"]);
        assert!(view.multi_host);
    }

    #[test]
    fn natural_sort() {
        let mut names = vec!["w10", "W2", "w1"];
        names.sort_by(|a, b| natural(a, b));
        assert_eq!(names, vec!["w1", "W2", "w10"]);
    }
}
