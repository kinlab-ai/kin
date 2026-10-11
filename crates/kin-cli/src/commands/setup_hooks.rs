// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! The prompt hook `kin setup` offers Claude Code.
//!
//! Claude Code runs a `UserPromptSubmit` hook on every prompt and adds what
//! it prints to the model's context before the model's first turn. The hook
//! Kin offers runs `kin ask --hook claude`, which answers a prompt that names
//! code and asks something Kin routes, and adds nothing for any other prompt.
//! It lives in Claude Code's user settings, `~/.claude/settings.json`, beside
//! whatever hooks a person already keeps there: Kin owns one entry in the
//! `UserPromptSubmit` list, recognised by its command, and never touches any
//! other.
//!
//! Writing into a person's agent settings is their call, so `kin setup` asks
//! first, recommending it, and a non-interactive run writes it only when
//! `--prompt-hook` says so. This module only shapes the settings; the
//! installer in `setup.rs` reads and writes them.

use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use serde_json::{json, Map, Value};

/// The ledger target the hook is recorded under.
pub(crate) const CLAUDE_TARGET: &str = "claude";

/// The Claude Code event the hook runs on.
pub(crate) const PROMPT_HOOK_EVENT: &str = "UserPromptSubmit";

/// The seconds Claude Code gives the hook, stated so a client default can
/// never cut it shorter than the deadline `kin ask --hook` keeps itself.
pub(crate) const PROMPT_HOOK_TIMEOUT_SECONDS: u64 = 30;

/// The arguments after the launcher that make a hook Kin's.
const HOOK_ARGUMENTS: &str = "ask --hook claude";

/// Claude Code's user settings, where a hook kept for every project lives.
pub(crate) fn claude_settings_path(home: &Path) -> PathBuf {
    home.join(".claude").join("settings.json")
}

/// The command the hook runs: the launcher every Kin client entry names,
/// quoted when it carries a space, then `ask --hook claude`.
pub(crate) fn prompt_hook_command(launcher: &str) -> String {
    if launcher.contains(char::is_whitespace) && !launcher.starts_with('"') {
        format!("\"{launcher}\" {HOOK_ARGUMENTS}")
    } else {
        format!("{launcher} {HOOK_ARGUMENTS}")
    }
}

/// The hook entry Kin owns, as Claude Code reads it.
pub(crate) fn prompt_hook_entry(command: &str) -> Value {
    json!({
        "type": "command",
        "command": command,
        "timeout": PROMPT_HOOK_TIMEOUT_SECONDS,
    })
}

/// Whether a hook entry is Kin's: a command that ends by running
/// `ask --hook claude`.
fn is_kin_prompt_hook(hook: &Value) -> bool {
    hook.get("command")
        .and_then(Value::as_str)
        .is_some_and(|command| command.trim_end().ends_with(HOOK_ARGUMENTS))
}

/// What installing the hook changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PromptHookWrite {
    /// The hook was not there and now is.
    Added,
    /// Kin's hook was there with another command or timeout, and now runs
    /// this one.
    Replaced,
    /// Kin's hook was already there exactly as it would be written.
    Unchanged,
}

/// The `UserPromptSubmit` groups in a settings root, created when absent.
fn prompt_groups(root: &mut Map<String, Value>) -> Result<&mut Vec<Value>> {
    let hooks = root
        .entry("hooks")
        .or_insert_with(|| Value::Object(Map::new()));
    let Some(hooks) = hooks.as_object_mut() else {
        bail!("its `hooks` value is not an object");
    };
    let groups = hooks
        .entry(PROMPT_HOOK_EVENT)
        .or_insert_with(|| Value::Array(Vec::new()));
    let Some(groups) = groups.as_array_mut() else {
        bail!("its `hooks.{PROMPT_HOOK_EVENT}` value is not a list");
    };
    Ok(groups)
}

/// `root` with Kin's hook running `command`, and what that changed. A root
/// that is not an object, or whose hooks are not shaped as Claude Code
/// writes them, is refused rather than rewritten.
pub(crate) fn with_prompt_hook(
    root: Option<Value>,
    command: &str,
) -> Result<(Value, PromptHookWrite)> {
    let mut root = root.unwrap_or_else(|| Value::Object(Map::new()));
    let entry = prompt_hook_entry(command);
    let Some(object) = root.as_object_mut() else {
        bail!("it is not a JSON object");
    };
    let groups = prompt_groups(object)?;
    let mut write = None;
    for group in groups.iter_mut() {
        let Some(hooks) = group.get_mut("hooks").and_then(Value::as_array_mut) else {
            continue;
        };
        for hook in hooks.iter_mut().filter(|hook| is_kin_prompt_hook(hook)) {
            if write.is_none() {
                write = Some(if *hook == entry {
                    PromptHookWrite::Unchanged
                } else {
                    *hook = entry.clone();
                    PromptHookWrite::Replaced
                });
            }
        }
    }
    let write = match write {
        Some(write) => write,
        None => {
            groups.push(json!({ "hooks": [entry] }));
            PromptHookWrite::Added
        }
    };
    Ok((root, write))
}

