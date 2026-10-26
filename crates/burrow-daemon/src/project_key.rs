//! Paseo's `projectKey`: an opaque key that joins the same repository across
//! hosts (`deriveProjectKey` in `project-key.ts`). Clients merge projects that
//! share it, and Paseo's app adds projects only on a daemon that advertises
//! it as stable (`stableProjectIdentity`).
//!
//! - A checkout with a parseable `origin` is `remote:<host>[:<port>]/<path>`,
//!   the path lowercased on GitHub, with `#subdir:<rel>` when the project is a
//!   folder inside the checkout.
//! - Anything else is `host:<serverId>:<real path>`.

use std::path::Path;

use crate::git::{self, GitFacts};

/// Hosts Paseo's forge manifest lists for GitHub.
const GITHUB_HOSTS: &[&str] = &["github.com", "ssh.github.com"];

/// The key for a project rooted at `root_path`, from its checkout facts.
pub fn derive(root_path: &str, git: Option<&GitFacts>, server_id: &str) -> String {
    let remote = git
        .and_then(|g| g.remote_url.as_deref())
        .and_then(parse_remote);
    let selected = git
        .and_then(|g| git::relative_inside(Path::new(&g.worktree_root), Path::new(root_path)))
        .map(|rel| rel.to_string_lossy().replace('\\', "/"))
        .filter(|rel| !rel.is_empty());
    if let Some(remote) = remote {
        let host = match &remote.port {
            Some(port) => format!("{}:{port}", remote.host),
            None => remote.host.clone(),
        };
        let path = if GITHUB_HOSTS.contains(&remote.host.as_str()) {
            remote.path.to_lowercase()
        } else {
            remote.path
        };
        let key = format!("remote:{host}/{path}");
        return match selected {
            Some(rel) => format!("{key}#subdir:{rel}"),
            None => key,
        };
    }
    let main = git.and_then(|g| g.main_repo_root.as_deref());
    let local = match (&selected, main) {
        (Some(rel), Some(main)) => git::canonical(&Path::new(main).join(rel)),
        _ => git::canonical(Path::new(root_path)),
    };
    let local = local.to_string_lossy();
    if server_id.is_empty() {
        local.into_owned()
    } else {
        format!("host:{server_id}:{local}")
    }
}

/// `parseGitRemoteLocation`: the host, port and path of a remote URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Remote {
    pub host: String,
    pub port: Option<String>,
    pub path: String,
}

pub fn parse_remote(url: &str) -> Option<Remote> {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        return None;
    }
    // scp form (`git@host:owner/repo`) has no scheme.
    if !trimmed.contains("://") {
        let (user_host, path) = trimmed.split_once(':')?;
        let (_, host) = user_host.split_once('@')?;
        if host.contains('@') || user_host.starts_with('@') {
            return None;
        }
        let host = normalize_host(host);
        let path = normalize_path(path)?;
        return valid_host(&host).then_some(Remote {
            host,
            port: None,
            path,
        });
    }
    let (scheme, rest) = trimmed.split_once("://")?;
    let default_port = match scheme.to_ascii_lowercase().as_str() {
        "https" => "443",
        "http" => "80",
        "ssh" => "22",
        _ => return None,
    };
    let (authority, path) = match rest.find('/') {
        Some(at) => (&rest[..at], &rest[at..]),
        None => (rest, ""),
    };
    let authority = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) if port.chars().all(|c| c.is_ascii_digit()) => (host, Some(port)),
        _ => (authority, None),
    };
    let path = path.split(['?', '#']).next().unwrap_or_default();
    let host = normalize_host(host);
    let path = normalize_path(&percent_decode(path)?)?;
    let port = port
        .filter(|p| !p.is_empty() && *p != default_port)
        .map(str::to_string);
    valid_host(&host).then_some(Remote { host, port, path })
}

fn normalize_host(host: &str) -> String {
    host.trim().trim_end_matches('.').to_lowercase()
}

fn normalize_path(path: &str) -> Option<String> {
    let trimmed = path.trim().trim_matches('/');
    let trimmed = trimmed.strip_suffix(".git").unwrap_or(trimmed);
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn valid_host(host: &str) -> bool {
    let bytes = host.as_bytes();
    let edge = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit();
    !bytes.is_empty()
        && edge(bytes[0])
        && edge(bytes[bytes.len() - 1])
        && bytes
            .iter()
            .all(|&b| edge(b) || b == b'.' || b == b'_' || b == b'-')
}

fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = text.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(root: &str, remote: Option<&str>) -> GitFacts {
        GitFacts {
            worktree_root: root.into(),
            branch: Some("main".into()),
            remote_url: remote.map(str::to_string),
            main_repo_root: None,
            paseo_owned: false,
        }
    }

    #[test]
    fn remotes_parse_as_paseo_parses_them() {
        let scp = parse_remote("git@github.com:Owner/Repo.git").unwrap();
        assert_eq!(
            (scp.host.as_str(), scp.path.as_str()),
            ("github.com", "Owner/Repo")
        );
        let https = parse_remote("https://example.org:8443/group/sub/repo.git").unwrap();
        assert_eq!(https.port.as_deref(), Some("8443"));
        assert_eq!(https.path, "group/sub/repo");
        let default_port = parse_remote("ssh://git@example.org:22/x/y").unwrap();
        assert_eq!(default_port.port, None);
        assert_eq!(parse_remote("file:///tmp/repo"), None);
        assert_eq!(parse_remote("/tmp/repo"), None);
    }

    #[test]
    fn keys_join_remotes_and_scope_local_paths_to_the_host() {
        let dir = std::env::temp_dir();
        let root = dir.to_string_lossy().into_owned();
        let github = facts(&root, Some("git@github.com:Owner/Repo.git"));
        assert_eq!(
            derive(&root, Some(&github), "srv_x"),
            "remote:github.com/owner/repo"
        );
        let sub = dir.join("pkg").to_string_lossy().into_owned();
        assert_eq!(
            derive(&sub, Some(&github), "srv_x"),
            "remote:github.com/owner/repo#subdir:pkg"
        );
        let local = derive(&root, None, "srv_x");
        assert!(local.starts_with("host:srv_x:"), "{local}");
    }
}
