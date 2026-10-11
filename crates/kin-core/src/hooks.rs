// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Hook template generation for assistant CLIs.
//!
//! Generates recommended hook configurations that remind coding agents
//! to use Kin tools instead of raw file operations.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookTemplate {
    pub event: String,
    pub matcher: Option<String>,
    pub command: String,
    pub description: String,
}

/// Generate recommended Claude Code hook templates.
///
/// The `UserPromptSubmit` hook is Kin's answer before the model's first turn:
/// `kin ask --hook claude` reads the prompt, answers it when it names code and
/// asks something Kin routes, and adds nothing otherwise. `kin setup` offers
/// that one hook and records it; the others are reminders a person can add.
pub fn generate_claude_hooks() -> Vec<HookTemplate> {
    vec![
        HookTemplate {
            event: "SessionStart".into(),
            matcher: None,
            command: "echo 'This repository is held in Kin. Ask code questions in plain words with the Kin ask tool or `kin ask \"<question>\"`; for a single step use `kin locate`, `kin refs`, `kin trace`, `kin impact`, `kin context` or `kin source`. Record changes with `kin commit` rather than `git commit`.'".into(),
            description: "Orients the agent at session start: ask first, then the verbs for single steps.".into(),
        },
        HookTemplate {
            event: "UserPromptSubmit".into(),
            matcher: None,
            command: "kin ask --hook claude".into(),
            description: "Kin answers a code question before the model's first turn, and adds nothing for any other prompt. `kin setup` offers this hook; `kin setup --prompt-hook` adds it without asking.".into(),
        },
        HookTemplate {
            event: "SubagentStart".into(),
            matcher: None,
            command: "echo 'Kin: before exploring broadly, ask the code question with the Kin ask tool or `kin ask \"<question>\"`; for a single step use `kin locate`, `kin refs` or `kin trace`.'".into(),
            description: "Keeps subagents from starting with broad exploration.".into(),
        },
        HookTemplate {
            event: "PreToolUse".into(),
            matcher: Some("Edit|Write".into()),
            command: "echo 'This repository is held in Kin: change code through Kin by entity rather than by editing text in place. Read the entity with source for its source base, then mutate it by id, and check what the change affects with `kin impact`.'".into(),
            description: "Redirects a text edit to Kin's entity edit through mutate.".into(),
        },
        HookTemplate {
            event: "PostToolUse".into(),
            matcher: Some("Bash".into()),
            command: r#"if echo "$TOOL_INPUT" | grep -q 'git commit'; then echo 'Use `kin commit` instead of `git commit` to keep semantic history.'; fi"#.into(),
            description: "Reminds the agent to record changes with `kin commit`.".into(),
        },
    ]
}

/// Render hook templates as Claude Code settings.json hooks format.
pub fn render_hooks_json(hooks: &[HookTemplate]) -> String {
    let mut events: std::collections::BTreeMap<&str, Vec<serde_json::Value>> =
        std::collections::BTreeMap::new();

    for hook in hooks {
        let hook_entry = serde_json::json!({
            "type": "command",
            "command": hook.command,
        });

        let entry = if let Some(ref matcher) = hook.matcher {
            serde_json::json!({
                "matcher": matcher,
                "hooks": [hook_entry],
            })
        } else {
            serde_json::json!({
                "hooks": [hook_entry],
            })
        };

        events.entry(&hook.event).or_default().push(entry);
    }

    let hooks_obj: serde_json::Map<String, serde_json::Value> = events
        .into_iter()
        .map(|(k, v)| (k.to_string(), serde_json::Value::Array(v)))
        .collect();

    let root = serde_json::json!({ "hooks": hooks_obj });
    serde_json::to_string_pretty(&root).expect("hook templates are always serializable")
}

