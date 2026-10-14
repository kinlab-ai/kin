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
//!
//! The same settings hold the edit hook, which `kin setup` writes whenever it
//! connects Claude Code. Synchronous `PreToolUse` and `PostToolUse` hooks run
//! `kin agents record-edit` around `Edit`, `Write` and `MultiEdit`: they tell
//! the repository's daemon which session made the edit and capture its before
//! and after state, so the entities the edit changed can name the agent in a
//! review. Kin owns one group in each event list, the one holding that command,
//! and touches no other.

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

// ---------------------------------------------------------------------------
// The edit hook
// ---------------------------------------------------------------------------

/// The Claude Code event the edit hook runs after.
pub(crate) const EDIT_HOOK_EVENT: &str = "PostToolUse";

/// The Claude Code event the edit hook runs before.
pub(crate) const PRE_EDIT_HOOK_EVENT: &str = "PreToolUse";

const EDIT_HOOK_EVENTS: [&str; 2] = [PRE_EDIT_HOOK_EVENT, EDIT_HOOK_EVENT];

/// The Claude Code tools the edit hooks run around: the ones that edit code.
pub(crate) const EDIT_HOOK_MATCHER: &str = "Edit|Write|MultiEdit";

/// The seconds Claude Code gives the edit hook. The hook sends one local
/// request and gives up on it well inside this.
pub(crate) const EDIT_HOOK_TIMEOUT_SECONDS: u64 = 10;

/// The command the edit hook runs: the launcher every Kin client entry
/// names, quoted when it carries a space, then `agents record-edit`.
pub(crate) fn edit_hook_command(launcher: &str) -> String {
    let arguments = super::agent_edits::EDIT_HOOK_ARGUMENTS;
    if launcher.contains(char::is_whitespace) && !launcher.starts_with('"') {
        format!("\"{launcher}\" {arguments}")
    } else {
        format!("{launcher} {arguments}")
    }
}

/// The synchronous group Kin owns in each edit-event list, as Claude Code
/// reads it. Claude Code's default is synchronous when `async` is absent.
pub(crate) fn edit_hook_group(command: &str) -> Value {
    json!({
        "matcher": EDIT_HOOK_MATCHER,
        "hooks": [{
            "type": "command",
            "command": command,
            "timeout": EDIT_HOOK_TIMEOUT_SECONDS,
        }],
    })
}

/// Whether a hook entry is Kin's edit hook: a command that ends by running
/// `agents record-edit` for Claude Code.
fn is_kin_edit_hook(hook: &Value) -> bool {
    hook.get("command")
        .and_then(Value::as_str)
        .is_some_and(|command| {
            command
                .trim_end()
                .ends_with(super::agent_edits::EDIT_HOOK_ARGUMENTS)
        })
}

fn holds_kin_edit_hook(group: &Value) -> bool {
    group
        .get("hooks")
        .and_then(Value::as_array)
        .is_some_and(|hooks| hooks.iter().any(is_kin_edit_hook))
}

/// `root` with Kin's paired edit hooks running `command`, and what that changed.
/// Each group is written whole: its matcher and its one hook. A Kin entry found
/// among a person's own hooks in another group is taken out of that group and
/// their hooks are left as they were. Settings not shaped as Claude Code
/// writes them are refused rather than rewritten.
pub(crate) fn with_edit_hook(
    root: Option<Value>,
    command: &str,
) -> Result<(Value, PromptHookWrite)> {
    let mut root = root.unwrap_or_else(|| Value::Object(Map::new()));
    let group = edit_hook_group(command);
    let Some(object) = root.as_object_mut() else {
        bail!("it is not a JSON object");
    };
    let hooks = object
        .entry("hooks")
        .or_insert_with(|| Value::Object(Map::new()));
    let Some(hooks) = hooks.as_object_mut() else {
        bail!("its `hooks` value is not an object");
    };
    let mut existing = false;
    let mut changed = false;
    for event in EDIT_HOOK_EVENTS {
        let groups = hooks
            .entry(event)
            .or_insert_with(|| Value::Array(Vec::new()));
        let Some(groups) = groups.as_array_mut() else {
            bail!("its `hooks.{event}` value is not a list");
        };
        let held = groups
            .iter()
            .filter(|held| holds_kin_edit_hook(held))
            .count();
        existing |= held != 0;
        if held == 1 && groups.iter().any(|held| *held == group) {
            continue;
        }
        changed = true;
        for held in groups.iter_mut() {
            if let Some(entries) = held.get_mut("hooks").and_then(Value::as_array_mut) {
                entries.retain(|hook| !is_kin_edit_hook(hook));
            }
        }
        groups.retain(|held| {
            held.get("hooks")
                .and_then(Value::as_array)
                .is_none_or(|entries| !entries.is_empty())
        });
        groups.push(group.clone());
    }
    let write = if !changed {
        PromptHookWrite::Unchanged
    } else if !existing {
        PromptHookWrite::Added
    } else {
        PromptHookWrite::Replaced
    };
    Ok((root, write))
}