/// `root` without Kin's hook: every Kin entry removed, and a group left with
/// no hooks removed with it. Whether anything was removed.
pub(crate) fn without_prompt_hook(root: &mut Value) -> bool {
    let Some(groups) = root
        .get_mut("hooks")
        .and_then(|hooks| hooks.get_mut(PROMPT_HOOK_EVENT))
        .and_then(Value::as_array_mut)
    else {
        return false;
    };
    let mut removed = false;
    for group in groups.iter_mut() {
        if let Some(hooks) = group.get_mut("hooks").and_then(Value::as_array_mut) {
            let before = hooks.len();
            hooks.retain(|hook| !is_kin_prompt_hook(hook));
            removed |= hooks.len() != before;
        }
    }
    groups.retain(|group| {
        group
            .get("hooks")
            .and_then(Value::as_array)
            .is_none_or(|hooks| !hooks.is_empty())
    });
    removed
}

/// Kin's hook entry in a settings root, when there is one.
pub(crate) fn current_prompt_hook(root: &Value) -> Option<Value> {
    root.get("hooks")?
        .get(PROMPT_HOOK_EVENT)?
        .as_array()?
        .iter()
        .filter_map(|group| group.get("hooks").and_then(Value::as_array))
        .flatten()
        .find(|hook| is_kin_prompt_hook(hook))
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    const LAUNCHER: &str = "/home/me/.kin/bin/kin";

    #[test]
    fn the_hook_joins_settings_beside_every_other_hook_and_key() {
        let settings = json!({
            "model": "opus",
            "hooks": {
                "UserPromptSubmit": [{"hooks": [{"type": "command", "command": "echo hi"}]}],
                "Stop": [{"hooks": [{"type": "command", "command": "say done"}]}]
            }
        });
        let command = prompt_hook_command(LAUNCHER);
        let (root, write) = with_prompt_hook(Some(settings), &command).unwrap();
        assert_eq!(write, PromptHookWrite::Added);
        assert_eq!(root["model"], "opus");
        assert_eq!(root["hooks"]["Stop"][0]["hooks"][0]["command"], "say done");
        let groups = root["hooks"]["UserPromptSubmit"].as_array().unwrap();
        assert_eq!(groups[0]["hooks"][0]["command"], "echo hi");
        assert_eq!(
            groups[1]["hooks"][0]["command"],
            format!("{LAUNCHER} ask --hook claude")
        );
        assert_eq!(groups[1]["hooks"][0]["timeout"], 30);
        assert_eq!(
            current_prompt_hook(&root),
            Some(prompt_hook_entry(&command))
        );
    }

    #[test]
    fn a_second_install_changes_nothing_and_a_new_launcher_replaces_kins_entry() {
        let command = prompt_hook_command(LAUNCHER);
        let (root, _) = with_prompt_hook(None, &command).unwrap();
        let (again, write) = with_prompt_hook(Some(root.clone()), &command).unwrap();
        assert_eq!(write, PromptHookWrite::Unchanged);
        assert_eq!(again, root);
        let moved = prompt_hook_command("/opt/kin/bin/kin");
        let (moved_root, write) = with_prompt_hook(Some(root), &moved).unwrap();
        assert_eq!(write, PromptHookWrite::Replaced);
        let groups = moved_root["hooks"]["UserPromptSubmit"].as_array().unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0]["hooks"][0]["command"], moved);
    }

    #[test]
    fn removal_takes_only_kins_entry_and_its_emptied_group() {
        let command = prompt_hook_command(LAUNCHER);
        let mine = json!({"type": "command", "command": "echo hi"});
        let settings = json!({"hooks": {"UserPromptSubmit": [{"hooks": [mine.clone()]}]}});
        let (mut root, _) = with_prompt_hook(Some(settings), &command).unwrap();
        assert!(without_prompt_hook(&mut root));
        assert_eq!(
            root["hooks"]["UserPromptSubmit"],
            json!([{"hooks": [mine]}])
        );
        assert!(!without_prompt_hook(&mut root));
        assert!(current_prompt_hook(&root).is_none());
    }

    #[test]
    fn settings_shaped_otherwise_are_refused_rather_than_rewritten() {
        let command = prompt_hook_command(LAUNCHER);
        for settings in [
            json!([]),
            json!({"hooks": []}),
            json!({"hooks": {"UserPromptSubmit": {}}}),
        ] {
            assert!(
                with_prompt_hook(Some(settings.clone()), &command).is_err(),
                "{settings}"
            );
        }
    }

    #[test]
    fn a_launcher_with_a_space_is_quoted() {
        assert_eq!(
            prompt_hook_command("/Users/A B/.kin/bin/kin"),
            "\"/Users/A B/.kin/bin/kin\" ask --hook claude"
        );
    }
}