/// Render a human-readable setup guide for the hook templates.
pub fn render_hooks_instructions(hooks: &[HookTemplate]) -> String {
    let mut out = String::new();
    out.push_str("# Kin hooks for Claude Code\n\n");
    out.push_str(
        "Add these hooks to your `.claude/settings.json`. `kin setup` offers the prompt hook \
         and records it, so `kin setup uninstall` removes it.\n\n",
    );

    for (i, hook) in hooks.iter().enumerate() {
        out.push_str(&format!("## Hook {}: {} ", i + 1, hook.event));
        if let Some(ref m) = hook.matcher {
            out.push_str(&format!("(matcher: {})", m));
        }
        out.push('\n');
        out.push_str(&format!("  {}\n\n", hook.description));
    }

    out.push_str("## Quick Start\n\n");
    out.push_str("1. Copy the JSON output from `kin assistant hooks claude-code`.\n");
    out.push_str("2. Merge the `hooks` object into your `.claude/settings.json`.\n");
    out.push_str("3. The Kin CLI commands the hooks name:\n");
    out.push_str(
        "   - `kin ask \"<question>\"`: a code question in plain words, answered in one reply\n",
    );
    out.push_str("   - `kin locate`: find an entity by name or by what it does\n");
    out.push_str("   - `kin refs`: callers, importers and references of one entity\n");
    out.push_str("   - `kin trace`: everything that can end up calling one entity\n");
    out.push_str("   - `kin impact`: what a change to an entity affects\n");
    out.push_str("   - `kin context`: the code around an entity\n");
    out.push_str("   - `kin commit`: record a semantic change\n");
    out.push_str("   - `kin review`: review semantic impact before a change lands\n");

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_claude_hooks_count() {
        let hooks = generate_claude_hooks();
        assert_eq!(hooks.len(), 5, "expected 5 hook templates");
    }

    #[test]
    fn test_render_hooks_json_valid() {
        let hooks = generate_claude_hooks();
        let json_str = render_hooks_json(&hooks);
        let parsed: serde_json::Value =
            serde_json::from_str(&json_str).expect("render_hooks_json must produce valid JSON");
        assert!(
            parsed.get("hooks").is_some(),
            "root must contain 'hooks' key"
        );
        let hooks_obj = parsed["hooks"].as_object().unwrap();
        assert!(hooks_obj.contains_key("UserPromptSubmit"));
        assert!(hooks_obj.contains_key("SubagentStart"));
        assert!(hooks_obj.contains_key("PreToolUse"));
        assert!(hooks_obj.contains_key("PostToolUse"));
        assert!(hooks_obj.contains_key("SessionStart"));
    }

    #[test]
    fn test_hooks_mention_kin_commands() {
        let hooks = generate_claude_hooks();
        let instructions = render_hooks_instructions(&hooks);
        for command in [
            "kin ask",
            "kin locate",
            "kin refs",
            "kin trace",
            "kin impact",
            "kin context",
            "kin commit",
            "kin review",
        ] {
            assert!(instructions.contains(command), "must mention {command}");
        }
    }

    #[test]
    fn every_echo_is_one_single_quoted_word_a_shell_runs_whole() {
        for hook in generate_claude_hooks() {
            if let Some(quoted) = hook.command.strip_prefix("echo '") {
                let body = quoted.strip_suffix('\'').expect("the echo ends its quote");
                assert!(!body.contains('\''), "{}", hook.command);
            }
        }
    }

    #[test]
    fn the_prompt_hook_is_kins_answer_before_the_first_turn() {
        let hooks = generate_claude_hooks();
        let prompt = hooks
            .iter()
            .find(|hook| hook.event == "UserPromptSubmit")
            .expect("a prompt hook");
        assert_eq!(prompt.command, "kin ask --hook claude");
    }

    #[test]
    fn the_templates_name_no_retired_command_and_carry_no_emoji_or_dash() {
        let hooks = generate_claude_hooks();
        let all: String = hooks
            .iter()
            .map(|hook| format!("{} {}", hook.command, hook.description))
            .chain([render_hooks_instructions(&hooks)])
            .collect();
        for retired in [
            "--compact",
            "kin overview",
            "kin search",
            "get_entity_source",
            "kin_mutate",
        ] {
            assert!(!all.contains(retired), "names {retired}");
        }
        assert!(!all.contains('\u{2014}'), "an em dash");
        assert!(
            all.chars().all(|character| (character as u32) < 0x2600),
            "an emoji"
        );
        assert!(!all.to_lowercase().contains("editing files"));
    }
}