/// `root` without Kin's edit hooks: every Kin entry removed, and a group left
/// with no hooks removed with it. Whether anything was removed.
pub(crate) fn without_edit_hook(root: &mut Value) -> bool {
    let mut removed = false;
    for event in EDIT_HOOK_EVENTS {
        let Some(groups) = root
            .get_mut("hooks")
            .and_then(|hooks| hooks.get_mut(event))
            .and_then(Value::as_array_mut)
        else {
            continue;
        };
        for group in groups.iter_mut() {
            if let Some(hooks) = group.get_mut("hooks").and_then(Value::as_array_mut) {
                let before = hooks.len();
                hooks.retain(|hook| !is_kin_edit_hook(hook));
                removed |= hooks.len() != before;
            }
        }
        groups.retain(|group| {
            group
                .get("hooks")
                .and_then(Value::as_array)
                .is_none_or(|hooks| !hooks.is_empty())
        });
    }
    removed
}

/// Every group holding Kin's edit hook, keyed by event, when any remain. Both
/// event lists are fingerprinted together, including unexpected duplicates,
/// so a partial pair or changed matcher cannot verify as an intact install.
pub(crate) fn current_edit_hook(root: &Value) -> Option<Value> {
    let hooks = root.get("hooks")?;
    let mut owned = Map::new();
    for event in EDIT_HOOK_EVENTS {
        let Some(groups) = hooks.get(event).and_then(Value::as_array) else {
            continue;
        };
        let groups: Vec<_> = groups
            .iter()
            .filter(|group| holds_kin_edit_hook(group))
            .cloned()
            .collect();
        if !groups.is_empty() {
            owned.insert(event.to_owned(), Value::Array(groups));
        }
    }
    (!owned.is_empty()).then_some(Value::Object(owned))
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

    #[test]
    fn the_edit_hook_is_its_own_group_beside_every_other_hook() {
        let settings = json!({
            "hooks": {
                "PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "echo ready"}]}],
                "PostToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "echo ran"}]}],
                "UserPromptSubmit": [{"hooks": [{"type": "command", "command": "echo hi"}]}]
            }
        });
        let command = edit_hook_command(LAUNCHER);
        assert_eq!(
            command,
            format!("{LAUNCHER} agents record-edit --client claude")
        );
        let (root, write) = with_edit_hook(Some(settings), &command).unwrap();
        assert_eq!(write, PromptHookWrite::Added);
        for event in EDIT_HOOK_EVENTS {
            let groups = root["hooks"][event].as_array().unwrap();
            assert_eq!(groups.len(), 2);
            assert_eq!(groups[0]["matcher"], "Bash");
            assert_eq!(groups[1]["matcher"], "Edit|Write|MultiEdit");
            assert_eq!(groups[1]["hooks"][0]["command"], command);
            assert_eq!(groups[1]["hooks"][0]["timeout"], 10);
            assert!(groups[1]["hooks"][0].get("async").is_none());
        }
        assert_eq!(
            root["hooks"]["PreToolUse"][0]["hooks"][0]["command"],
            "echo ready"
        );
        assert_eq!(
            root["hooks"]["PostToolUse"][0]["hooks"][0]["command"],
            "echo ran"
        );
        assert_eq!(
            root["hooks"]["UserPromptSubmit"][0]["hooks"][0]["command"],
            "echo hi"
        );
        assert_eq!(
            current_edit_hook(&root),
            Some(json!({
                "PreToolUse": [edit_hook_group(&command)],
                "PostToolUse": [edit_hook_group(&command)],
            }))
        );

        let (again, write) = with_edit_hook(Some(root.clone()), &command).unwrap();
        assert_eq!(write, PromptHookWrite::Unchanged);
        assert_eq!(again, root);

        let moved = edit_hook_command("/opt/kin/bin/kin");
        let (mut moved_root, write) = with_edit_hook(Some(root), &moved).unwrap();
        assert_eq!(write, PromptHookWrite::Replaced);
        for event in EDIT_HOOK_EVENTS {
            let groups = moved_root["hooks"][event].as_array().unwrap();
            assert_eq!(groups.len(), 2, "one Kin group per event, never two");
            assert_eq!(groups[1]["hooks"][0]["command"], moved);
        }

        assert!(without_edit_hook(&mut moved_root));
        assert_eq!(
            moved_root["hooks"]["PreToolUse"],
            json!([{"matcher": "Bash", "hooks": [{"type": "command", "command": "echo ready"}]}])
        );
        assert_eq!(
            moved_root["hooks"]["PostToolUse"],
            json!([{"matcher": "Bash", "hooks": [{"type": "command", "command": "echo ran"}]}])
        );
        assert!(!without_edit_hook(&mut moved_root));
        assert!(current_edit_hook(&moved_root).is_none());
    }

    #[test]
    fn a_kin_edit_hook_a_person_moved_among_their_own_is_taken_back_out() {
        let command = edit_hook_command(LAUNCHER);
        let theirs = json!({"type": "command", "command": "echo edited"});
        let mixed = json!({
            "matcher": "Edit",
            "hooks": [theirs.clone(), {"type": "command", "command": command}],
        });
        let settings = json!({"hooks": {
            "PreToolUse": [mixed.clone()],
            "PostToolUse": [mixed],
        }});
        let (mut root, write) = with_edit_hook(Some(settings), &command).unwrap();
        assert_eq!(write, PromptHookWrite::Replaced);
        for event in EDIT_HOOK_EVENTS {
            assert_eq!(
                root["hooks"][event],
                json!([{"matcher": "Edit", "hooks": [theirs]}, edit_hook_group(&command)])
            );
        }
        assert!(without_edit_hook(&mut root));
        for event in EDIT_HOOK_EVENTS {
            assert_eq!(
                root["hooks"][event],
                json!([{"matcher": "Edit", "hooks": [theirs]}])
            );
        }
        for settings in [
            json!([]),
            json!({"hooks": []}),
            json!({"hooks": {"PreToolUse": {}}}),
            json!({"hooks": {"PostToolUse": {}}}),
        ] {
            assert!(
                with_edit_hook(Some(settings.clone()), &command).is_err(),
                "{settings}"
            );
        }
    }

    #[test]
    fn a_post_only_install_is_upgraded_without_duplicating_its_group() {
        let command = edit_hook_command(LAUNCHER);
        let post = edit_hook_group(&command);
        let settings = json!({"hooks": {"PostToolUse": [post.clone()]}});
        let (root, write) = with_edit_hook(Some(settings), &command).unwrap();
        assert_eq!(write, PromptHookWrite::Replaced);
        assert_eq!(root["hooks"]["PostToolUse"], json!([post]));
        assert_eq!(
            root["hooks"]["PreToolUse"],
            json!([edit_hook_group(&command)])
        );
        let (again, write) = with_edit_hook(Some(root.clone()), &command).unwrap();
        assert_eq!(write, PromptHookWrite::Unchanged);
        assert_eq!(again, root);
    }

    #[test]
    fn duplicate_edit_groups_are_collapsed_and_all_owned_groups_are_observed() {
        let command = edit_hook_command(LAUNCHER);
        let group = edit_hook_group(&command);
        let settings = json!({"hooks": {
            "PreToolUse": [group.clone(), group.clone()],
            "PostToolUse": [group.clone()],
        }});
        assert_eq!(
            current_edit_hook(&settings).unwrap()["PreToolUse"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        let (root, write) = with_edit_hook(Some(settings), &command).unwrap();
        assert_eq!(write, PromptHookWrite::Replaced);
        for event in EDIT_HOOK_EVENTS {
            assert_eq!(root["hooks"][event], json!([group]));
        }
    }
}
