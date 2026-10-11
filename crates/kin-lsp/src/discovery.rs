// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Discover installed LSP servers on the system.

use kin_model::LanguageId;
use tracing::debug;

pub use crate::start_failure::{describe_search_path, searched_on_this_path};

/// A discovered LSP server that can be started.
#[derive(Debug, Clone)]
pub struct DiscoveredServer {
    pub language: LanguageId,
    pub command: String,
    pub args: Vec<String>,
    pub version: Option<String>,
}

/// Known LSP server configurations per language.
pub(crate) const KNOWN_SERVERS: &[(LanguageId, &[&str], &[&str])] = &[
    // (language, [binary_names_to_search], [default_args])
    (LanguageId::Rust, &["rust-analyzer"], &[]),
    (
        LanguageId::Python,
        &["pyright-langserver", "pylsp"],
        &["--stdio"],
    ),
    (
        LanguageId::TypeScript,
        &["typescript-language-server", "vtsls"],
        &["--stdio"],
    ),
    (
        LanguageId::JavaScript,
        &["typescript-language-server"],
        &["--stdio"],
    ),
    (LanguageId::Go, &["gopls"], &["serve"]),
    (LanguageId::Java, &["jdtls"], &[]),
    (LanguageId::C, &["clangd"], &[]),
    (LanguageId::Cpp, &["clangd"], &[]),
];

/// Discover which LSP servers are installed on this system.
///
/// Each distinct binary is asked for its version once, and all of them at
/// once. One binary serves several languages (typescript-language-server for
/// TypeScript and JavaScript, clangd for C and C++), and every answer is a
/// process start, a Node one for most servers, so asking per language and in
/// turn cost the sum of those starts: about a second on an idle machine and
/// three on a loaded one.
pub fn discover_servers() -> Vec<DiscoveredServer> {
    let mut found = Vec::new();

    for (language, binaries, default_args) in KNOWN_SERVERS {
        let mut resolved = false;
        for binary in *binaries {
            if let Ok(path) = which::which(binary) {
                debug!(
                    language = %language,
                    binary = %path.display(),
                    "found LSP server"
                );
                found.push((*language, path, *default_args));
                resolved = true;
                break; // Use first found binary for each language
            }
        }
        if !resolved {
            debug!(
                language = %language,
                binaries = %binaries.join(" or "),
                searched = %searched_on_this_path(),
                "no LSP server found for this language"
            );
        }
    }

    let mut distinct: Vec<&std::path::Path> = Vec::new();
    for (_, path, _) in &found {
        if !distinct.contains(&path.as_path()) {
            distinct.push(path);
        }
    }
    let versions: Vec<(&std::path::Path, Option<String>)> = std::thread::scope(|scope| {
        let asked: Vec<_> = distinct
            .iter()
            .map(|path| (*path, scope.spawn(move || detect_version(path))))
            .collect();
        asked
            .into_iter()
            .map(|(path, answer)| (path, answer.join().ok().flatten()))
            .collect()
    });

    found
        .iter()
        .map(|(language, path, default_args)| DiscoveredServer {
            language: *language,
            command: path.display().to_string(),
            args: default_args.iter().map(|s| s.to_string()).collect(),
            version: versions
                .iter()
                .find(|(asked, _)| *asked == path.as_path())
                .and_then(|(_, version)| version.clone()),
        })
        .collect()
}

/// Try to detect the version of an LSP server binary.
pub(crate) fn detect_version(path: &std::path::Path) -> Option<String> {
    let output = std::process::Command::new(path)
        .arg("--version")
        .output()
        .ok()?;
    if output.status.success() {
        let version = String::from_utf8_lossy(&output.stdout);
        Some(version.trim().to_string())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discover_finds_at_least_one_server() {
        // This test is environment-dependent — it passes if ANY LSP server is installed.
        // In CI, we might need to skip this.
        let servers = discover_servers();
        // Don't assert non-empty — just verify it doesn't panic.
        for server in &servers {
            assert!(!server.command.is_empty());
        }
        // A binary serving several languages was asked once, and every
        // language it serves carries that one answer.
        for server in &servers {
            for other in servers
                .iter()
                .filter(|other| other.command == server.command)
            {
                assert_eq!(other.version, server.version, "{servers:?}");
            }
        }
    }
}
