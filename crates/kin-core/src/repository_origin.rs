// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Where a repository says it lives: the host, owner and name its Git remote
//! names, such as `github.com`, `pallets` and `itsdangerous`. One parser for
//! every caller, so the workspace page and `kin store list` cannot disagree.
//!
//! The remote comes from Kin's own sealed configuration. Only when that
//! records no remote at all is the repository's Git configuration read for
//! `origin`, as a configuration boundary. Nothing else is guessed: a
//! repository without a remote has no origin.

use std::path::Path;

use gix::bstr::ByteSlice;
use serde::Serialize;

use crate::config::{GitCoexistenceConfig, KinConfig};
use crate::layout::KinLayout;

/// The host, owner and name of a repository's remote. `owner` is every path
/// segment before the name, so a nested group on a host that has them reads
/// as `group/sub`. It is absent when the remote names only the repository.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RepositoryOrigin {
    pub host: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    pub name: String,
}

/// Parse a Git remote URL into its origin. HTTPS, SSH (including the short
/// `git@host:owner/name` form) and `git://` URLs answer; a local path or a
/// `file://` URL has no host and answers `None`, as does anything unparsable.
/// Credentials and ports are never part of the answer.
pub fn parse_remote_url(url: &str) -> Option<RepositoryOrigin> {
    if url
        .bytes()
        .any(|byte| byte.is_ascii_control() || byte == b'?' || byte == b'#')
    {
        return None;
    }
    let parsed = gix::Url::try_from(url).ok()?;
    match parsed.scheme {
        gix::url::Scheme::Ssh
        | gix::url::Scheme::Git
        | gix::url::Scheme::Http
        | gix::url::Scheme::Https => {}
        gix::url::Scheme::File | gix::url::Scheme::Ext(_) => return None,
    }
    let host = parsed.host()?.trim().to_ascii_lowercase();
    if host.is_empty() {
        return None;
    }
    let path = parsed.path.to_str().ok()?;
    let mut segments: Vec<&str> = path
        .split('/')
        .map(str::trim)
        .filter(|segment| !segment.is_empty() && *segment != "~")
        .collect();
    let last = segments.pop()?;
    let name = last.strip_suffix(".git").unwrap_or(last);
    if name.is_empty() {
        return None;
    }
    let owner = (!segments.is_empty()).then(|| segments.join("/"));
    Some(RepositoryOrigin {
        host,
        owner,
        name: name.to_string(),
    })
}

/// The origin Kin's sealed Git configuration records: the remote named
/// `origin`, or the only remote when there is exactly one. Its first fetch
/// URL is read, then its first push URL.
pub fn recorded_origin(git: &GitCoexistenceConfig) -> Option<RepositoryOrigin> {
    let remote = git
        .remotes
        .iter()
        .find(|remote| remote.name == "origin")
        .or(match git.remotes.as_slice() {
            [only] => Some(only),
            _ => None,
        })?;
    let url = remote
        .fetch_urls
        .first()
        .or_else(|| remote.push_urls.first())?;
    parse_remote_url(url)
}

/// The origin of the repository at `layout`: what Kin's configuration
/// records, or, when it records no remote at all, the repository's own Git
/// `origin`. `None` when neither names one.
pub fn repository_origin(layout: &KinLayout) -> Option<RepositoryOrigin> {
    if let Ok(config) = KinConfig::load(&layout.config_path()) {
        if !config.git.remotes.is_empty() {
            return recorded_origin(&config.git);
        }
    }
    git_config_origin_url(layout.working_dir()).and_then(|url| parse_remote_url(&url))
}

/// The fetch URL of `origin` in the repository's Git configuration. A
/// configuration boundary, read only when Kin's own records hold no remote.
fn git_config_origin_url(working_dir: &Path) -> Option<String> {
    let repository = gix::open(working_dir).ok()?;
    let remote = repository.find_remote("origin").ok()?;
    let url = remote.url(gix::remote::Direction::Fetch)?;
    Some(url.to_bstring().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::GitRemoteTransportConfig;

    fn origin(host: &str, owner: Option<&str>, name: &str) -> Option<RepositoryOrigin> {
        Some(RepositoryOrigin {
            host: host.into(),
            owner: owner.map(Into::into),
            name: name.into(),
        })
    }

    #[test]
    fn https_and_ssh_remotes_name_their_owner_and_repository() {
        for url in [
            "https://github.com/pallets/itsdangerous.git",
            "https://github.com/pallets/itsdangerous",
            "https://github.com/pallets/itsdangerous/",
            "https://GitHub.com:443/pallets/itsdangerous.git",
            "git@github.com:pallets/itsdangerous.git",
            "ssh://git@github.com/pallets/itsdangerous.git",
            "ssh://github.com:22/pallets/itsdangerous",
            "git://github.com/pallets/itsdangerous.git",
        ] {
            assert_eq!(
                parse_remote_url(url),
                origin("github.com", Some("pallets"), "itsdangerous"),
                "{url}"
            );
        }
    }

    #[test]
    fn a_nested_group_is_the_owner_and_credentials_never_show() {
        assert_eq!(
            parse_remote_url("https://gitlab.com/group/sub/repo.git"),
            origin("gitlab.com", Some("group/sub"), "repo")
        );
        assert_eq!(
            parse_remote_url("git@gitlab.example.org:platform/tools/kin.git"),
            origin("gitlab.example.org", Some("platform/tools"), "kin")
        );
        let with_token = parse_remote_url("https://user:secret@github.com/acme/app.git").unwrap();
        assert_eq!(
            with_token,
            origin("github.com", Some("acme"), "app").unwrap()
        );
        assert!(!format!("{with_token:?}").contains("secret"));
        assert_eq!(
            parse_remote_url("https://example.com/solo.git"),
            origin("example.com", None, "solo")
        );
    }

    #[test]
    fn no_remote_or_a_local_one_has_no_origin() {
        for url in [
            "",
            "/srv/git/project.git",
            "../project",
            "file:///srv/git/project.git",
            "https://github.com/",
            "https://github.com/owner/repo?ref=x",
        ] {
            assert_eq!(parse_remote_url(url), None, "{url:?}");
        }
        assert_eq!(recorded_origin(&GitCoexistenceConfig::default()), None);
    }

    #[test]
    fn the_recorded_origin_is_origin_or_the_only_remote() {
        let remote = |name: &str, url: &str| GitRemoteTransportConfig {
            name: name.into(),
            fetch_urls: vec![url.into()],
            push_urls: Vec::new(),
            fetch_refspecs: Vec::new(),
            push_refspecs: Vec::new(),
        };
        let mut git = GitCoexistenceConfig {
            remotes: vec![
                remote("fork", "https://github.com/someone/itsdangerous.git"),
                remote("origin", "https://github.com/pallets/itsdangerous.git"),
            ],
            ..GitCoexistenceConfig::default()
        };
        assert_eq!(
            recorded_origin(&git),
            origin("github.com", Some("pallets"), "itsdangerous")
        );
        git.remotes.remove(1);
        assert_eq!(
            recorded_origin(&git),
            origin("github.com", Some("someone"), "itsdangerous"),
            "the only remote stands in for origin"
        );
        git.remotes
            .push(remote("upstream", "https://github.com/pallets/x.git"));
        assert_eq!(
            recorded_origin(&git),
            None,
            "two remotes and no origin name none"
        );
    }
}
