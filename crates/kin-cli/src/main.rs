// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use anyhow::{Context, Result};
use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::{self, Shell};
use kin_cli::commands;
use kin_cli::{eprintln, println};
use std::io::IsTerminal;
use std::path::PathBuf;
use tracing::Instrument;
use tracing_subscriber::filter::FilterExt;
use tracing_subscriber::prelude::*;
use tracing_subscriber::EnvFilter;

kin_buildinfo::embed_update_build_identity!(
    KIN_UPDATE_BUILD_IDENTITY,
    env!("CARGO_PKG_VERSION"),
    kin_db::GraphSnapshot::CURRENT_VERSION
);

/// First-use guidance precedes the complete command index. The first line is
/// the brand canon's definition, shared with the terminal version display.
const ORIENTATION: &str = "\
A graph-native code repository for AI agents and the people who build with them.

Start here:
  kin setup           set up AI clients and your shell
  kin init            start Kin in this repository
  kin clone <source>  clone a repository into Kin
  kin status          see changes and Kin's progress
  kin commit          record your change
  kin log / kin diff  read the history and what changed

Ask the graph:
  kin locate / search / trace / impact / refs / context

Use an editor or AI:
  kin open code         open an editor in a session workspace
  kin with codex        start an assistant in a session workspace
  kin mcp start --help  how AI clients connect to Kin";

/// Clap groups arguments, but only has one heading for all subcommands. The
/// root template uses the metadata-derived index without changing its parser.
const ROOT_HELP_TEMPLATE: &str = "\
{before-help}{usage-heading} {usage}

Options:
{options}{after-help}
";

const COMMAND_GROUPS: &[(&str, &[&str])] = &[
    (
        "Get started",
        &[
            "setup",
            "init",
            "clone",
            "migrate",
            "status",
            "capabilities",
            "languages",
            "help",
        ],
    ),
    (
        "Understand code",
        &[
            "ask",
            "overview",
            "locate",
            "search",
            "trace",
            "path",
            "context",
            "refs",
            "source",
            "impact",
            "trace-data-flow",
            "deps",
            "xref",
            "history",
            "blame",
            "dead-code",
            "scope",
        ],
    ),
    (
        "Edit and record changes",
        &[
            "admit",
            "reconcile",
            "rename",
            "commit",
            "diff",
            "log",
            "branch",
            "checkout",
            "merge",
            "conflicts",
            "resolve",
            "stash",
            "rollback",
            "purge-ignored",
        ],
    ),
    (
        "Use editors and AI",
        &[
            "ui",
            "open",
            "with",
            "shell",
            "exec",
            "agent",
            "assistant",
            "mcp",
            "describe",
            "call",
            "vfs",
        ],
    ),
    (
        "Coordinate work",
        &[
            "work", "feature", "todo", "spec", "note", "intent", "traffic", "agents", "notify",
        ],
    ),
    (
        "Review and protect",
        &[
            "review",
            "verify",
            "approvals",
            "security",
            "audit",
            "backup",
            "secret",
            "allow",
        ],
    ),
    (
        "Share and interoperate",
        &["auth", "remote", "push", "pull", "git", "eject"],
    ),
    (
        "Deliver releases",
        &[
            "publish",
            "semver",
            "release",
            "tag",
            "pipeline",
            "hosted-release",
        ],
    ),
    (
        "Diagnose and tune",
        &[
            "doctor",
            "graph",
            "embed",
            "cache",
            "store",
            "locate-debug",
            "bench",
            "support",
            "telemetry",
            "resources",
            "daemon",
            "runtime",
            "registry",
            "update",
            "upgrade",
            "completions",
        ],
    ),
];

fn grouped_commands_help(root: &clap::Command, groups: &[(&str, &[&str])]) -> String {
    fn append_group(output: &mut String, heading: &str, members: Vec<clap::Command>) {
        if members.is_empty() {
            return;
        }
        let mut group = clap::Command::new("kin")
            .disable_help_flag(true)
            .disable_help_subcommand(true)
            .help_template("{subcommands}")
            .subcommands(
                members
                    .into_iter()
                    .enumerate()
                    .map(|(order, command)| command.display_order(order)),
            );
        output.push_str(heading);
        output.push_str(":\n");
        output.push_str(group.render_help().to_string().trim_end());
        output.push_str("\n\n");
    }

    let visible: Vec<_> = root
        .get_subcommands()
        .filter(|command| !command.is_hide_set())
        .collect();
    let mut seen = std::collections::HashSet::new();
    let mut output = String::new();
    for (heading, names) in groups {
        let members = names
            .iter()
            .filter_map(|name| {
                visible
                    .iter()
                    .find(|command| command.get_name() == *name)
                    .filter(|command| seen.insert(command.get_name()))
                    .map(|command| (*command).clone())
            })
            .collect();
        append_group(&mut output, heading, members);
    }
    // A newly added command remains visible even before it has a category.
    // The exhaustive mapping test still requires its category before landing.
    let remaining = visible
        .into_iter()
        .filter(|command| !seen.contains(command.get_name()))
        .cloned()
        .collect();
    append_group(&mut output, "Other commands", remaining);
    output
}

fn command_index() -> &'static str {
    static INDEX: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    INDEX.get_or_init(|| {
        // Build from the enum, not Cli, whose after-help calls this function.
        let mut command = Command::augment_subcommands(clap::Command::new("kin"));
        command.build();
        grouped_commands_help(&command, COMMAND_GROUPS)
    })
}

/// What follows the command list: where to read the rest.
const AFTER_HELP: &str = "\
`kin <command> --help` explains each command and its subcommands.
`kin capabilities` prints repository replacement readiness, `--json` for machines.";

/// The `[OPEN GATE]` legend, which is only true while something carries the
/// marker.
const OPEN_GATE_LEGEND: &str = "\n\nCommands marked [OPEN GATE] are fail-closed on \
repository-v6 and say why when run.";

/// Root after-help, with the open-gate legend appended only when the inventory
/// actually reports a gate.
///
/// The legend teaches a marker, so printing it when nothing is marked tells a
/// caller scanning the list for what works that everything is ready. It had
/// been doing exactly that: the gates all closed, the markers came off, and the
/// sentence describing them stayed, pinned by a test asserting the bare string.
///
/// An unreadable inventory drops the legend rather than failing, because help
/// has to render; a broken inventory is reported by `kin capabilities`, which
/// is the command that exists to read it.
fn after_help() -> String {
    let gated = commands::capabilities::inventory()
        .map(|inventory| {
            inventory.commands.iter().any(|capability| {
                capability.status == commands::capabilities::CapabilityStatus::OpenGate
            })
        })
        .unwrap_or(false);
    if gated {
        format!("{}{AFTER_HELP}{OPEN_GATE_LEGEND}", command_index())
    } else {
        format!("{}{AFTER_HELP}", command_index())
    }
}

/// Help that does not fit the terminal it is going to, wrapped at words, or
/// `None` when it fits or goes to a file or a pipe.
///
/// clap prints help as written, so a narrow terminal broke words in two
/// ("ki / n to your shell"). The wrap keeps every command line's description
/// column, hanging its continuation under it, and a flag's description under
/// its own indent. Help that fits keeps clap's own styled rendering, and help
/// a program reads is never rewrapped.
fn help_fitted_to_terminal(err: &clap::Error) -> Option<String> {
    let term = if err.use_stderr() {
        console::Term::stderr()
    } else {
        console::Term::stdout()
    };
    if !term.is_term() {
        return None;
    }
    let (_, columns) = term.size_checked()?;
    let width = usize::from(columns);
    let plain = err.render().to_string();
    let overflows = plain.lines().any(|line| line.chars().count() > width);
    (width > 0 && overflows).then(|| kin_cli::screen::hang_wrap(&plain, width))
}

#[derive(Parser)]
#[command(
    name = "kin",
    version = kin_buildinfo::version(),
    before_help = ORIENTATION,
    after_help = after_help(),
    help_template = ROOT_HELP_TEMPLATE,
)]
struct Cli {
    /// Write a machine-readable execution profile to this JSON file
    #[arg(long, global = true, value_name = "FILE")]
    profile_out: Option<PathBuf>,

    /// Print the hottest profiled stages to stderr after the command finishes
    #[arg(long, global = true, default_value_t = false)]
    profile_summary: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Inspect the one admitted CLI and daemon runtime pair
    Runtime {
        #[command(subcommand)]
        action: RuntimeAction,
    },
    /// Show which Git commands Kin replaces, and how ready each one is
    Capabilities {
        /// Output the versioned capability inventory as JSON
        #[arg(long, default_value_t = false)]
        json: bool,
        /// Add the per-command notes under each matrix row
        #[arg(long, default_value_t = false, conflicts_with = "json")]
        verbose: bool,
    },
    /// Start Kin in a repository, new or existing
    Init {
        /// Directory to initialize (defaults to current directory)
        path: Option<String>,
        /// Output machine-readable JSON status instead of human text
        #[arg(long, default_value_t = false)]
        json: bool,
        /// Switch language-server enrichment off for this repository
        ///
        /// Conversion queries an installed language server for the reference,
        /// override and type-use edges across units that parsing each unit on
        /// its own cannot derive. It is the slowest phase and it needs a
        /// server, so a conversion that does not want it can say so. This flag
        /// records that enrichment is off for the repository: no daemon that
        /// serves it runs a sweep, enriches a change or starts a language
        /// server, whoever starts it. Import and call edges still come from
        /// source. On a repository that already exists it records the same
        /// setting and stops the daemon serving it, which started with
        /// enrichment on. `kin daemon sweep --enable` turns enrichment on
        /// later. To skip the sweep now and let the next daemon run it, use
        /// `--defer-enrich`.
        #[arg(long = "no-enrich", default_value_t = false)]
        no_enrich: bool,
        /// Skip the language-server sweep now; the next daemon start runs it
        ///
        /// Enrichment stays on for the repository. This command returns
        /// without the language-server sweep, and the next daemon that starts
        /// on the repository, which the first query after this one starts,
        /// queues it and links references across entities in the background.
        /// Queries during that sweep share the daemon with it. Use
        /// `--no-enrich` to switch enrichment off instead.
        #[arg(
            long = "defer-enrich",
            default_value_t = false,
            conflicts_with = "no_enrich"
        )]
        defer_enrich: bool,
        /// Adopt an existing repository's identity instead of minting a new one
        ///
        /// Use this when the store being created has to publish into a
        /// repository that already exists, such as a hosted one that serves
        /// transfer under an identity it already holds. Every exact-transfer
        /// surface is identity-exact and none of them can be retargeted after
        /// the fact, so a store that minted its own identity can never push
        /// into that repository. Without this flag `kin init` mints a fresh
        /// identity, which is right for a repository nothing else holds.
        #[arg(long = "adopt-repository-id", value_name = "ID")]
        adopt_repository_id: Option<String>,
        /// Keep the store in this directory instead of the central stores directory
        ///
        /// A new store normally lives in the central stores directory under the
        /// Kin home (`~/.kin/stores`). With this flag it is created at DIR,
        /// which must be outside the working tree and absent or empty. Either
        /// way the working tree carries nothing: no pointer file and no ignore
        /// rule, so the store never shows in its diff and a read-only checkout
        /// can be served. Every later command, daemon, `kin mcp start` and
        /// editor finds the store from the working tree through the store index
        /// beside the registry. `KIN_STORE` sets the same thing from the
        /// environment, and `KIN_STORE_LAYOUT=in-tree` keeps a new store in the
        /// working tree as `.kin` instead.
        #[arg(long, value_name = "DIR")]
        store: Option<PathBuf>,
        /// Print the full record: every admission stage, the ids and the
        /// enrichment detail
        ///
        /// On a terminal `kin init` prints one line per phase and a short
        /// result. A pipe, CI and this flag keep the full record.
        #[arg(long, default_value_t = false, conflicts_with = "json")]
        verbose: bool,
        /// How much Git history to admit: background, none or full
        ///
        /// `background` (the default) admits the checked-out commit first, so
        /// the repository answers within seconds, and derives the full history
        /// afterwards at low priority. `none` admits the checked-out commit
        /// only and never derives history, for CI jobs, containers and agent
        /// sandboxes; it also admits a shallow clone. `full` admits the whole
        /// reachable history before the store is published. Over an existing
        /// store, `--history full` derives and attaches its history now, and
        /// `background` or `none` changes what it does next.
        #[arg(long, value_name = "MODE", value_parser = parse_history_mode)]
        history: Option<kin_core::history_state::HistoryMode>,
    },
    /// Show your changes, your branch and Kin's progress
    Status {
        /// Output machine-readable JSON for editor integrations
        #[arg(long, default_value_t = false)]
        json: bool,
        /// Seconds to keep re-reading while embedding coverage is only
        /// momentarily unobservable, such as an embedding pass or a graph
        /// mutation batch spanning the sample. Never waits on a coverage that
        /// was observed, nor on an absence a re-read cannot clear. 0 reads once
        #[arg(long, value_name = "SECONDS", default_value_t = 0)]
        wait_quiesce: u64,
        /// Print the full record: ids, generations, the working copy's basis,
        /// the store and the daemon
        ///
        /// On a terminal `kin status` prints a short page. A pipe, CI and this
        /// flag keep the full record.
        #[arg(long, default_value_t = false, conflicts_with = "json")]
        verbose: bool,
    },
    /// Create an exact semantic and artifact commit
    ///
    /// The long help is built in `commit_progress` around the same sentence a
    /// commit prints, so this surface and that one cannot drift (FIR-2627).
    #[command(long_about = commands::commit_progress::COMMIT_LONG_ABOUT)]
    Commit {
        /// Commit message; amend preserves the existing message when omitted
        #[arg(short, long, required_unless_present = "amend")]
        message: Option<String>,
        /// Replace the current change with the full working state, preserving its parents
        #[arg(long)]
        amend: bool,
        /// Suppress progress output (only print final summary)
        #[arg(short, long)]
        quiet: bool,
        /// Commit only this entity's uncommitted change, leaving every other
        /// change uncommitted (repeat for more entities)
        #[arg(long = "entity", value_name = "ENTITY_ID", conflicts_with = "amend")]
        entities: Vec<String>,
    },
    /// Show the immutable repository-v6 change log
    Log {
        /// Maximum number of entries
        #[arg(short = 'n', long, default_value = "10")]
        count: usize,
        /// Output the exact authority-backed report as JSON
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Repository-v6 branch operations (see subcommand readiness)
    Branch {
        #[command(subcommand)]
        action: BranchAction,
    },
    /// Show exact repository-v6 artifact and semantic changes
    Diff {
        /// Base ref, change ID, Git object ID, HEAD, or ref-hex:<hex>
        base: Option<String>,
        /// Head ref, change ID, Git object ID, WORKSPACE, or ref-hex:<hex>
        head: Option<String>,
        /// Output the exact authority-backed report as JSON
        #[arg(long, default_value_t = false)]
        json: bool,
        /// Carry each changed artifact's full old and new body, not just hunks
        #[arg(long, default_value_t = false)]
        full_bodies: bool,
    },
    /// Verify graph-derived projection, install exact Git, and detach Kin.
    ///
    /// Every graph-owned artifact and blob must match one durable authority
    /// generation before metadata can be detached.
    Eject {
        /// Skip the typed "eject" confirmation.
        #[arg(long)]
        yes: bool,
    },
    /// Show downstream impact of an entity
    Impact {
        /// Entity name or ID
        entity: String,
        /// Maximum depth
        #[arg(short, long, default_value = "3")]
        depth: u32,
        /// Exact repo-relative file qualifier for stable identity resolution
        #[arg(long)]
        file: Option<String>,
        /// Exact entity-kind qualifier (for example: function or method)
        #[arg(long)]
        kind: Option<String>,
        /// Whitespace-normalized declaration signature for overload resolution
        #[arg(long)]
        signature: Option<String>,
        /// Emit the ranked graph-evidence report as JSON; ambiguous identities fail closed
        #[arg(long, default_value_t = false)]
        json: bool,
        /// Also list interface-dispatch candidates: Go call sites that reach an
        /// interface method this method's receiver type satisfies. They are
        /// possible dependents, not confirmed ones, and are never added to the
        /// impacted count.
        #[arg(long, default_value_t = false)]
        dispatch: bool,
    },
    /// Build a context pack for one entity, several, or a question
    Context {
        /// Entity names or IDs. A name that has twins in the store can pin the
        /// one it means: `Name@file`, `Name@file:line`, `Name#Kind`
        #[arg(value_name = "ENTITY")]
        entities: Vec<String>,
        /// Build the pack from the entities this question ranks for, through
        /// kin's own locate ranking
        #[arg(long, value_name = "TEXT")]
        question: Option<String>,
        /// Token budget (8k, 16k, 32k, or custom number)
        #[arg(short, long, default_value = "8k")]
        budget: String,
        /// Assistant hint for tuning context pack strategy
        #[arg(long)]
        assistant: Option<String>,
        /// Most focal entities a question may resolve to
        #[arg(long, value_name = "N")]
        max_focals: Option<usize>,
        /// Emit the resolved targets and the whole context pack as JSON
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Print the exact implementation body for an entity
    ///
    /// The same command as `kin graph source`, at the top level so that `kin
    /// source`, the word the routed MCP tool teaches for reading one entity's
    /// code, runs in a shell too.
    Source(EntitySourceArgs),
    /// Hidden ContextBench locate wrapper that keeps benchmark query shaping inside Kin
    #[command(hide = true)]
    ContextbenchLocate {
        /// Path to the raw ContextBench task payload JSON
        #[arg(long)]
        task_file: PathBuf,
        /// Output machine-readable JSON
        #[arg(long, default_value_t = false)]
        json: bool,
        /// Persist the full locate debug payload and per-file signal/score breakdowns
        #[arg(long, default_value_t = false)]
        debug: bool,
    },
    /// Trace a focal entity in one shot: resolve it, show the body, and summarize nearby context
    Trace {
        /// Entity name or ID
        entity: String,
        /// Exact repo-relative file qualifier for stable identity resolution
        #[arg(long)]
        file: Option<String>,
        /// Exact entity-kind qualifier (for example: function or method)
        #[arg(long)]
        kind: Option<String>,
        /// Output machine-readable JSON for editor integrations
        #[arg(long, default_value_t = false)]
        json: bool,
        /// Render a smaller, cheaper trace tuned for assistant workflows
        #[arg(long, default_value_t = false)]
        compact: bool,
        /// Compatibility no-op: trace already shows the focal body by default
        #[arg(long, hide = true, default_value_t = false)]
        show_body: bool,
        /// Compatibility alias: interpreted as the nearby entry cap when provided
        #[arg(long, hide = true)]
        limit: Option<usize>,
        /// Token budget (8k, 16k, 32k, or custom number)
        #[arg(short, long, default_value = "8k")]
        budget: String,
        /// Assistant hint for tuning context pack strategy
        #[arg(long)]
        assistant: Option<String>,
        /// Max lines to print for any single source snippet
        #[arg(long, default_value_t = 40)]
        max_lines: usize,
        /// Max nearby entries to print
        #[arg(long, default_value_t = 4)]
        nearby: usize,
        /// Max transitive entries to print
        #[arg(long, default_value_t = 2)]
        transitive: usize,
    },
    /// Find the shortest routes from one entity to another over the graph's
    /// call, instantiation, reference, import and include edges.
    ///
    /// Each end is an entity name, an entity id, or `name@file` to pin one of
    /// two same-named entities. A class stands for its members, so a route
    /// between two classes runs through the methods that carry it. Exits 3 when
    /// the graph holds no route inside the depth bound, with the gap on stderr.
    Path {
        /// Source entity: name, id, or name@file
        from: String,
        /// Target entity: name, id, or name@file
        to: String,
        /// Pin the source to the entity of that name in this file (path or path suffix)
        #[arg(long = "from-file", value_name = "FILE")]
        from_file: Option<String>,
        /// Pin the target the same way
        #[arg(long = "to-file", value_name = "FILE")]
        to_file: Option<String>,
        /// Hops walked between the two ends (default 6, ceiling 12); containment hops are not counted
        #[arg(long = "max-depth", value_name = "N")]
        max_depth: Option<usize>,
        /// Routes printed, shortest first (default 3, ceiling 25)
        #[arg(long, value_name = "K")]
        limit: Option<usize>,
        /// `forward` (from reaches to), `reverse` (to reaches from), or `either` (default; forward first, reports which held)
        #[arg(long, value_name = "DIR")]
        direction: Option<String>,
        /// Walk through type-annotation edges too
        #[arg(long = "include-type-edges", default_value_t = false)]
        include_type_edges: bool,
        /// Output machine-readable JSON, `_kin` envelope included
        #[arg(long, default_value_t = false)]
        json: bool,
        /// One line per hop and nothing else, sized for a prompt
        #[arg(long, default_value_t = false)]
        compact: bool,
    },
    /// Print where entities are projected in the working copy, for editors
    ///
    /// Each ENTITY_ID is answered with the path its declaration is projected
    /// to, relative to the repository root, and the declaration's 1-based
    /// start and end, or with the reason the graph cannot place it. Up to 512
    /// ids per call. Editors use this to put a cursor on an entity; to explore
    /// a repository use `kin locate`, `kin refs` and `kin source`.
    #[command(hide = true)]
    Places {
        /// Entity ids to place, bare or spelled `entity:<uuid>`
        #[arg(required = true, value_name = "ENTITY_ID")]
        entity_ids: Vec<String>,
        /// Output machine-readable JSON
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Search entities in the graph
    Search {
        /// Search pattern (use '|' for OR, e.g. "save|load|persist")
        pattern: String,
        /// Output machine-readable JSON for editor integrations
        #[arg(long, default_value_t = false)]
        json: bool,
        /// Filter by entity kind
        #[arg(short, long)]
        kind: Option<String>,
        /// Filter by language
        #[arg(short, long)]
        language: Option<String>,
        /// Show entity source body inline
        #[arg(long)]
        show_body: bool,
        /// Max lines per entity body (with --show-body)
        #[arg(long)]
        limit: Option<usize>,
        /// Use semantic (vector similarity) search instead of name matching
        #[arg(long)]
        semantic: bool,
    },
    /// Set, show, or clear a temporal scope for the current session
    Scope {
        /// Ref to scope to (git:sha, branch name, HEAD~N, etc.)
        ref_string: Option<String>,
        /// Clear the current scope
        #[arg(long)]
        clear: bool,
        /// Show the current scope
        #[arg(long)]
        show: bool,
        /// Session ID (or set KIN_SESSION_ID env var)
        #[arg(long)]
        session: Option<String>,
    },
    /// Locate files relevant to an issue or problem description
    ///
    /// This command ranks problem text. For the MCP locate contract, including
    /// exact graph file coordinates and mode selection, use `kin call locate`
    /// with the JSON arguments shown by `kin describe locate`.
    Locate {
        /// Problem text (inline)
        text: Option<String>,
        /// Additional query variant(s) for multi-query fan-out (repeatable).
        /// The primary text plus each variant are retrieved independently and
        /// their rankings RRF-fused into one deduped result. Diverse variants
        /// (identifiers, behavior, subsystem) recover more relevant files than
        /// any single phrasing. Omit for a normal single-query locate.
        #[arg(long = "query", value_name = "QUERY")]
        query: Vec<String>,
        /// Read problem text from file
        #[arg(long)]
        file: Option<String>,
        /// Read from stdin
        #[arg(long)]
        stdin: bool,
        /// Output JSON
        #[arg(long)]
        json: bool,
        /// Include graph-native projection reasons in the output
        #[arg(long, default_value_t = false)]
        explain: bool,
        /// Diagnostic mode: enables --json --explain, adds per-stage scoring
        /// detail, entity seed dump, and timing breakdown. Compares against
        /// --gold files if provided. Use this for debugging locate quality.
        #[arg(long, default_value_t = false)]
        diagnose: bool,
        /// Gold file paths for diagnostic comparison (comma-separated).
        /// With --diagnose, shows where each gold file appears/disappears
        /// in the scoring pipeline and why.
        #[arg(long, value_delimiter = ',')]
        gold: Vec<String>,
        /// Max files to return (omit for adaptive sizing)
        #[arg(long)]
        max_files: Option<usize>,
        /// Resolve locate against a specific ref.
        /// Accepts `HEAD`, `HEAD~N`, branch names, `branch:<name>`,
        /// imported Git commits as `git:<sha>` or bare 40-hex SHAs,
        /// and semantic changes as `kin:<id>`, `change:<id>`, or bare change IDs.
        #[arg(long = "ref", value_name = "REF")]
        reference: Option<String>,
        /// Attach a bounded inline source snippet (signature + first body lines)
        /// to each top definition symbol. Default ON for `--json` (the agent
        /// surface), so an agent can act on the first locate without a follow-up
        /// read; force it on for any output with this flag.
        #[arg(long)]
        snippets: bool,
        /// Suppress inline snippets even on the `--json` surface.
        #[arg(long = "no-snippets", conflicts_with = "snippets")]
        no_snippets: bool,
        /// Fetch the NEXT page of ranked entities from the previous query,
        /// reading the cursor persisted in `.kin/locate-cursor`. No retrieval
        /// re-run; pages the daemon's cached ranking. Query text is not required.
        #[arg(long, conflicts_with = "cursor")]
        next: bool,
        /// Fetch a specific entity page using an explicit cursor token (from a
        /// prior result's `next_cursor`). Lower-level alternative to `--next`.
        #[arg(long)]
        cursor: Option<String>,
        /// Entities per page for the graph-native `entities` surface
        /// (`KIN_LOCATE_ENTITY_CAP` otherwise).
        #[arg(long)]
        page_size: Option<usize>,
        /// Rank test-role entities alongside source. Off by default: locate
        /// demotes tests unless the query text itself reads as being about
        /// them, which is right for "where does this feature live" and wrong
        /// when you already know you are asking for a test. The response says
        /// how many test paths a default run withheld.
        #[arg(long = "include-tests", default_value_t = false)]
        include_tests: bool,
        /// Which JSON shape `--json` emits: `full` (every field, the default) or
        /// `compact` (the agent surface: id, or the artifact path on a hit for a
        /// tracked file with no parsed entities, name, kind, file, line,
        /// signature, score and match kind per hit, `collapsed_rows` on a Go
        /// package row that folded at least that many of the package's other
        /// files, ranked file paths, and a `_kin` envelope naming the embedding
        /// coverage behind the ranking).
        ///
        /// `full` stays the default because it is what ContextBench, the
        /// acceptance scripts and `--diagnose` read. How much `compact` saves
        /// depends on how large the store is, so both scales are given here. On
        /// the 730-entity hiredis store twelve results are 38,819 bytes full and
        /// 3,472 compact, about a tenth. On the microsoft/vscode and
        /// facebook/react stores, measured 2026-09-02, the same commands come
        /// back 3.0x to 4.9x smaller rather than ten times, because the
        /// back-compat `files[].symbols` roll-up is 69 percent of a small
        /// store's payload and a smaller share of a large one's.
        #[arg(long = "surface", value_name = "SHAPE", default_value = "full")]
        surface: String,
    },
    /// Debug locate results: show per-signal breakdown, rank gold files,
    /// and diagnose why targets were missed.
    #[command(name = "locate-debug")]
    LocateDebug {
        /// Problem text (inline query)
        #[arg(default_value = "")]
        text: String,
        /// Gold file to track (report rank and signal breakdown)
        #[arg(long)]
        target: Option<String>,
        /// Load query and gold files from a benchmark task JSON
        #[arg(long)]
        task_file: Option<String>,
        /// Max files to search (wider than default to find low-ranked targets)
        #[arg(long, default_value_t = 50)]
        max_files: usize,
        /// Output machine-readable JSON
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Build embeddings for the current repository's entity graph.
    ///
    /// Generates vector embeddings for all entities using a local code retriever
    /// (nomic-embed-text-v1.5, 768 dimensions; override via KIN_EMBED_MODEL_ID).
    /// Embeddings enable semantic similarity
    /// search in `kin locate` and `kin search --semantic`.
    ///
    /// Repository admission and enrichment are separate: `kin init` commits
    /// repository authority; `kin embed` adds vectors for graph-owned entities
    /// after semantic enrichment exists.
    ///
    /// The model is not bundled with any install. The first embed on a machine
    /// downloads about 523 MB of nomic-embed-text-v1.5 from huggingface.co into
    /// the Hugging Face hub cache under the home directory
    /// (`~/.cache/huggingface/hub`), and nothing embeds until that download
    /// lands. A host with no egress to huggingface.co needs that cache
    /// pre-seeded from a machine that has it, or KIN_EMBED_MODEL_ID pointed at
    /// a local model directory. `kin doctor` reports whether the model is
    /// already here.
    ///
    /// If a repo was indexed with an older model at a different dimension, pass
    /// `--rebuild` to drop the stale index and re-embed every entity at the
    /// current model's dimension.
    Embed {
        /// Embedding batch size (entities per inference pass) for THIS pass only.
        /// Defaults to 64, or the throughput resource plan's per-chunk budget when
        /// KIN_RESOURCE_PROFILE=throughput is set. It does not change the daemon's
        /// own background queue and does not survive a daemon restart; record a
        /// durable one with `kin resources set --embed-batch-size N`.
        #[arg(long)]
        batch_size: Option<usize>,
        /// Stop after this many seconds, persist completed vectors, and leave the rest pending.
        #[arg(long, value_name = "SECONDS")]
        max_seconds: Option<u64>,
        /// Drop the existing vector index and re-embed every entity at the current
        /// model's dimension. Use this to migrate a repo indexed with an older
        /// model (e.g. a 384-dim index that fails against the 768-dim default).
        #[arg(long, visible_alias = "force")]
        rebuild: bool,
        /// Output JSON status instead of progress text.
        #[arg(long)]
        json: bool,
    },
    /// Bounded graph-native rename; unsupported cases fail closed
    Rename {
        /// Entity name or symbol under the cursor
        symbol: String,
        /// Replacement name
        new_name: String,
        /// File hint to disambiguate the target entity
        #[arg(long)]
        file: Option<String>,
        /// 1-based line hint in --file; required when --column is provided
        #[arg(long)]
        line: Option<u32>,
        /// 0-based UTF-8 byte column (tree-sitter coordinate), requires --line
        #[arg(long)]
        column: Option<u32>,
        /// Output machine-readable JSON for editor integrations
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Show upstream callers/importers/references for an entity
    Refs {
        /// Entity name or ID. Required unless --bulk-json + --entities is provided.
        /// A name with twins can carry its pin: `Name@file`, `Name@file:line`,
        /// `Name#kind`
        #[arg(required_unless_present = "bulk_json")]
        entity: Option<String>,
        /// Filter relation kinds: all, calls, imports, or references (or Any for
        /// bulk mode). Suffix with +dispatch (or pass dispatch on its own) to
        /// list interface-dispatch candidates beside the answer: Go call sites
        /// that reach an interface method this method's receiver type
        /// satisfies. On a Go interface method it lists the other direction
        /// instead, the concrete methods whose receiver types satisfy that
        /// contract. Either way they are possible, not confirmed, and are never
        /// added to the count.
        #[arg(long, default_value = "all")]
        kind: String,
        /// Exact repo-relative file of the entity, when its name has twins
        #[arg(long, conflicts_with = "bulk_json")]
        file: Option<String>,
        /// Exact entity kind (for example: function or method), when its name
        /// has twins. `--kind` filters relation kinds here, so the entity's own
        /// kind takes this flag
        #[arg(long, conflicts_with = "bulk_json")]
        entity_kind: Option<String>,
        /// Bulk mode: classify many entities by reachability in one daemon call.
        /// Outputs JSON to stdout. Requires --entities.
        #[arg(long, default_value_t = false, requires = "entities")]
        bulk_json: bool,
        /// Comma-separated entity UUIDs for --bulk-json. Required when --bulk-json is set.
        #[arg(long)]
        entities: Option<String>,
        /// If true (default) emit compact bulk-mode rows ({entity_id, has_references, reference_count, receiver_name_candidate_count, unconfirmed_candidate_count}). A row holding an unconfirmed candidate caller reads `reference_count` null beside `known_reference_count`, and `has_references` null unless a caller is confirmed.
        /// Set --no-compact for verbose rows with name/kind/file_path/matched_kinds.
        #[arg(long, default_value_t = true)]
        compact: bool,
        /// Force verbose bulk-mode rows (overrides --compact). Required for clap to accept `--no-compact`.
        #[arg(long = "no-compact", default_value_t = false, action = clap::ArgAction::SetTrue)]
        no_compact: bool,
        /// List every caller in full, each with its id and projection, instead of
        /// the first 20 sized to the terminal. Output that is not a terminal is
        /// always complete
        #[arg(long, default_value_t = false, conflicts_with = "bulk_json")]
        all: bool,
        /// Print the complete answer as JSON: its lines, the absence verdict and
        /// the call-site block
        #[arg(long, default_value_t = false, conflicts_with = "bulk_json")]
        json: bool,
        /// Bound the JSON answer in bytes and page every semantic record
        #[arg(long, requires = "json", conflicts_with_all = ["bulk_json", "all"])]
        max_chars: Option<usize>,
        /// Continue a frozen JSON reference answer with its next_cursor
        #[arg(long, requires = "json", conflicts_with_all = ["bulk_json", "all"])]
        cursor: Option<String>,
        /// Return only overriding methods and inheritance coverage. Overrides
        /// requires an exact entity UUID and does not accept --kind filters.
        #[arg(long, requires = "json", value_parser = ["references", "overrides"], conflicts_with_all = ["bulk_json", "all"])]
        view: Option<String>,
    },
    /// Open the local Kin workspace for repositories, entities and review
    Ui {
        /// Repository working directory (defaults to the current repository,
        /// or to the list of this machine's repositories outside one)
        path: Option<std::path::PathBuf>,
        /// Serve on this loopback port
        #[arg(long)]
        port: Option<u16>,
        /// Print the address without opening a browser
        #[arg(long)]
        no_open: bool,
    },
    /// Run semantic review on changes, or manage review state
    Review {
        /// Change ID to review (defaults to latest)
        change: Option<String>,
        /// Output machine-readable JSON for editor integrations
        #[arg(long, default_value_t = false)]
        json: bool,
        /// Comma-separated entity IDs to review
        #[arg(long)]
        entities: Option<String>,
        /// Comma-separated file paths to review
        #[arg(long)]
        files: Option<String>,
        /// Comma-separated change IDs to combine into one review
        #[arg(long)]
        changes: Option<String>,
        /// Show the full review, including every relation change by name
        #[arg(long, default_value_t = false, conflicts_with = "json")]
        relations: bool,
        /// Show all findings, entity changes and file detail (JSON is always complete)
        #[arg(long, default_value_t = false)]
        details: bool,
        /// Review mutation subcommand
        #[command(subcommand)]
        action: Option<ReviewAction>,
    },
    /// Derive a partial store's Git history into a stage and attach it
    ///
    /// The worker `kin init --history background` and the daemon start. A
    /// person runs `kin init --history full` instead.
    #[command(hide = true)]
    DeriveHistory {
        /// Repository whose store to derive (defaults to current directory)
        path: Option<String>,
        /// Run at background priority and attach only when no daemon serves
        /// the store
        #[arg(long, default_value_t = false)]
        background: bool,
    },
    /// Publish a reserved hosted repository's first graph authority.
    ///
    /// Hidden because it is an operator boundary rather than a Kin verb: it is
    /// driven by a trusted orchestrator that has already reserved an identity
    /// and materialized a source, and it is meaningless without both.
    #[command(hide = true)]
    HostedPublication {
        #[command(subcommand)]
        action: HostedPublicationAction,
    },
    /// Show entity history
    History {
        /// Entity name or ID. A name with twins can carry its pin:
        /// `Name@file`, `Name@file:line`, `Name#kind`
        entity: String,
        /// Exact repo-relative file of the entity, when its name has twins
        #[arg(long)]
        file: Option<String>,
        /// Exact entity kind (for example: function), when its name has twins
        #[arg(long)]
        kind: Option<String>,
        /// Resolve history against a specific ref.
        /// Accepts `HEAD`, `HEAD~N`, branch names, `branch:<name>`,
        /// imported Git commits as `git:<sha>` or bare 40-hex SHAs,
        /// and semantic changes as `kin:<id>`, `change:<id>`, or bare change IDs.
        #[arg(long = "ref", value_name = "REF")]
        reference: Option<String>,
        /// List every file-level revision, including ones that did not change
        /// this entity
        #[arg(long, default_value_t = false)]
        all_revisions: bool,
    },
    /// Find dead code (whole-repo scan, or seeded by semantic query)
    DeadCode {
        /// Seeded mode: run semantic_search(query) → classify each top-N
        /// candidate by incoming references → return dead-first ranked JSON.
        /// Closes the find-dead-code accuracy gap on large repos where
        /// the agent burns the tool-call cap looping search → find_references.
        #[arg(long = "seed", value_name = "QUERY")]
        seed: Option<String>,
        /// Max candidates to classify in seeded mode (default 20, max 200).
        /// Ignored when --seed is not set.
        #[arg(long = "limit", value_name = "N")]
        limit: Option<usize>,
        /// Optional case-insensitive substring filter on the candidate entity name.
        /// Lets callers pre-narrow to a known prefix or suffix (e.g., a planted-secret
        /// tag like "_eaca1f07") without burning extra tool-call rounds.
        #[arg(long = "name-pattern", value_name = "SUBSTRING")]
        name_pattern: Option<String>,
    },
    /// Trace the call/data-flow chain rooted at a focal entity.
    ///
    /// Returns the focal body plus a structured chain of callees, callers, or
    /// both (with bodies inlined) in a single substrate call. Closes the
    /// trace-computation accuracy gap where the agent loops `get_entity_source`
    /// per step and burns the 24-round tool-call cap.
    TraceDataFlow {
        /// Focal entity to start tracing from. Accepts a UUID or an exact
        /// entity name (resolved via the same ranking path as `graph source`).
        #[arg(long = "focal", value_name = "ENTITY")]
        focal: String,
        /// Continue the same trace with the prior response's next_cursor.
        #[arg(long, value_name = "CURSOR")]
        cursor: Option<String>,
        /// Maximum traversal depth from the focal (default 3, capped at 8).
        #[arg(long = "depth", value_name = "N")]
        depth: Option<usize>,
        /// Traversal direction: `calls`, `callers`, or `both` (default both).
        #[arg(long = "direction", value_name = "DIR")]
        direction: Option<String>,
        /// Max relations expanded per step (default 5, capped at 25).
        #[arg(long = "limit-per-step", value_name = "M")]
        limit_per_step: Option<usize>,
        /// A symbol you are trying to reach. Neighbors from which it is still
        /// reachable inside the requested depth survive the per-step cap ahead
        /// of ones that are not, so the question decides what a narrow walk
        /// keeps instead of proximity deciding it.
        #[arg(long = "target", value_name = "ENTITY")]
        target: Option<String>,
        /// Return the chain's shape, meaning names, kinds, roles, spans and
        /// edges, without inlining any source body.
        #[arg(long = "no-bodies", default_value_t = false)]
        no_bodies: bool,
        /// Hard UTF-8 JSON byte ceiling, including trust metadata. A larger
        /// trace returns a page and next_cursor; oversized semantic fields
        /// carry lossless fragments. Repeat this query with --cursor to resume.
        #[arg(long = "max-response-chars", value_name = "C")]
        max_response_chars: Option<usize>,
        /// Walk through a type-annotation edge to a type this repository
        /// defines. Off by default, because a shared type name otherwise joins
        /// every entity that annotates with it to every other one. A target
        /// this repository does not define stays a leaf either way.
        #[arg(long = "include-type-edges", default_value_t = false)]
        include_type_edges: bool,
        /// Hold the walk to a module path prefix, as a row's `module` field
        /// spells it (`flask.sansio`). The walk follows no edge out of it, and
        /// the reply's `scope` block says what it left out.
        #[arg(long = "scope", value_name = "MODULE")]
        scope: Option<String>,
        /// What `--scope` restricts: `paths` (default) keeps the walk inside
        /// it; `rows` walks everything and lists only the rows inside it.
        #[arg(long = "scope-mode", value_name = "MODE")]
        scope_mode: Option<String>,
    },
    /// Show this repository's recorded cross-repo dependencies
    Deps {
        /// Report every registered repository instead of this one
        #[arg(long, default_value_t = false)]
        all: bool,
        /// Output machine-readable JSON
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Show federated cross-repo references (xrefs) for an entity
    Xref {
        /// Entity name or ID
        entity: String,
    },
    /// Manage specs
    Spec {
        #[command(subcommand)]
        action: SpecAction,
    },
    /// Merge semantic and exact-tree changes from another branch
    ///
    /// Exits 0 when the merge published or was already up to date, and 8 when it
    /// is parked with conflicts: no ref moved, the record is durable, and `kin
    /// conflicts` lists what is outstanding. A script can tell those apart from
    /// a failed command, which exits 1.
    Merge {
        /// Branch to merge from
        branch: String,
        /// Emit the machine-readable merge report
        #[arg(long)]
        json: bool,
    },
    /// Show the durable merge transaction held for this workspace
    Conflicts {
        /// Emit the machine-readable merge transaction record
        #[arg(long)]
        json: bool,
        /// Print each conflict subject's three sides as source, diffed against
        /// the common ancestor
        #[arg(long)]
        show: bool,
    },
    /// Resolve repository-v6 merge conflicts
    ///
    /// At least one resolution flag is required, which is a
    /// group rather than a per-argument condition. `kin conflicts` is the
    /// read-only view of the same transaction, so nothing here has to accept
    /// an empty invocation in order to be inspectable.
    #[command(group = clap::ArgGroup::new("resolution")
        .required(true)
        .multiple(true)
        .args([
            "ours", "theirs", "base", "remove", "keep_path", "file",
            "all_ours", "all_theirs", "do_continue", "abort",
        ]))]
    Resolve {
        /// Keep your (target branch) version of a conflicting identity
        #[arg(long, value_name = "SELECTOR")]
        ours: Vec<String>,
        /// Keep the incoming (source branch) version of a conflicting identity
        #[arg(long, value_name = "SELECTOR")]
        theirs: Vec<String>,
        /// Keep the merge base version of a conflicting identity
        #[arg(long, value_name = "SELECTOR")]
        base: Vec<String>,
        /// Settle a conflicting identity by dropping it from the merge
        #[arg(long, value_name = "SELECTOR")]
        remove: Vec<String>,
        /// Settle a contested path by naming the artifact that keeps it
        #[arg(long, value_name = "PATH=ARTIFACT")]
        keep_path: Vec<String>,
        /// Resolve a conflicted repository path using the exact bytes in FILE
        #[arg(long, num_args = 2, value_names = ["PATH", "FILE"], action = clap::ArgAction::Append)]
        file: Vec<String>,
        /// Resolve all remaining conflicts keeping your version
        #[arg(long)]
        all_ours: bool,
        /// Resolve all remaining conflicts keeping the incoming version
        #[arg(long)]
        all_theirs: bool,
        /// Complete the merge after all conflicts are resolved
        #[arg(long, visible_alias = "continue")]
        do_continue: bool,
        /// Abort the merge and discard conflict state
        #[arg(long)]
        abort: bool,
        /// Require the merge transaction to still be the one this identity names
        #[arg(long, value_name = "HASH")]
        expect: Option<String>,
        /// Emit the machine-readable merge transaction record
        #[arg(long)]
        json: bool,
    },
    /// Seal and restore exact graph-owned workspace state
    Stash {
        #[command(subcommand)]
        action: StashAction,
    },
    /// Show blame (version history) for an entity
    Blame {
        /// Entity name or ID. A name with twins can carry its pin:
        /// `Name@file`, `Name@file:line`, `Name#kind`
        entity: String,
        /// Exact repo-relative file of the entity, when its name has twins
        #[arg(long)]
        file: Option<String>,
        /// Exact entity kind (for example: function), when its name has twins
        #[arg(long)]
        kind: Option<String>,
        /// Resolve blame against a specific ref.
        /// Accepts `HEAD`, `HEAD~N`, branch names, `branch:<name>`,
        /// imported Git commits as `git:<sha>` or bare 40-hex SHAs,
        /// and semantic changes as `kin:<id>`, `change:<id>`, or bare change IDs.
        #[arg(long = "ref", value_name = "REF")]
        reference: Option<String>,
        /// List every file-level revision, including ones that did not change
        /// this entity
        #[arg(long, default_value_t = false)]
        all_revisions: bool,
    },
    /// MCP server commands
    Mcp {
        #[command(subcommand)]
        action: McpAction,
    },
    /// Ask a question about this repository in plain words
    ///
    /// Kin reads the question, runs the graph query that answers it (refs,
    /// trace, impact, locate, source or context) and prints one reply: what it
    /// ran and which words of the question it read that from, what the
    /// question asked for that the query did not apply, then each answer,
    /// verdict first. It is the reply an MCP client gets from the `ask` tool.
    /// The question's words decide whether test code counts and which package
    /// limits the answer; the options override them.
    Ask {
        /// The question, in plain words. Name the function, method or class
        #[arg(required_unless_present = "hook", num_args = 1..)]
        question: Vec<String>,
        /// Answer an agent client's prompt hook instead: read the hook's JSON
        /// on stdin and print the context the client adds before the model's
        /// first turn, or nothing when the prompt asks nothing Kin routes
        #[arg(long, value_parser = commands::ask::HOOK_CLIENTS, conflicts_with = "json")]
        hook: Option<String>,
        /// Count test code, whatever the question says
        #[arg(long, conflicts_with = "no_tests")]
        tests: bool,
        /// Leave test code out, whatever the question says
        #[arg(long)]
        no_tests: bool,
        /// Limit the answer to modules under this dotted prefix, as rows spell a module
        #[arg(long)]
        scope: Option<String>,
        /// The most bytes the reply may take
        #[arg(long)]
        budget: Option<u64>,
        /// Print one JSON object with the answer, its one verdict and every call Kin made
        #[arg(long)]
        json: bool,
    },
    /// Show a routed kin tool command's or any Kin tool's arguments
    ///
    /// Prints what the routed `kin` MCP tool's `describe` command answers: the
    /// command's or tool's arguments as a JSON schema and one call that works.
    /// With no command it lists every routed command and every other tool `kin
    /// call` runs, marking each tool that writes. It answers as the
    /// agent-routed profile does, needs no repository, and exits 1 when the
    /// name is neither a command nor a tool.
    Describe {
        /// A routed command, such as locate or mutate, or any Kin tool's name.
        /// Omit it to list them all
        command: Option<String>,
    },
    /// Run any Kin tool by its registered name, as the routed kin tool's call
    ///
    /// Sends the routed `kin` MCP tool's `call` command through the same server
    /// path `kin mcp start` answers it on, against this repository's daemon, and
    /// prints the tool's answer with its `_kin` envelope. The arguments are one
    /// JSON object of the tool's fields, which `kin describe <tool>` lists, and
    /// `-` reads that object from stdin. A tool that answers with an error
    /// exits 1, with the answer printed as usual.
    /// `kin call locate` shares MCP locate's name, exact file-coordinate and
    /// description modes; standalone `kin locate` ranks problem text.
    ///
    /// It answers as the agent-routed profile does, writes included. A
    /// read-only MCP profile limits what its one tool reaches, not what a shell
    /// on the same machine runs, and a shell already writes through `kin
    /// commit`. A name a shell runs another way is refused with the command
    /// that works: `kin init` for kin_init, `kin describe` and `kin call` for
    /// the tool dispatchers, and the CLI spelling of a routed command's name.
    Call {
        /// The tool's registered name, as `kin describe` lists it
        tool: String,
        /// The tool's arguments as one JSON object, or `-` to read it from
        /// stdin. Omitted, the tool is called with none
        arguments: Option<String>,
    },
    /// Run a task through Kin's own agent loop, or check that it can start.
    ///
    /// The agent answers repository questions from the Kin graph over MCP and has no
    /// shell, no grep and no file-reading tool, so it cannot fall back to raw file
    /// search. Its one write is kin_mutate on an entity, under a Kin session, so the
    /// change names the entity it changes and carries provenance naming the agent.
    Agent {
        #[command(subcommand)]
        action: AgentAction,
    },
    /// Authenticate with KinLab for native remotes
    Auth {
        #[command(subcommand)]
        action: AuthAction,
    },
    /// Manage native and compatibility remotes
    Remote {
        #[command(subcommand)]
        action: RemoteAction,
    },
    /// Package and upload crate(s) to the kin-daemon registry
    Publish {
        /// Package(s) to publish (can be repeated: -p foo -p bar)
        #[arg(short = 'p', long = "package")]
        packages: Vec<String>,
        /// Registry URL (default: http://localhost:4219, or KIN_REGISTRY_URL env var)
        #[arg(long, default_value = "http://localhost:4219")]
        registry: String,
        /// Don't actually publish, just package and show what would be uploaded
        #[arg(long)]
        dry_run: bool,
    },
    /// Publish exact repository-v6 history to a native Kin remote
    Push {
        /// Remote name (defaults to the configured default native-kin remote)
        #[arg(long)]
        remote: Option<String>,
        /// Peer transfer base URL, overriding any configured remote
        #[arg(long)]
        url: Option<String>,
        /// Ref to publish (defaults to the repository default ref)
        #[arg(long = "ref")]
        reference: Option<String>,
        /// Print the negotiated outcome as JSON
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Admit exact repository-v6 history from a native Kin remote and move the
    /// workspace onto it
    #[command(visible_alias = "fetch")]
    Pull {
        /// Remote name (defaults to the configured default native-kin remote)
        #[arg(long)]
        remote: Option<String>,
        /// Peer transfer base URL, overriding any configured remote
        #[arg(long)]
        url: Option<String>,
        /// Ref to admit (defaults to the repository default ref)
        #[arg(long = "ref")]
        reference: Option<String>,
        /// Print the negotiated outcome as JSON
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Clone a repository into Kin
    Clone {
        /// Native Kin locator or Git repository URL
        url: String,
        /// Target directory (defaults to repo name)
        path: Option<String>,
        /// Native repository identity when URL is a peer daemon HTTP endpoint
        #[arg(long)]
        repository: Option<String>,
        /// Print the full record: Git's own progress, every admission stage,
        /// the ids and the enrichment detail
        ///
        /// On a terminal `kin clone` prints one line per phase and a short
        /// result. A pipe, CI and this flag keep the full record.
        #[arg(long, default_value_t = false)]
        verbose: bool,
    },
    /// Restore an exact path or subtree from immutable repository-v6 history
    Checkout {
        /// UTF-8 repository path to restore
        path: Option<String>,
        /// Byte-exact repository path as canonical lowercase hexadecimal
        #[arg(long, conflicts_with = "path")]
        path_hex: Option<String>,
        /// Change ID (defaults to current branch head)
        #[arg(long)]
        change: Option<String>,
        /// Restore one entity's source, as an uncommitted edit, instead of a path
        #[arg(long, value_name = "ENTITY_ID", conflicts_with_all = ["path", "path_hex"])]
        entity: Option<String>,
        /// Refuse unless the entity is exactly as last read: source:<body_hash> from a review, or absent
        #[arg(long, value_name = "FINGERPRINT", requires = "entity")]
        expected: Option<String>,
    },
    /// Verify test coverage for entities
    Verify {
        #[command(subcommand)]
        action: VerifyAction,
    },
    /// Run a command in an exact graph-derived session workspace
    Exec {
        /// Command to run (put kin flags before it: `kin exec --keep -- npm test`)
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        command: Vec<String>,
        /// Interpret the command through the platform shell instead of preserving argv boundaries
        #[arg(long)]
        shell: bool,
        /// Keep the session workspace after the run and defer reconcile
        #[arg(long)]
        keep: bool,
        /// Discard all workspace changes after the run (no reconcile)
        #[arg(long, conflicts_with = "keep")]
        discard: bool,
        /// Materialization strategy
        #[arg(long)]
        strategy: Option<String>,
    },
    /// Manage local telemetry consent and the spool
    Telemetry {
        #[command(subcommand)]
        action: TelemetryAction,
    },
    /// Show graph observability
    Support {
        /// Output machine-readable JSON for editor integrations
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// List the languages Kin understands
    Languages {
        /// Output machine-readable JSON
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Emit benchmark/runtime metadata for strict prepared-state cache keys
    #[command(hide = true)]
    BenchMeta {
        /// Output machine-readable JSON
        #[arg(long, default_value_t = false)]
        json: bool,
        /// Include repo-specific prepared-state cache manifest fields
        #[arg(long, default_value_t = false, hide = true)]
        prepared_state: bool,
        /// Static executable identity, without host or repository configuration
        #[arg(
            long,
            hide = true,
            requires = "json",
            conflicts_with = "prepared_state"
        )]
        runtime_identity_only: bool,
    },
    /// Hidden prepared-state publish/materialize surfaces for benchmark/runtime orchestration
    #[command(hide = true)]
    PreparedState {
        #[command(subcommand)]
        action: PreparedStateAction,
    },
    /// Show audit trail
    Audit {
        /// Filter by actor ID
        #[arg(long)]
        actor: Option<String>,
        /// Maximum number of events
        #[arg(long, default_value = "50")]
        limit: usize,
        /// Filter by action type
        #[arg(long)]
        action: Option<String>,
        /// Filter events since date (ISO 8601)
        #[arg(long)]
        since: Option<String>,
        /// Filter by target scope
        #[arg(long)]
        scope: Option<String>,
    },
    /// Back up and restore complete native repository state
    Backup {
        #[command(subcommand)]
        action: BackupAction,
    },
    /// Show where a working tree's store lives, and list or prune stores
    Store {
        #[command(subcommand)]
        action: StoreAction,
    },
    /// Manage change approvals
    Approvals {
        #[command(subcommand)]
        action: ApprovalsAction,
    },
    /// Scan entity graph for security patterns
    Security {
        /// Trace transitive dependency vulnerabilities
        #[arg(long)]
        propagate: bool,
    },
    /// Analyze semver impact from immutable repository-v6 changes
    Semver {
        /// Explicit base endpoint: a ref, change, HEAD, or WORKSPACE
        #[arg(long)]
        base: String,
        /// Explicit head endpoint (defaults to the committed workspace base)
        #[arg(long, default_value = "HEAD")]
        head: String,
        /// Emit the machine-readable impact report as JSON
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Cross-repo release orchestration and per-repo release snapshots
    Release {
        #[command(subcommand)]
        action: ReleaseAction,
    },
    /// Publish an exact repository-v6 tag ref
    Tag {
        /// Release tag
        tag: String,
        /// Block release if entities lack linked passing tests
        #[arg(long)]
        require_proof: bool,
        /// Require known-human approval for every reachable non-root change
        #[arg(long)]
        require_approval: bool,
        /// Force release even with low coverage
        #[arg(long)]
        force: bool,
    },
    /// Publish a new change restoring an earlier change's complete content
    ///
    /// This is not a single-change undo. Later changes remain in immutable
    /// history, but their effects are removed from the working view. Unless
    /// the target already is the tip, --discard-later must accept restoring
    /// its complete content. Restore the previous tip's content with another
    /// rollback using --discard-later; the output names that command.
    Rollback {
        /// Change whose complete content the new restoring change will carry.
        /// Omit when naming a work item with --feature.
        #[arg(required_unless_present = "feature", conflicts_with = "feature")]
        change_id: Option<String>,
        /// Roll back every change the named work item records
        #[arg(long)]
        feature: Option<String>,
        /// Accept replacing current content, even when the preview count is unknown
        #[arg(long)]
        discard_later: bool,
    },
    /// No single-change undo; rollback publishes a complete restoration
    #[command(hide = true)]
    Revert {
        /// Ignored; kin has no revert. See the refusal this prints.
        change_id: Option<String>,
    },
    /// Run benchmarks (delegates to kin-bench binary)
    Bench {
        /// Arguments to forward to kin-bench
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Move an existing Git repository into Kin, in place or to a new directory
    Migrate {
        /// Source repository path (defaults to current directory)
        source: Option<String>,
        /// Distinct destination (defaults to an in-place migration)
        #[arg(long)]
        target: Option<PathBuf>,
    },
    /// Inspect and bound the on-disk embedding cache
    Cache {
        #[command(subcommand)]
        action: CacheAction,
    },
    /// Inspect and validate the semantic graph
    Graph {
        #[command(subcommand)]
        action: GraphAction,
    },
    /// Exact Git interoperability projections
    Git {
        #[command(subcommand)]
        action: GitAction,
    },
    /// Manage agent intents (locks on scopes)
    Intent {
        #[command(subcommand)]
        action: IntentAction,
    },
    /// Show traffic (active intents) on a scope
    Traffic {
        #[command(subcommand)]
        action: TrafficAction,
    },
    /// Show the AI agents working in this repository and what they do through Kin
    Agents {
        /// Print the daemon's agents payload as JSON
        #[arg(long)]
        json: bool,
        #[command(subcommand)]
        action: Option<AgentsAction>,
    },
    /// Manage assistant adapters
    Assistant {
        #[command(subcommand)]
        action: AssistantAction,
    },
    /// Manage work items (features, tasks, issues, debt, TODOs)
    Work {
        #[command(subcommand)]
        action: WorkAction,
    },
    /// Manage annotations (comments, warnings, instructions, reasoning)
    Note {
        #[command(subcommand)]
        action: NoteAction,
    },
    /// Create a feature (alias for `kin work create --kind feature`)
    Feature {
        /// Feature title
        title: String,
        /// Optional description
        #[arg(short, long)]
        description: Option<String>,
    },
    /// Import inline TODOs as work items
    Todo {
        #[command(subcommand)]
        action: TodoAction,
    },
    /// Launch an editor over an exact graph-derived session workspace
    Open {
        /// Editor to launch: code or cursor
        editor: String,
    },
    /// Launch an assistant in an exact graph-derived session workspace
    With {
        /// Assistant to launch: claude, codex, gemini
        assistant: String,
        /// Launch the assistant with none of its built-in tools and Kin's MCP
        /// server as its only other one, so it reads and changes code through
        /// Kin, by entity; the enforcement tier is printed at launch and differs
        /// per assistant
        #[arg(long)]
        semantic_only: bool,
        /// Task prompt
        #[arg(last = true)]
        task: Vec<String>,
    },
    /// Hidden PreToolUse adjudicator for a `kin with --semantic-only` session.
    ///
    /// Reads one hook payload on stdin and exits 0 to permit or 2 to refuse.
    /// The launched assistant calls this, not an operator.
    #[command(hide = true)]
    SemanticOnlyGuard,
    /// Retire tracked paths that ignore rules now cover. Reports without changing anything
    /// unless --confirm is given.
    PurgeIgnored {
        /// Publish the removal instead of only reporting it
        #[arg(long)]
        confirm: bool,
        /// Accept a purge that removes more than 75% of a non-trivial tree
        #[arg(long)]
        confirm_mass_deletion: bool,
    },
    /// Admit the complete exact working tree into graph authority now
    ///
    /// The daemon admits a complete tree on startup, on commit, and on what its
    /// watcher observes. This is the trigger for the case none of those covers:
    /// a graph that fell behind its working tree and is waiting for churn that
    /// is not coming.
    Admit,
    /// Approve one blocked sensitive artifact for publication
    ///
    /// Admission refuses untracked content that looks like a leaked credential
    /// and names the path, digest, and entry kind an approval must carry. This
    /// records exactly that triple in the tracked .kin-allowances file, so the
    /// approval travels with the repository and a reviewer sees it in the diff.
    /// Editing the artifact changes its digest and blocks it again.
    Allow {
        /// Path to the artifact admission blocked
        path: String,
        /// Why this artifact is safe to publish, read by whoever reviews it
        #[arg(long)]
        reason: String,
    },
    /// Admit one exact disposable-session observation into repository-v6 authority
    Reconcile {
        /// Session ID (defaults to most recent session)
        session: Option<String>,
        /// Confirm an observation that removes more than 75% of a non-trivial tree
        #[arg(long)]
        confirm_mass_deletion: bool,
    },
    /// Open a shell in an exact graph-derived session workspace
    Shell {
        /// Materialization strategy
        #[arg(long)]
        strategy: Option<String>,
    },
    /// Show a quick codebase overview (entity counts by kind, language, top files)
    Overview {
        /// Compact mode: only show counts, no entity listings
        #[arg(long)]
        compact: bool,
        /// Output all entities as JSON (for programmatic use)
        #[arg(long)]
        json: bool,
    },
    /// Generate shell completions for bash, zsh, or fish
    Completions {
        /// Shell to generate completions for
        #[arg(value_enum)]
        shell: Shell,
    },
    /// Bring this store up to this build's replay semantics, keeping its history
    ///
    /// A store an older Kin build wrote serves state that build derived, and
    /// every answer is qualified until it is re-derived. This re-derives the
    /// state every branch head and the workspace serve, records each head's
    /// difference as one new change with no file change, and keeps every earlier
    /// change, branch, review, spec and history record exactly as it was. It
    /// stops this repository's daemon while it runs, and refuses, changing
    /// nothing, when it cannot finish.
    Upgrade {
        /// Output machine-readable JSON
        #[arg(long)]
        json: bool,
    },
    /// Update Kin to the latest release
    Update {
        /// Skip SHA-256 checksum verification (NOT recommended)
        #[arg(long, conflicts_with = "check_only")]
        skip_verify: bool,
        /// Release channel: `stable` (default) or `alpha` (latest pre-release,
        /// unstable). A mutating update saves the choice; check-only never writes it.
        #[arg(long, value_enum)]
        channel: Option<commands::update::Channel>,
        /// Require the selected release to have this exact SemVer. This selects
        /// a release; it does not authenticate archive bytes. Automation must
        /// provide the complete pinned expectation tuple.
        #[arg(
            long,
            requires_all = ["expect_sha", "expect_archive_sha256"],
            conflicts_with = "ack_restart",
            value_name = "SEMVER"
        )]
        expect_version: Option<semver::Version>,
        /// Require the selected release tag to peel to this exact Kin commit.
        /// This selects the tag source; it does not authenticate archive bytes.
        #[arg(
            long,
            requires_all = ["expect_version", "expect_archive_sha256"],
            conflicts_with = "ack_restart",
            value_parser = commands::update::parse_expected_commit_sha,
            value_name = "40HEX"
        )]
        expect_sha: Option<String>,
        /// Require the downloaded platform archive to match this exact SHA-256.
        /// Supply it only after external cryptographic attestation verification
        /// pins kinlab-ai/kin, release.yml, the release tag, and source commit.
        #[arg(
            long,
            requires_all = ["expect_version", "expect_sha"],
            conflicts_with = "ack_restart",
            value_parser = commands::update::parse_expected_archive_sha256,
            value_name = "64HEX"
        )]
        expect_archive_sha256: Option<String>,
        /// Check whether an update is available without downloading or installing it.
        #[arg(long)]
        check_only: bool,
        /// Emit the check-only result as JSON.
        #[arg(long, requires = "check_only")]
        json: bool,
        /// Verify the durable restart fence and exact installed binary
        /// identities for the release awaiting acknowledgement. Legacy markers
        /// may additionally require explicit replacement-session evidence.
        #[arg(
            long,
            conflicts_with_all = [
                "skip_verify",
                "channel",
                "expect_version",
                "expect_sha",
                "expect_archive_sha256",
                "check_only",
                "json"
            ]
        )]
        ack_restart: bool,
        /// Legacy-marker live replacement proof: `daemon=PID`, `mcp=PID`, or
        /// `vfs=PID`. New stop-before-update markers reject these arguments and
        /// require no replacement session evidence.
        #[arg(
            long = "runtime-session",
            requires = "ack_restart",
            value_name = "KIND=PID"
        )]
        runtime_sessions: Vec<String>,
        /// Set how an available update should reach this machine and exit.
        /// `auto` (the default) installs unattended through the gated
        /// executor: it waits for a moment with no managed Kin daemon or VFS
        /// server, defers at most a bounded window, and runs the full
        /// stop-install-acknowledge chain. An open agent session does not hold
        /// it back. `prompt` notifies with the remedy attached and waits to be
        /// told. `manual` never notifies; checks still run.
        #[arg(
            long,
            value_enum,
            value_name = "POLICY",
            conflicts_with_all = [
                "skip_verify",
                "channel",
                "expect_version",
                "expect_sha",
                "expect_archive_sha256",
                "check_only",
                "json",
                "ack_restart",
                "apply"
            ]
        )]
        set_policy: Option<commands::update::UpdatePolicy>,
        /// Bring this machine current in one gesture: install the release,
        /// acknowledge the restart fence, and repair agent configs, in that
        /// order. This is what the update notification's button runs.
        #[arg(long, conflicts_with_all = ["check_only", "json", "ack_restart"])]
        apply: bool,
        /// With --apply: print the ordered steps and change nothing.
        #[arg(long, requires = "apply")]
        dry_run: bool,
        /// Run the unattended executor (what the update watchdog invokes on a
        /// stale install with policy auto): evaluate the machine-activity
        /// gates, and on proceed stop the managed daemon and VFS server
        /// cooperatively and run the full --apply chain. Agent MCP servers keep
        /// running; each picks up the new binary when its agent next starts it.
        /// Blocked runs persist a deferral clock instead of installing. The
        /// final stdout line is one JSON record (also appended to
        /// ~/.kin/update-ledger.jsonl) carrying the decision, reason,
        /// blocked_seconds, window_seconds, and how many releases the deferral
        /// has blocked across.
        #[arg(long, conflicts_with_all = [
            "skip_verify",
            "channel",
            "expect_version",
            "expect_sha",
            "expect_archive_sha256",
            "check_only",
            "json",
            "ack_restart",
            "set_policy",
            "apply",
            "dry_run"
        ])]
        unattended: bool,
        /// With --unattended: apply despite the activity gates. For the
        /// watchdog once a deferred record shows blocked_seconds >=
        /// window_seconds, which starts at 24h and shortens as the
        /// installation falls further behind, to a floor of 6h. Never
        /// overrides a recorded prompt or manual policy, only the executor's
        /// own activity gates.
        #[arg(long, requires = "unattended")]
        force_window: bool,
        /// Prepare a verified out-of-process Windows update worker
        #[arg(long, hide = true, conflicts_with_all = [
            "skip_verify", "channel", "expect_version", "expect_sha", "expect_archive_sha256",
            "check_only", "ack_restart", "runtime_sessions", "set_policy", "apply", "dry_run",
            "unattended", "force_window", "windows_worker"
        ])]
        prepare_windows_worker: bool,
        /// Verify the native updater's exact Windows handoff before replacement
        #[arg(long, hide = true, value_name = "RECORD", conflicts_with_all = [
            "prepare_windows_worker", "skip_verify", "check_only", "json", "ack_restart", "runtime_sessions",
            "set_policy", "unattended", "force_window", "dry_run"
        ])]
        windows_worker: Option<PathBuf>,
        /// Remove an authenticated, completed Windows update worker handoff
        #[arg(long, hide = true, value_name = "RECORD", conflicts_with_all = [
            "skip_verify", "channel", "expect_version", "expect_sha", "expect_archive_sha256",
            "check_only", "json", "ack_restart", "runtime_sessions", "set_policy", "apply", "dry_run",
            "unattended", "force_window", "prepare_windows_worker", "windows_worker",
            "bootstrap_published", "expect_target", "expect_archive_name"
        ])]
        cleanup_windows_worker: Option<PathBuf>,
        /// Admit an independently verified published archive into the managed install
        #[arg(long, hide = true, requires_all = [
            "channel", "expect_version", "expect_sha", "expect_archive_sha256",
            "expect_target", "expect_archive_name"
        ], conflicts_with_all = [
            "skip_verify", "check_only", "json", "ack_restart", "runtime_sessions",
            "set_policy", "dry_run", "unattended", "force_window",
            "prepare_windows_worker", "windows_worker"
        ])]
        bootstrap_published: bool,
        #[arg(
            long,
            hide = true,
            requires = "bootstrap_published",
            value_name = "TARGET"
        )]
        expect_target: Option<String>,
        #[arg(
            long,
            hide = true,
            requires = "bootstrap_published",
            value_name = "ARCHIVE"
        )]
        expect_archive_name: Option<String>,
    },
    /// Show or manage the global Kin repository registry
    Registry {
        #[command(subcommand)]
        action: Option<RegistryAction>,
    },
    /// Inspect Kin daemons, stop them gracefully, or ask one to enrich
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },
    /// Probe first-run health and optionally apply safe repairs
    Doctor {
        /// Apply safe automatic repairs (shell hook, MCP configs, config dirs)
        #[arg(long, default_value_t = false)]
        fix: bool,
        /// With --fix: also remove every other copy of Kin that has its own
        /// uninstall
        ///
        /// Runs each copy's own channel (npm, cargo, Homebrew), or deletes a
        /// standalone release file. Never removes a development build or
        /// anything in the managed Kin home. `--fix` alone never removes a
        /// copy, because `kin update` runs it unattended.
        #[arg(
            long = "remove-other-copies",
            requires = "fix",
            default_value_t = false
        )]
        remove_other_copies: bool,
        /// Install the missing language servers this repository's languages need
        ///
        /// Only for the languages the repository uses. An install can write
        /// outside Kin's directory (npm global, or the active rustup toolchain),
        /// and the run says so before it does, so it never runs unasked: an
        /// interactive run asks first, and a scripted one needs this flag.
        #[arg(long = "install-language-servers", default_value_t = false)]
        install_language_servers: bool,
        /// Emit the machine-readable health report as JSON
        #[arg(long, default_value_t = false)]
        json: bool,
        /// List every check on a terminal, not only the ones that need something
        #[arg(long, default_value_t = false)]
        verbose: bool,
        /// Compare an explicit projection observation with graph truth
        #[arg(long, default_value_t = false)]
        drift: bool,
        /// Rematerialize the derived projection from graph truth, DISCARDING
        /// uncommitted changes to tracked files that diverge from it
        #[arg(long, default_value_t = false)]
        heal: bool,
        /// Delete the staged stores a crashed `kin init` left beside this
        /// directory and its parent
        ///
        /// Removes only a stage no live `kin init` on this machine owns and
        /// whose destination repository was never created. The lock that proves
        /// that is host-local, so run this on local storage: on a shared mount a
        /// stage another machine is still writing can read as abandoned. Bare
        /// `kin doctor` is the dry run: it names every stage this would take,
        /// with its size, and deletes nothing.
        #[arg(long = "reclaim-staging", default_value_t = false)]
        reclaim_staging: bool,
        /// Report one file's persisted conversion coverage and its entity
        /// counts by kind
        ///
        /// A conversion diagnostic: whether an adapter parsed the file and how
        /// completely, the tier it is tracked at, whether its type is one no
        /// adapter claims, and how many entities of each kind conversion
        /// produced. It lists no entities. A path the graph does not track is
        /// refused.
        #[arg(
            long = "conversion-source",
            value_name = "PATH",
            conflicts_with_all = ["fix", "install_language_servers", "drift", "heal", "reclaim_staging"]
        )]
        conversion_source: Option<String>,
    },
    /// Set up this machine: connect AI clients and add kin to your shell
    Setup {
        #[command(subcommand)]
        action: Option<SetupAction>,
        /// What to set up: local, agent, editor, hosted, or advanced
        ///
        /// `local` sets up kin in your shell and connects no AI client.
        /// `agent` also connects the AI clients installed here. `editor` and
        /// `hosted` are local setup plus a pointer to the editor extension or
        /// your KinLab sign-in. `advanced` lets you choose each step.
        #[arg(long, global = true, value_parser = ["local", "agent", "editor", "hosted", "advanced"])]
        intent: Option<String>,
        /// Ignored: Kin has one repository format, so there is no mode to
        /// choose. Still accepted, and said to be ignored, so a script that
        /// passes it keeps running.
        #[arg(long, global = true, hide = true)]
        mode: Option<String>,
        /// Shell to configure: zsh, bash, or powershell
        #[arg(long, global = true)]
        shell: Option<String>,
        /// Start the Kin daemon on its own when you enter a Kin repository
        #[arg(long, global = true)]
        auto_daemon: bool,
        /// Ask nothing: use the defaults and any flags given
        #[arg(long, global = true)]
        no_interactive: bool,
        /// Skip the test call that proves each connected AI client can reach
        /// Kin (for a scripted install outside a repository)
        #[arg(long, global = true)]
        skip_mcp_check: bool,
        /// Connect only these AI clients, by the ids `kin setup clients`
        /// lists, leaving every other client as it is
        ///
        /// An empty list (`--clients ""`) runs the rest of setup and connects
        /// none. To add kin to your shell and do nothing else, use
        /// `--shell-only`.
        #[arg(long, global = true, value_delimiter = ',', value_name = "ID,...")]
        clients: Option<Vec<String>>,
        /// Add kin to your shell and nothing else: connect no AI client
        ///
        /// Runs only setup's shell step: the shell hook, and the line that puts
        /// Kin's bin directory on PATH in the profile your shell reads. It asks
        /// nothing, connects no AI client, and leaves the daemon, the prompt
        /// hook and language servers as they are. What it writes is recorded,
        /// so `kin doctor` checks it and `kin setup uninstall` removes it.
        /// Exits 1 when the profile cannot be written, after naming the lines
        /// to add by hand.
        #[arg(
            long = "shell-only",
            conflicts_with_all = [
                "clients",
                "prompt_hook",
                "tool_profile",
                "install_language_servers",
                "resource_profile",
                "embedding_model",
                "embedding_provider",
                "auto_daemon",
                "check"
            ]
        )]
        shell_only: bool,
        /// With --shell-only: print the result as one JSON object
        /// (`kin.setup-shell.v1`)
        #[arg(long, requires = "shell_only")]
        json: bool,
        /// Add Kin's Claude Code prompt hook without asking, so Kin answers a
        /// code question before the model's first turn. An interactive run
        /// asks first; a scripted one writes into agent settings only with
        /// this flag
        #[arg(long = "prompt-hook", global = true)]
        prompt_hook: bool,
        /// Install missing language servers without asking
        ///
        /// Inside a repository, only the servers its languages need; outside
        /// one, the servers for the languages Kin supports. An install can
        /// write outside Kin's directory (npm's global packages, or the active
        /// rustup toolchain), and the run says so before it does, so it never
        /// runs unasked: an interactive run asks first, and a scripted one
        /// needs this flag.
        #[arg(long = "install-language-servers", global = true)]
        install_language_servers: bool,
        /// Record this machine's resource profile without the advanced prompt
        ///
        /// The opening hardware check prints what it detected and what it
        /// recommends either way. A profile that budgets past what the machine
        /// actually has can exceed safe memory and GPU thresholds.
        #[arg(long = "resource-profile", global = true, value_parser = ["proof", "interactive", "throughput", "ci"])]
        resource_profile: Option<String>,
        /// When to download the search model: later (recommended) or never
        ///
        /// Setup itself never downloads it. `later` lets `kin embed` and
        /// `kin init` download it the first time they need it; `never` records
        /// that this machine does not, which `kin doctor` then reports as your
        /// choice.
        #[arg(long = "embedding-model", global = true, value_parser = ["later", "never"])]
        embedding_model: Option<String>,
        /// Where optional embeddings come from: none (default), built-in, or
        /// external
        ///
        /// `local` is another name for `built-in`, and `remote` for `external`.
        /// `external` collects no credential: it prints the environment
        /// variables an OpenAI-compatible endpoint needs, and says that entity
        /// text is sent to it.
        #[arg(long = "embedding-provider", global = true, value_parser = ["none", "built-in", "external", "local", "remote"])]
        embedding_provider: Option<String>,
        /// Leave your shell profile without the line that puts kin on PATH
        ///
        /// Setup asks about this only for an npm or npx install, which cannot
        /// add the line itself. Without it, a new terminal does not find kin.
        #[arg(long = "skip-path", global = true)]
        skip_path: bool,
        /// Tool profile to write into every AI client this run configures
        ///
        /// Without it each client gets the profile that suits it: agent-routed
        /// for a client that sends every tool with every request, agent-default
        /// for the rest. A profile set by hand is kept. With it, the entry is
        /// marked with KIN_MCP_TOOL_PROFILE_PINNED=1 so later runs of `kin
        /// setup` and `kin update` keep this profile too.
        #[arg(
            long = "tool-profile",
            global = true,
            value_parser = ["agent-default", "agent-query", "agent-search", "agent-routed", "agent-routed-query"]
        )]
        tool_profile: Option<String>,
        /// Print every step and check in full
        ///
        /// Setup prints a short summary: the questions it asked (or that a
        /// run without a terminal took the defaults), what needs you, the AI
        /// clients it connected and what to run next. This prints the full
        /// record instead, every step and every health check.
        #[arg(long, default_value_t = false)]
        verbose: bool,
        /// Only run the health check, without the questions
        #[arg(long, default_value_t = false)]
        check: bool,
    },
    /// Engage, disengage, or report the filesystem projection for this repository
    Vfs {
        #[command(subcommand)]
        action: VfsAction,
    },
    /// Send a user-facing notification through Kin's own identity
    Notify {
        #[command(subcommand)]
        action: Option<NotifyAction>,
        /// Notification title
        #[arg(long)]
        title: Option<String>,
        /// Notification body
        #[arg(long)]
        body: Option<String>,
        /// Urgency: info (silent), warn (silent), or urgent (sound, breaks through Focus)
        #[arg(long, default_value = "info", value_parser = ["info", "warn", "urgent"])]
        level: String,
        /// Suppression and replacement identity; reposting under the same key
        /// replaces the previous notification instead of stacking another
        #[arg(long)]
        key: Option<String>,
        /// With --key: re-notify only after this many seconds have passed
        #[arg(long)]
        cooldown: Option<u64>,
        /// With --key: notify once, then stay quiet until `kin notify clear`
        #[arg(long, default_value_t = false)]
        latch: bool,
        /// Emit the outcome as JSON
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Manage secrets (org and repo level)
    Secret {
        #[command(subcommand)]
        action: SecretAction,
    },
    /// Manage CI/CD pipelines
    Pipeline {
        #[command(subcommand)]
        action: PipelineAction,
    },
    /// Manage hosted releases
    #[command(name = "hosted-release")]
    HostedRelease {
        #[command(subcommand)]
        action: HostedReleaseAction,
    },
    /// Inspect host/accelerator/memory resources and per-profile budgets
    Resources {
        #[command(subcommand)]
        action: ResourcesAction,
    },
}

#[derive(Subcommand)]
enum RuntimeAction {
    /// Report exact executable identities without opening a repository
    Status {
        #[arg(long)]
        json: bool,
        /// Inspect an isolated development pair instead of the managed install
        #[arg(long)]
        development: bool,
        /// Development CLI whose adjacent daemon should be inspected
        #[arg(long, requires = "development", value_name = "PATH")]
        binary: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum HostedPublicationAction {
    /// Build the reserved repository's durable authority and report what landed
    Publish {
        /// Operation-bound manifest naming the reservation, source and destination
        #[arg(long, value_name = "PATH")]
        manifest: PathBuf,
        /// Trusted materialized source, as it exists on this machine
        #[arg(long, value_name = "DIR")]
        source: PathBuf,
        /// Where the intended publication is made durable before it is permanent
        #[arg(long, value_name = "PATH")]
        intent_out: PathBuf,
        /// Where the evidence record is written
        #[arg(long, value_name = "PATH")]
        evidence_out: PathBuf,
        /// The reserved identity the caller believes the manifest holds
        #[arg(long, value_name = "ID")]
        expect_repository_id: String,
        /// native-empty or exact-git, refused when it disagrees with the manifest
        #[arg(long, value_name = "MODE")]
        expect_mode: String,
    },
    /// Read a destination against a durable intent, writing nothing
    Verify {
        /// Operation-bound manifest naming the reservation and destination
        #[arg(long, value_name = "PATH")]
        manifest: PathBuf,
        /// The durable intent to verify against, when it is not in the manifest
        #[arg(long, value_name = "PATH")]
        intent: Option<PathBuf>,
        /// Where the evidence record is written
        #[arg(long, value_name = "PATH")]
        evidence_out: PathBuf,
        /// The reserved identity the caller believes the manifest holds
        #[arg(long, value_name = "ID")]
        expect_repository_id: String,
        /// native-empty or exact-git, refused when it disagrees with the manifest
        #[arg(long, value_name = "MODE")]
        expect_mode: String,
    },
}

#[derive(Subcommand)]
enum NotifyAction {
    /// Release a latch or cooldown so the next send is delivered
    Clear {
        /// The suppression key to forget
        key: String,
        /// Emit the result as JSON
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Report which backend would deliver and what is currently held back
    Status {
        /// Emit the report as JSON
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Ask macOS whether Kin may send you notifications
    ///
    /// Run it in Terminal on the Mac's own screen: macOS shows the request
    /// there, so from an SSH session it asks nothing and says so. A decision
    /// macOS already holds is reported, never asked again.
    Authorize {
        /// Seconds to wait for the answer; an unanswered request stays open
        #[arg(
            long,
            value_name = "SECONDS",
            default_value_t = kin_notify::authorization::DEFAULT_WAIT.as_secs(),
            value_parser = clap::value_parser!(u64).range(1..=600)
        )]
        wait: u64,
        /// Emit the outcome as JSON
        #[arg(long, default_value_t = false)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum ResourcesAction {
    /// Record resource knobs for this repository so they survive a daemon restart
    Set {
        /// Resource profile the daemon adopts at its next start: proof, interactive, throughput, or ci
        #[arg(long)]
        profile: Option<String>,
        /// Batch size for the daemon's background embedding queue
        #[arg(long)]
        embed_batch_size: Option<usize>,
        /// Remove the recorded knobs and go back to the built-in defaults
        #[arg(long)]
        clear: bool,
    },
    /// Report the detected resource plan and live daemon embedding state
    Inspect {
        /// Output the stable JSON resource plan instead of a human summary
        #[arg(long)]
        json: bool,
        /// Resource profile to plan for: proof, interactive, throughput, or ci
        #[arg(long)]
        profile: Option<String>,
    },
}

#[derive(Subcommand)]
enum BranchAction {
    /// List byte-exact repository-v6 branch refs
    List {
        /// Output exact ref names and targets as JSON
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Create a ref with compare-and-swap
    Create {
        /// UTF-8 short branch name or fully-qualified refs/heads/... name
        #[arg(required_unless_present = "ref_hex", conflicts_with = "ref_hex")]
        name: Option<String>,
        /// Canonical lowercase hex for a fully-qualified byte-exact branch ref
        #[arg(long, value_name = "LOWER_HEX")]
        ref_hex: Option<String>,
    },
    /// Delete a ref with force-with-lease
    Delete {
        /// UTF-8 short branch name or fully-qualified refs/heads/... name
        #[arg(required_unless_present = "ref_hex", conflicts_with = "ref_hex")]
        name: Option<String>,
        /// Canonical lowercase hex for a fully-qualified byte-exact branch ref
        #[arg(long, value_name = "LOWER_HEX")]
        ref_hex: Option<String>,
    },
    /// Switch workspace authority and projection atomically
    ///
    /// Uncommitted work comes with you, the way it does across a Git checkout.
    /// Pending work at a path the destination branch does not track moves
    /// across and is still uncommitted when you arrive. Pending work at a path
    /// the destination already tracks with identical content becomes an
    /// ordinary member of that branch. A pending edit to a member both branches
    /// hold identically moves across too.
    ///
    /// The switch refuses only where replaying the work would lose something:
    /// a new member the destination already holds with different content, or
    /// an edit to a member the destination holds differently or does not hold
    /// at all. It names the entities every blocked change holds, and commit or
    /// `kin stash push` clears the way.
    ///
    /// With --detach, NAME is any point in history kin admitted, such as
    /// git:<sha> (whole or a unique prefix), a change id, or HEAD~3, and the
    /// workspace moves there with a detached head by the same exact
    /// transition, applying only what differs and parsing nothing.
    Switch {
        /// UTF-8 short branch name or fully-qualified refs/heads/... name
        #[arg(required_unless_present = "ref_hex", conflicts_with = "ref_hex")]
        name: Option<String>,
        /// Canonical lowercase hex for a fully-qualified byte-exact branch ref
        #[arg(long, value_name = "LOWER_HEX")]
        ref_hex: Option<String>,
        /// Detach at the admitted commit or change NAME instead of entering a branch
        #[arg(long, default_value_t = false, conflicts_with = "ref_hex")]
        detach: bool,
    },
}

#[derive(Subcommand)]
enum StoreAction {
    /// Print the store serving a directory, in the tree or kept outside it
    ///
    /// A new store lives in the central stores directory under the Kin home.
    /// A store an earlier version created stays in the working tree as `.kin`,
    /// and `kin init --store` or `KIN_STORE` can put one anywhere else.
    /// `--json` also names the working tree and says whether the store is
    /// external. Starts nothing.
    Path {
        /// Directory to ask about (defaults to the current directory)
        path: Option<PathBuf>,
        /// Output machine-readable JSON
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Make a copied or moved store, kept outside any tree, serve a working tree
    ///
    /// For a copy of a prepared store taken for one checkout, such as an APFS
    /// clone, or a store a moved checkout left behind. The store's contents
    /// are unchanged: the first daemon reconciles the tree, and `kin branch
    /// switch --detach git:<sha>` moves the store to the commit the checkout
    /// holds without parsing anything.
    Attach {
        /// The store directory to attach
        store: PathBuf,
        /// Working tree it serves (defaults to the current directory)
        path: Option<PathBuf>,
        /// Output machine-readable JSON
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// List the stores kept outside their working trees, and how each stands
    ///
    /// Every store the store index names and every store in the central
    /// stores directory: `serving` its tree, `detached` (its tree exists and
    /// finds another store or none), `gone` (its tree was moved or deleted),
    /// `unreachable` (its tree and the directory above it are missing, maybe
    /// on a volume that is not mounted), or `reserved` (a location with no
    /// store yet). Starts nothing.
    List {
        /// Also measure what each store occupies on disk
        #[arg(long, default_value_t = false)]
        size: bool,
        /// Output machine-readable JSON
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Delete central stores whose working tree is gone
    ///
    /// Reports what it would remove unless `--yes` is given: a store holds the
    /// native Kin history of its tree, which Git cannot rebuild. Only stores in
    /// the central stores directory are deleted, never one a daemon is still
    /// serving. A store whose tree is merely unreachable is kept unless
    /// `--unreachable` is given.
    Prune {
        /// Delete the stores instead of only listing them
        #[arg(long, default_value_t = false)]
        yes: bool,
        /// Include stores whose tree and its parent directory are both missing
        #[arg(long, default_value_t = false)]
        unreachable: bool,
        /// Output machine-readable JSON
        #[arg(long, default_value_t = false)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum BackupAction {
    /// Create a complete native recovery backup outside .kin
    Create {
        /// Optional tag to label the backup
        #[arg(short, long)]
        tag: Option<String>,
        /// Absent carrier directory with an existing parent
        #[arg(long)]
        output: Option<PathBuf>,
    },
    /// List available backups
    List {
        /// Output machine-readable JSON
        #[arg(long, default_value_t = false)]
        json: bool,
        /// Carrier directory to list even if the original repository was lost
        #[arg(long)]
        directory: Option<PathBuf>,
    },
    /// Restore a complete recovery carrier into a fresh .kin directory
    Restore {
        /// Legacy graph-only name, retained to report safe recovery guidance
        name: Option<String>,
        /// Legacy in-place restore, retained to report safe recovery guidance
        #[arg(long)]
        latest: bool,
        /// Complete recovery carrier directory
        #[arg(long, requires = "target", conflicts_with_all = ["name", "latest"])]
        from: Option<PathBuf>,
        /// Absent .kin directory inside an existing destination working directory
        #[arg(long, requires = "from")]
        target: Option<PathBuf>,
    },
    /// Delete a specific backup
    Delete {
        /// Exact recovery carrier name from backup list
        name: String,
    },
}

#[derive(Subcommand)]
enum PreparedStateAction {
    /// Publish the current repo's .kin state into a prepared-state directory
    Publish {
        /// Target prepared-state directory
        #[arg(long)]
        target: PathBuf,
        /// Output machine-readable JSON
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Materialize a prepared-state directory into the current repo
    Materialize {
        /// Source prepared-state directory
        #[arg(long)]
        source: PathBuf,
        /// Output machine-readable JSON
        #[arg(long, default_value_t = false)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum SpecAction {
    /// Create a new spec
    Create {
        /// Spec intent description
        intent: String,
    },
    /// List specs
    List {
        /// Output machine-readable JSON
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Show a spec
    Show {
        /// Spec ID
        id: String,
    },
}

#[derive(Subcommand)]
enum GraphAction {
    /// Quick health check of the semantic graph
    Status {
        /// Output machine-readable JSON: the rendered lines beside the
        /// structured reference-edge coverage, relation census, graph section
        /// and call-site shares
        #[arg(long, default_value_t = false)]
        json: bool,
        /// Exact graph-owned dependency paths; every requested path is accounted for
        #[arg(long = "dependency")]
        dependencies: Vec<String>,
        /// Continue the same immutable status observation
        #[arg(long)]
        cursor: Option<String>,
        /// Maximum serialized status payload bytes
        #[arg(long)]
        max_chars: Option<usize>,
    },
    /// Structural integrity validation
    Validate,
    /// Persist the current workspace base graph section for faster reopen
    Materialize {
        /// Output machine-readable JSON
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Show the owed derivation ledger repository authority holds: each source
    /// body owed a parse, and the re-derivation that last paid the workspace,
    /// then the files whose callers are owed enrichment. Reads the local store
    /// only; starts no daemon, admits nothing, and migrates no earlier build's
    /// records
    Owed {
        /// Output machine-readable JSON (`kin.graph.owed-derivations.v1`)
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Look up an entity by name and show its relations
    Inspect {
        /// Entity name or UUID to inspect. A name can carry its pin:
        /// `Name@file`, `Name@file:line`, `Name#kind`
        name: String,
        /// Keep only the entities in this exact repo-relative file
        #[arg(long)]
        file: Option<String>,
        /// Keep only entities of this exact kind (for example: function)
        #[arg(long)]
        kind: Option<String>,
        /// Output machine-readable JSON ({lines, error}); missing entities exit 0 with structured error.
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Print the exact implementation body for an entity
    Source(EntitySourceArgs),
    /// Alias for source: print the exact implementation body for an entity
    Body(EntitySourceArgs),
    /// Export the drawable projection of the live graph as JSON
    Export {
        /// Cap the exported node count, sampled by degree with per-module
        /// quotas. 0 exports every entity; omitted uses the default cap
        #[arg(long, value_name = "N")]
        limit: Option<usize>,
        /// Keep only these entity kinds (comma-separated, e.g. function,class)
        #[arg(long, value_name = "KINDS")]
        kinds: Option<String>,
        /// Keep only entities whose file starts with this repository path prefix
        #[arg(long, value_name = "PREFIX")]
        path: Option<String>,
        /// Attach optional node fields (comma-separated: signature,line)
        #[arg(long, value_name = "FIELDS")]
        include: Option<String>,
        /// Write the payload to this file instead of stdout
        #[arg(long, value_name = "FILE")]
        out: Option<PathBuf>,
        /// Output machine-readable JSON
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Follow live graph changes, one event per line
    Watch {
        /// Keep only these event types (comma-separated, e.g.
        /// EntityChanged,RelationChanged)
        #[arg(long, value_name = "TYPES")]
        types: Option<String>,
        /// Output machine-readable NDJSON, one event object per line
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Serve an interactive force-directed visualization of the semantic graph
    Viz {
        /// Port to bind the local HTTP server to
        #[arg(long, default_value_t = 4220)]
        port: u16,
        /// Open the visualization in the system default browser
        #[arg(long, default_value_t = false)]
        open: bool,
        /// Cap the drawn node count, sampled by degree with per-module quotas.
        /// 0 draws every entity; omitted uses the default cap
        #[arg(long, value_name = "N")]
        limit: Option<usize>,
    },
}

/// What `kin source`, `kin graph source` and `kin graph body` take: one
/// definition, so the three cannot drift apart.
#[derive(clap::Args)]
struct EntitySourceArgs {
    /// Entity name or ID. A name with twins can carry its pin:
    /// `Name@file`, `Name@file:line`, `Name#kind`
    entity: String,
    /// Exact repo-relative file of the entity, when its name has twins
    #[arg(long)]
    file: Option<String>,
    /// Exact entity kind (for example: function), when its name has twins
    #[arg(long)]
    kind: Option<String>,
    /// Output machine-readable JSON
    #[arg(long, default_value_t = false)]
    json: bool,
}

impl EntitySourceArgs {
    /// Print the entity's body. The flags are the `Name#kind@path` pin every
    /// resolver reads, so the daemon sees one spelling.
    async fn print(self) -> Result<()> {
        let entity = kin_cli::entity_ref::compose_entity_ref(
            &self.entity,
            self.file.as_deref(),
            self.kind.as_deref(),
        );
        commands::graph::source(entity, self.json).await
    }
}

#[derive(Subcommand)]
enum CacheAction {
    /// Report embedding-cache size, composition, and age distribution
    Status {
        /// Output machine-readable JSON instead of a human summary
        #[arg(long, default_value_t = false)]
        json: bool,
        /// Stop scanning after this many entries and report the partial totals.
        /// Unset scans the whole cache, which on a bench-scale tree takes minutes
        /// but is the only way the totals are exact
        #[arg(long, value_name = "ENTRIES")]
        limit: Option<u64>,
    },
    /// Reclaim space: drop abandoned schema versions and/or evict oldest entries to a budget
    Gc {
        /// Report what would be reclaimed without deleting anything
        #[arg(long)]
        dry_run: bool,
        /// Evict the oldest entries until the cache fits this many gigabytes.
        /// Overrides KIN_EMBED_CACHE_BUDGET_GB; unset means no budget eviction.
        #[arg(long, value_name = "GB")]
        budget_gb: Option<f64>,
        /// Also remove every abandoned (non-current) schema-version subtree
        #[arg(long)]
        prune_stale_schema: bool,
    },
}

#[derive(Subcommand)]
enum GitAction {
    /// Export exact objects, refs, aliases, and source CAS to a new Git repo
    Export {
        /// New target directory (must be outside the Kin working repository)
        #[arg(short, long)]
        output: PathBuf,
    },
}

#[derive(Subcommand)]
enum RemoteAction {
    /// List configured and detected remotes
    List {
        /// Output machine-readable JSON
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Add or update a configured remote
    Add {
        /// Remote name
        name: String,
        /// Host kind: github or kinlab
        #[arg(long)]
        host: String,
        /// Transport kind: git-export or native-kin
        #[arg(long)]
        transport: String,
        /// Optional remote URL or locator
        #[arg(long)]
        url: Option<String>,
        /// Publish review state to this remote
        #[arg(long, default_value_t = false)]
        publish_review_state: bool,
        /// Publish proofs to this remote
        #[arg(long, default_value_t = false)]
        publish_proofs: bool,
        /// Set as the default remote
        #[arg(long, default_value_t = false)]
        default: bool,
    },
    /// Negotiate an exact closure and lease-protected push plan, moving nothing
    PlanPush {
        /// Remote name (defaults to the configured default native-kin remote)
        #[arg(long)]
        remote: Option<String>,
        /// Peer transfer base URL, overriding any configured remote
        #[arg(long)]
        url: Option<String>,
        /// Ref to plan for (defaults to the repository default ref)
        #[arg(long = "ref")]
        reference: Option<String>,
        /// Print the negotiated plan as JSON
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Acquire a graph-aware session lease for a native Kin remote
    Lease {
        /// Remote name (defaults to configured default)
        #[arg(long)]
        remote: Option<String>,
        /// Override the actor ID sent to KinLab
        #[arg(long)]
        actor_id: Option<String>,
        /// Optional lease TTL in seconds
        #[arg(long)]
        ttl_seconds: Option<u64>,
        /// Print the full lease payload as JSON
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// List active hosted repo sessions for a native Kin remote
    Sessions {
        /// Remote name (defaults to configured default)
        #[arg(long)]
        remote: Option<String>,
        /// Print the full session payload as JSON
        #[arg(long, default_value_t = false)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum IntentAction {
    /// List all active intents
    List,
    /// Register a new intent (lock a scope)
    Register {
        /// Scope to lock (entity:<uuid>, contract:<uuid>, file:<path>, artifact:<path>, or an entity UUID)
        scope: String,
        /// Lock type: hard or soft
        #[arg(short, long, default_value = "soft")]
        lock: String,
        /// Task description
        #[arg(short, long)]
        task: String,
        /// Session ID (defaults to a new CLI session)
        #[arg(short, long)]
        session: Option<String>,
    },
    /// Release a specific intent
    Release {
        /// Intent ID to release
        intent_id: String,
    },
    /// Clear all intents for a session
    Clear {
        /// Session ID whose intents to clear
        session_id: String,
    },
}

#[derive(Subcommand)]
enum ReviewAction {
    /// Shadow-mode merge gate: evaluate a PR-shaped change and emit a
    /// report-only verdict with blast radius, repair context, and audit
    /// evidence. Never blocks and never mutates graph state.
    Shadow {
        /// Change range as <base>..<head>. Refs accept branch names,
        /// semantic change IDs, and imported Git commit SHAs.
        #[arg(
            required_unless_present = "base",
            conflicts_with_all = ["base", "head"]
        )]
        range: Option<String>,
        /// Base ref (alternative to the positional range; pair with --head)
        #[arg(long, requires = "head")]
        base: Option<String>,
        /// Head ref (alternative to the positional range; pair with --base)
        #[arg(long, requires = "base")]
        head: Option<String>,
        /// Change title for the report (e.g. PR title)
        #[arg(long)]
        title: Option<String>,
        /// Source URL for the report (e.g. PR URL)
        #[arg(long = "source-url")]
        source_url: Option<String>,
        /// Change author identity for the report
        #[arg(long)]
        author: Option<String>,
        /// Emit the report as machine-readable JSON
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Create a new review
    Create {
        /// Review title
        #[arg(short, long)]
        title: String,
        /// Base ref (branch name or change ID)
        #[arg(long)]
        base: String,
        /// Head ref (branch name or change ID)
        #[arg(long)]
        head: String,
        /// Optional description
        #[arg(short, long)]
        description: Option<String>,
    },
    /// Record a review decision (approve, needs-work, block)
    Decide {
        /// Review ID
        review_id: String,
        /// Decision state: approved, needs_work, blocked
        #[arg(long)]
        state: String,
        /// Optional comment
        #[arg(long)]
        comment: Option<String>,
    },
    /// Add a note to a review
    Note {
        /// Review ID
        review_id: String,
        /// Note body
        #[arg(long)]
        body: String,
        /// Optional scope (entity:<uuid> or artifact:<path>)
        #[arg(long)]
        scope: Option<String>,
    },
    /// Start a discussion thread on a review
    Discuss {
        /// Review ID
        review_id: String,
        /// Discussion body
        #[arg(long)]
        body: String,
        /// Optional scope (entity:<uuid> or artifact:<path>)
        #[arg(long)]
        scope: Option<String>,
    },
    /// Reply to a discussion thread
    Reply {
        /// Discussion ID
        discussion_id: String,
        /// Reply body
        #[arg(long)]
        body: String,
    },
    /// Resolve a discussion thread
    Resolve {
        /// Discussion ID
        discussion_id: String,
    },
    /// Assign a reviewer
    Assign {
        /// Review ID
        review_id: String,
        /// Reviewer identity (email or handle)
        #[arg(long)]
        reviewer: String,
    },
    /// List reviews
    List {
        /// Filter by state: pending, approved, needs_work, blocked
        #[arg(long)]
        state: Option<String>,
    },
    /// Show a specific review with all details
    Show {
        /// Review ID
        review_id: String,
    },
    /// Walk a change step by step: connected entities in reading order,
    /// each with its before and after text and what reaches it
    Guide {
        /// What to walk: a change ID, branch name or imported Git commit, or
        /// a range as <base>..<head> (defaults to the current change)
        change: Option<String>,
        /// Walk the uncommitted work in the working tree
        #[arg(long, default_value_t = false, conflicts_with_all = ["change", "base"])]
        working: bool,
        /// Walk everything the change reaches that this ref does not
        #[arg(long, value_name = "REF")]
        base: Option<String>,
        /// Output the walkthrough as machine-readable JSON
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Serve the guided review as a local page on this machine
    Open {
        /// What to open first: a change, or a range as <base>..<head>
        /// (defaults to the current change)
        change: Option<String>,
        /// Open the uncommitted work in the working tree first
        #[arg(long, default_value_t = false, conflicts_with_all = ["change", "base"])]
        working: bool,
        /// Open everything the change reaches that this ref does not
        #[arg(long, value_name = "REF")]
        base: Option<String>,
        /// Port to bind on 127.0.0.1. When the default is taken, the next
        /// free port is used
        #[arg(long)]
        port: Option<u16>,
        /// Serve the page without opening a browser
        #[arg(long, default_value_t = false)]
        no_browser: bool,
    },
}

#[derive(Subcommand)]
enum AgentsAction {
    /// Answer an agent client's edit hook: read the hook's JSON on stdin and
    /// tell this repository's daemon which session made the edit
    #[command(name = "record-edit", hide = true)]
    RecordEdit {
        /// The client whose hook this is
        #[arg(long, value_parser = commands::agent_edits::EDIT_HOOK_CLIENTS)]
        client: String,
        /// Wait for the daemon and print what became of the report
        #[arg(long)]
        wait: bool,
    },
}

#[derive(Subcommand)]
enum TrafficAction {
    /// Show active traffic on a scope
    Show {
        /// Scope to query (entity:<uuid>, contract:<uuid>, file:<path>, artifact:<path>, or an entity UUID)
        scope: String,
    },
    /// List all active sessions
    Sessions,
}

#[derive(Subcommand)]
enum AssistantAction {
    /// Install an assistant adapter
    Install {
        /// Assistant name: claude-code, codex, gemini-cli, cursor, generic
        assistant: String,
    },
    /// Run connectivity checks
    Doctor {
        /// Specific assistant to check (checks all if omitted)
        assistant: Option<String>,
    },
    /// List installed adapters
    List {
        /// Output machine-readable JSON
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Sync managed doc blocks
    Sync,
    /// Configure managed doc sync targets
    Configure {
        /// Sync mode: manual, on-commit, daemon-auto
        #[arg(long)]
        sync_mode: Option<String>,
        /// Enable a target file
        #[arg(long)]
        enable: Option<String>,
        /// Disable a target file
        #[arg(long)]
        disable: Option<String>,
    },
    /// Generate ready-to-paste config snippets
    Snippets {
        /// Specific assistant (defaults to all MCP-capable)
        assistant: Option<String>,
    },
    /// Show recommended hook templates
    Hooks {
        /// Specific assistant (defaults to claude-code)
        assistant: Option<String>,
    },
    /// Generate injectable prompt guidance
    Prompt {
        /// Assistant: claude, codex, gemini
        #[arg(long)]
        assistant: String,
        /// Mode: normal or benchmark
        #[arg(long, default_value = "normal")]
        mode: String,
    },
}

#[derive(Subcommand)]
enum StashAction {
    /// Seal exact graph-owned workspace state and return the workspace to its base.
    Push {
        /// Label the sealed state. Defaults to the workspace head it was sealed on.
        #[arg(long, short = 'm')]
        message: Option<String>,
        /// Skip the typed confirmation for discarding the projected working
        /// files (for non-interactive use).
        #[arg(long)]
        yes: bool,
    },
    /// Restore the most recently sealed workspace state and drop its stash
    Pop,
    /// List sealed workspace states
    List {
        /// Output the machine-readable stash report
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum AgentAction {
    /// Run one task to completion against an OpenAI-compatible endpoint
    Run {
        /// The task: a path to a file holding it, or the text itself
        #[arg(long, value_name = "FILE|TEXT")]
        task: String,
        /// Model id as the endpoint names it
        #[arg(long, value_name = "ID")]
        model: String,
        /// OpenAI-compatible base URL, with or without a trailing /v1
        #[arg(long = "base-url", value_name = "URL")]
        base_url: String,
        /// Name of an environment variable holding the API key. The key itself is never
        /// accepted on the command line, so it cannot land in a process listing.
        #[arg(long = "api-key-env", value_name = "NAME")]
        api_key_env: Option<String>,
        /// Repository to work in (default: the current directory). Repeat to attach a
        /// second repository; the first given is the primary and a relative path means it.
        #[arg(long, value_name = "PATH")]
        repo: Vec<PathBuf>,
        /// Override the MCP server command (default: this binary serving --repo). Repeat
        /// to give one command per --repo, paired in the order both were written.
        #[arg(long = "mcp-command", value_name = "CMD")]
        mcp_command: Vec<String>,
        /// Directory for the transcript, the Kin trace and the result record
        #[arg(long, value_name = "DIR")]
        out: Option<PathBuf>,
        /// Tool-call budget before the agent is asked for a final answer
        #[arg(long = "max-tool-calls", value_name = "N")]
        max_tool_calls: Option<u32>,
        /// Wall-clock deadline in seconds
        #[arg(long, value_name = "S")]
        deadline: Option<u64>,
        /// The model's context window in tokens (default: what the endpoint reports, else 32768)
        #[arg(long = "context-tokens", value_name = "N")]
        context_tokens: Option<u64>,
        /// Most bytes of one tool result sent to the model (default: an eighth of the window, at most 32768)
        #[arg(long = "max-result-bytes", value_name = "N")]
        max_result_bytes: Option<usize>,
        /// File holding a system prompt that replaces the built-in one
        #[arg(long, value_name = "FILE")]
        system: Option<PathBuf>,
        /// Sampling temperature passed through to the endpoint
        #[arg(long, value_name = "F")]
        temperature: Option<f32>,
        /// Tool surface the MCP server should serve
        #[arg(long = "tool-profile", value_name = "PROFILE")]
        tool_profile: Option<String>,
    },
    /// Check that the model endpoint and the Kin MCP server both answer
    Doctor {
        /// OpenAI-compatible base URL
        #[arg(long = "base-url", value_name = "URL")]
        base_url: String,
        /// Model id to look for in the endpoint's list
        #[arg(long, value_name = "ID")]
        model: Option<String>,
        /// Repository to serve (default: the current directory). Repeat to probe every
        /// repository a run would attach.
        #[arg(long, value_name = "PATH")]
        repo: Vec<PathBuf>,
        /// Override the MCP server command. Repeat to give one command per --repo.
        #[arg(long = "mcp-command", value_name = "CMD")]
        mcp_command: Vec<String>,
        /// Name of an environment variable holding the API key
        #[arg(long = "api-key-env", value_name = "NAME")]
        api_key_env: Option<String>,
        /// Tool surface the MCP server should serve
        #[arg(long = "tool-profile", value_name = "PROFILE")]
        tool_profile: Option<String>,
    },
}

#[derive(Subcommand)]
enum McpAction {
    /// Start the MCP stdio server
    ///
    /// The server binds one Kin repository: the one named by --repo or
    /// KIN_MCP_REPO, otherwise the one containing its working directory,
    /// otherwise whatever the MCP client's workspace roots point at.
    ///
    /// A server that bound a repository of its own keeps serving it and ignores
    /// client workspace roots it cannot resolve to a Kin repository. That is
    /// what makes a container or remote registration work: with
    /// `docker exec -i -w /work/repo <container> /absolute/path/to/kin mcp start`,
    /// the client announces host paths that do not exist inside the container,
    /// and treating them as a workspace change would refuse every call. Roots
    /// that do name a Kin repository this server can see, and does not serve,
    /// are still a real disagreement and are refused rather than answered from
    /// the wrong repository.
    ///
    /// Register the absolute path to the binary, never a bare `kin`.
    /// `docker exec` resolves a bare command against the image's own PATH,
    /// which carries neither `~/.kin/bin` nor an npm user prefix, so a bare
    /// registration dies with `exec: "kin": executable file not found in
    /// $PATH` and blames Docker for a Kin configuration gap. After
    /// `kin setup`, the managed binary is at `~/.kin/bin/kin`; after
    /// `npm install -g @kinlab/kin`, it is at `$(npm prefix -g)/bin/kin`. To
    /// keep the bare command instead, hand `docker exec` the environment it
    /// needs: `docker exec -e PATH=/home/<user>/.kin/bin:$PATH -i -w
    /// /work/repo <container> kin mcp start`.
    ///
    /// Reinstalling as root is not a shortcut past this on an image that gives
    /// every user the same HOME. Root's npm reads that HOME's `.npmrc` and
    /// reinstalls into the user prefix while reporting success, so nothing new
    /// lands on the default PATH. Name the prefix outright when that is what
    /// you want: `npm install -g --prefix /usr/local @kinlab/kin`.
    Start {
        /// Run in global mode, serving every repo in this home's registry
        /// (KIN_REGISTRY_PATH, else <KIN_HOME>/registry.toml, else ~/.kin/registry.toml)
        #[arg(long)]
        global: bool,
        /// Bind this server to a specific Kin repository instead of relying on
        /// the launching process's working directory. Overrides KIN_MCP_REPO.
        /// Use this for a global agent-CLI MCP entry that may launch outside
        /// any Kin repository (e.g. an umbrella workspace root). The pin is
        /// never repointed by the client's workspace roots.
        #[arg(long, value_name = "PATH")]
        repo: Option<PathBuf>,
        /// Tool surface to serve: `agent-default` (the curated agent belt,
        /// and the default), `agent-query` (that belt without the session and
        /// transaction tools, for a client that only queries), `agent-search`
        /// (the measured always-on set plus registry discovery through
        /// `kin_tool_search`; discovery does not enable withheld tools),
        /// `agent-routed` (one tool, `kin`, whose commands reach the agent
        /// belt, writes included, and every other tool through `describe` and
        /// `call`, for a client that sends every tool with every request),
        /// `agent-routed-query` (that one tool without a write path), `full`
        /// (every tool; schema token cost depends on the model), `benchmark`,
        /// or `context-bench`. Overrides KIN_MCP_TOOL_PROFILE.
        #[arg(long = "tool-profile", value_name = "PROFILE")]
        tool_profile: Option<String>,
        /// Never start or revive a daemon from this server: bind only a daemon
        /// that is already running, and answer graph tool calls with an honest
        /// "no daemon is running" error otherwise. This is the probe mode for
        /// watchdogs and boot-time checks (equivalent to KIN_NO_DAEMON=1): the
        /// MCP handshake and tool list are served in full, and nothing heavy
        /// is ever spawned by the check itself.
        #[arg(long)]
        no_spawn: bool,
    },
}

#[derive(Subcommand)]
enum AuthAction {
    /// Log into KinLab and store a CLI credential
    Login {
        /// Override the KinLab base URL
        #[arg(long)]
        base_url: Option<String>,
        /// Print a browser URL and exchange a one-time code manually
        #[arg(long, default_value_t = false)]
        no_browser: bool,
        /// Which identity provider to sign in with. Defaults to google, which
        /// is where every login went before there was a choice.
        #[arg(long, value_enum, default_value_t = commands::auth::AuthProvider::Google)]
        provider: commands::auth::AuthProvider,
    },
    /// Log out and remove the stored KinLab credential
    Logout {
        /// Override the KinLab base URL
        #[arg(long)]
        base_url: Option<String>,
    },
    /// Show the authenticated KinLab user
    Whoami {
        /// Override the KinLab base URL
        #[arg(long)]
        base_url: Option<String>,
    },
    /// Show whether a KinLab credential is stored
    Status {
        /// Override the KinLab base URL
        #[arg(long)]
        base_url: Option<String>,
    },
}

#[derive(Subcommand)]
enum WorkAction {
    /// Create a new work item
    Create {
        /// Work kind: feature, task, issue, debt, todo, investigation
        #[arg(short, long)]
        kind: String,
        /// Work item title
        #[arg(short, long)]
        title: String,
        /// Optional description
        #[arg(short, long)]
        description: Option<String>,
        /// Scope to link (entity:<uuid>, contract:<uuid>, artifact:<path>, change:<id>, or an entity UUID)
        #[arg(short, long)]
        scope: Option<String>,
        /// Priority: critical, high, medium, low, none
        #[arg(short, long)]
        priority: Option<String>,
    },
    /// List work items
    List {
        /// Filter by status
        #[arg(short, long)]
        status: Option<String>,
        /// Filter by kind
        #[arg(short, long)]
        kind: Option<String>,
        /// Filter by scope (entity:<uuid>, contract:<uuid>, artifact:<path>, change:<id>, or an entity UUID)
        #[arg(long)]
        scope: Option<String>,
    },
    /// Show work item details
    Show {
        /// Work item ID
        work_id: String,
    },
    /// Link a work item to a scope
    Link {
        /// Work item ID
        work_id: String,
        /// Scope to link
        scope: String,
    },
    /// Link a parent work item to a child work item
    Decompose {
        /// Parent work item ID
        parent_work_id: String,
        /// Child work item ID
        child_work_id: String,
    },
    /// Mark one work item as blocked by another
    Block {
        /// Blocked work item ID
        blocked_work_id: String,
        /// Blocker work item ID
        blocker_work_id: String,
    },
    /// Link semantic scopes that implement a work item
    Implement {
        /// Work item ID
        work_id: String,
        /// Implementing scope
        scope: String,
    },
    /// Update a work item status
    Status {
        /// Work item ID
        work_id: String,
        /// New status: proposed, planned, in_progress, blocked, done, verified, archived
        status: String,
    },
    /// Close a work item
    Close {
        /// Work item ID
        work_id: String,
    },
    /// Verify test coverage for a work item's implementing entities
    Verify {
        /// Work item ID
        work_id: String,
    },
}

#[derive(Subcommand)]
enum NoteAction {
    /// Add an annotation to a semantic scope or work item
    Add {
        /// Target to annotate (entity:<uuid>, contract:<uuid>, artifact:<path>, change:<id>, work:<uuid>, or an entity UUID)
        target: String,
        /// Annotation kind: comment, warning, instruction, reasoning
        #[arg(short, long)]
        kind: String,
        /// Annotation body
        #[arg(short, long)]
        body: String,
    },
    /// List annotations for a semantic scope or work item
    List {
        /// Target to query (entity:<uuid>, contract:<uuid>, artifact:<path>, change:<id>, work:<uuid>, or an entity UUID)
        target: String,
    },
    /// Show stale annotations
    Stale,
}

#[derive(Subcommand)]
enum TodoAction {
    /// Import inline TODOs from source files
    Import {
        /// Path to scan (defaults to working directory)
        path: Option<String>,
    },
}

#[derive(Subcommand)]
enum VerifyAction {
    /// Check coverage for a specific entity
    Entity {
        /// Entity name or ID
        entity: String,
    },
    /// Plan a targeted proof set from an entity and its downstream impact
    Plan {
        /// Entity name or ID
        entity: String,
        /// Dependent traversal depth used to widen the proof set
        #[arg(long, default_value_t = 2)]
        depth: u32,
    },
    /// Plan a targeted proof set for a semantic change or the current HEAD
    Change {
        /// Semantic change ID (defaults to current branch head)
        change_id: Option<String>,
        /// Dependent traversal depth used to widen the proof set
        #[arg(long, default_value_t = 2)]
        depth: u32,
    },
    /// Show repository-wide coverage summary
    Summary,
    /// Show only entities missing test coverage
    Missing,
    /// Execute tests for an entity and record a VerificationRun
    Run {
        /// Entity name or ID
        entity: String,
        /// Test runner: cargo, jest, pytest, go, junit, or custom command
        #[arg(long, default_value = "cargo")]
        runner: String,
        /// Dependent traversal depth used to widen the proof set
        #[arg(long, default_value_t = 2)]
        depth: u32,
    },
}

#[derive(Subcommand)]
enum TelemetryAction {
    /// Show consent status and spool statistics
    Status,
    /// Record consent to local telemetry collection
    Consent,
    /// Revoke telemetry consent
    Revoke,
    /// Delete all spooled telemetry data
    Purge,
}

#[derive(Subcommand)]
enum ApprovalsAction {
    /// Show approvals for a change
    Show {
        /// Change ID
        change_id: String,
    },
    /// List all actors and delegations
    List,
}

#[derive(Subcommand)]
enum SetupAction {
    /// Show what's installed
    Status {
        /// Emit the machine-readable health report as JSON
        #[arg(long, default_value_t = false)]
        json: bool,
        /// Print every check, including the ones that pass or do not apply
        ///
        /// On a terminal `kin setup status` prints only the checks that need
        /// something, and one line for the rest. A pipe, CI and this flag keep
        /// the full table.
        #[arg(long, default_value_t = false, conflicts_with = "json")]
        verbose: bool,
    },
    /// Quick health check
    Doctor {
        /// Apply safe automatic repairs (shell hook, MCP configs, config dirs)
        #[arg(long, default_value_t = false)]
        fix: bool,
        /// With --fix: also remove every other copy of Kin that has its own
        /// uninstall
        ///
        /// Runs each copy's own channel (npm, cargo, Homebrew), or deletes a
        /// standalone release file. Never removes a development build or
        /// anything in the managed Kin home. `--fix` alone never removes a
        /// copy, because `kin update` runs it unattended.
        #[arg(
            long = "remove-other-copies",
            requires = "fix",
            default_value_t = false
        )]
        remove_other_copies: bool,
        /// Emit the machine-readable health report as JSON
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// List the AI clients Kin can connect, and which are installed and
    /// connected
    ///
    /// The last column is the id `kin setup --clients` takes.
    Clients {
        /// Emit the list as JSON
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Show what `kin setup` installed, and whether it is still in place
    Ledger {
        /// Emit the ledger + verification as JSON
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Remove what `kin setup` installed, exactly as it recorded it
    Uninstall {
        /// Remove the complete managed install after ledger cleanup (Windows retains an inert authority sidecar)
        #[arg(long, default_value_t = false)]
        all: bool,
        /// Show what would be removed without changing anything
        #[arg(long, default_value_t = false)]
        dry_run: bool,
        /// Also remove entries modified since install (never done by default)
        #[arg(long, default_value_t = false)]
        force: bool,
        /// Emit the per-artifact outcomes as JSON
        #[arg(long, default_value_t = false)]
        json: bool,
    },
}

/// `kin vfs`: one command for the projection that shows graph truth as files.
///
/// The graph is the authority either way. These verbs decide which of the three
/// views of it this repository gets, and prove the one they engaged rather than
/// announcing it.
#[derive(Subcommand)]
enum VfsAction {
    /// Engage the projection for this repository
    On {
        /// Force a projection mode: shim, nfs, or fuse
        #[arg(long, value_parser = ["shim", "nfs", "fuse"])]
        mode: Option<String>,
    },
    /// Disengage the projection for this repository
    Off,
    /// Report which projection is in force, probed live
    Status {
        /// Emit the probe results as JSON
        #[arg(long, default_value_t = false)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum RegistryAction {
    /// Verify local registry authority without reading its contents
    Authority {
        /// Emit machine-readable JSON
        #[arg(long)]
        json: bool,
        /// Explicitly repair mode bits on structurally safe authority files
        #[arg(long)]
        fix: bool,
        /// Create missing private authority files without replacing existing data
        #[arg(long)]
        initialize: bool,
    },
    /// Show repo daemons registered with the central local supervisor
    Daemons {
        /// Emit machine-readable JSON
        #[arg(long)]
        json: bool,
    },
    /// Remove stale entries (paths that no longer contain .kin/)
    Clean,
}

#[derive(Subcommand)]
enum DaemonAction {
    /// Show the supervisor and every repo worker daemon, with stale-file detection
    Status {
        /// The repository whose own daemon to describe, in either store
        /// layout; the current directory when omitted
        path: Option<std::path::PathBuf>,
        /// Emit machine-readable JSON
        #[arg(long)]
        json: bool,
    },
    /// Gracefully stop the current repo's worker daemon (or every daemon with --all)
    Stop {
        /// Stop every worker daemon under this KIN_HOME, then the supervisor
        ///
        /// The supervisor is machine-wide, so it can hold daemons from other
        /// managed homes. Those are skipped and named rather than stopped, and
        /// the supervisor itself is left running while any of them remain. Use
        /// --machine to stop every daemon on the box regardless of home.
        #[arg(long)]
        all: bool,
        /// Widen --all to every daemon on this machine, whatever KIN_HOME it runs under
        #[arg(long, requires = "all")]
        machine: bool,
        /// Stop only daemons nothing is using, and name the rest
        ///
        /// A daemon with an attached client, a request in flight, a write it
        /// has not flushed, or enrichment or embedding still running is left up
        /// and says which, then exits on its own as soon as that ends. Without
        /// this flag the stop happens now, whatever is attached.
        #[arg(long)]
        when_unused: bool,
        /// Emit machine-readable JSON
        #[arg(long)]
        json: bool,
    },
    /// Ask this repository's daemon for a language-server enrichment sweep
    Sweep {
        /// Return as soon as the sweep is queued, instead of waiting for it
        ///
        /// The daemon runs the sweep either way. Its idle timeout does not
        /// count a running sweep, so it keeps sweeping after this command
        /// returns and publishes what it finds when the sweep ends. Waiting
        /// reports progress and the outcome, for up to 900 seconds.
        #[arg(long, default_value_t = false)]
        no_wait: bool,
        /// Emit machine-readable JSON
        #[arg(long)]
        json: bool,
        /// Print the daemon's answer and a line per file, not one live line
        #[arg(long, default_value_t = false, conflicts_with = "json")]
        verbose: bool,
        /// Turn language-server enrichment on for this repository, then sweep
        ///
        /// For a repository whose enrichment is off, as `kin init --no-enrich`
        /// leaves it. Records that enrichment is on, so every daemon that
        /// serves the repository enriches from now on, stops the daemon
        /// serving it now if it was started with enrichment off, because a
        /// daemon reads the setting when it starts, and then sweeps. When
        /// something else keeps the daemon off, an environment switch or a
        /// graph-only store, it names that and its way out instead.
        #[arg(long, default_value_t = false)]
        enable: bool,
    },
}

#[derive(Subcommand)]
enum SecretAction {
    /// Set an org-level secret (reads value from stdin)
    Set {
        /// Secret name
        name: String,
    },
    /// List org-level secrets
    List,
    /// Delete an org-level secret
    Delete {
        /// Secret name
        name: String,
    },
    /// Set a repo-level secret (reads value from stdin)
    SetRepo {
        /// Secret name
        name: String,
    },
    /// List repo-level secrets
    ListRepo,
}

#[derive(Subcommand)]
enum PipelineAction {
    /// List pipelines for the current repo
    List,
    /// Manually trigger a pipeline
    Run {
        /// Pipeline name
        name: String,
    },
    /// Show logs for a pipeline run
    Logs {
        /// Run ID
        run_id: String,
    },
    /// Cancel a running pipeline
    Cancel {
        /// Run ID
        run_id: String,
    },
}

/// Deliberate, bottom-up cross-repo release orchestration (the in-binary port
/// of the umbrella `bin/kin-release`), plus the per-repo graph release snapshot.
///
/// `plan`/`apply`/`intent` read registry truth + the local sibling manifests and
/// drive the bottom-up front (primitives -> kin-model -> kin-db -> kin -> bench/
/// vfs/lsp) deliberately. They never publish, because publishing stays in CI
/// behind the version gate. `snapshot` is the original `kin release <tag>`
/// graph snapshot.
#[derive(Subcommand)]
enum ReleaseAction {
    /// Read-only bottom-up release plan: which crates need publishing and which
    /// downstream pins lag a published crate.
    Plan {
        /// Skip registry queries; show local versions + pins only.
        #[arg(long)]
        offline: bool,
    },
    /// Propagate a published crate version into downstream Cargo.toml pins
    /// (registry = "kin"). Edits manifests locally; never commits/pushes/publishes.
    Apply {
        /// The registry crate whose pin to bump (e.g. kin-db).
        crate_name: String,
        /// The version to pin (e.g. 0.2.24).
        version: String,
        /// Repos to update (default: every consumer repo).
        repos: Vec<String>,
        /// Do not refresh Cargo.lock with `cargo update --precise` after editing.
        #[arg(long)]
        no_lock: bool,
    },
    /// Release-intent gate for one repo (exit 0 = release intended / nothing to
    /// do, non-zero = staged but out of sync). For `kin`, runs the canonical
    /// scripts/release-intent.mjs gate.
    Intent {
        /// Repo to gate (e.g. kin, kin-db).
        repo: String,
    },
    /// Publish a release tag and the snapshot bound to its exact repository state.
    Snapshot {
        /// Release tag
        tag: String,
        /// Block release if entities lack linked passing tests
        #[arg(long)]
        require_proof: bool,
        /// Require known-human approval for every reachable non-root change
        #[arg(long)]
        require_approval: bool,
        /// Force release even with low coverage
        #[arg(long)]
        force: bool,
    },
}

#[derive(Subcommand)]
enum HostedReleaseAction {
    /// Create a hosted release
    Create {
        /// Release tag
        tag: String,
        /// Release name
        #[arg(long)]
        name: Option<String>,
        /// Release notes
        #[arg(long)]
        notes: Option<String>,
    },
    /// List hosted releases
    List,
    /// Upload an artifact to a release
    Upload {
        /// Release ID
        release_id: String,
        /// File to upload
        file: String,
    },
}

/// Command paths that Kin used to accept, mapped to the surface that replaced
/// them.
///
/// These names stay unparseable: the table exists only to replace clap's
/// "similar subcommand" tip, which ranks by edit distance and so answers
/// `import` with `support` and `impact`. A caller arriving from Git types the
/// retired name first, and a suggestion pointing at an unrelated command costs
/// more than no suggestion at all.
const RETIRED_COMMANDS: &[(&[&str], &str)] = &[
    (
        &["setup", "editor"],
        "`kin setup --intent editor` selects the editor onboarding path; setup intents are options, not subcommands.",
    ),
    (
        &["import"],
        "`kin clone <source>` admits a repository from elsewhere, and `kin init` admits one in place.",
    ),
    (
        &["git", "import"],
        "`kin clone <source>` admits a repository from elsewhere, and `kin init` admits one in place. \
         `kin git export` remains the only Git interoperability direction.",
    ),
    (
        &["git", "sync"],
        "`kin reconcile` republishes workspace changes into graph truth, and `kin git export` \
         publishes an exact Git repository to a new destination.",
    ),
    (
        &["workspace"],
        "Session workspaces are graph-derived and command-scoped: `kin with`, `kin open`, \
         `kin shell`, and `kin exec` each run against one.",
    ),
    (
        &["run"],
        "`kin exec` runs a command in an exact graph-derived session workspace.",
    ),
    (&["gc"], "`kin cache gc` reclaims embedding cache space."),
];

/// Leading command-path tokens of `args`, excluding flags and the values they take.
///
/// A global flag declared with a value consumes the token after it, so that
/// token is a value and never a command a caller typed. Reading it as one names
/// a retired command nobody asked for: `kin --profile-out run frobnicate` writes
/// its profile to a file called `run`. Which flags take a value is read from the
/// parser definition rather than restated, so a new global flag is accounted for
/// where it is declared.
fn typed_command_path(args: &[String]) -> Vec<&str> {
    let cli = Cli::command();
    // Arity is resolved when clap builds the command, so an unbuilt definition
    // answers through the declared action instead. Both spellings carry the
    // same fact, and reading the definition costs no parse.
    let takes_value = |matches: &dyn Fn(&clap::Arg) -> bool| {
        cli.get_arguments().any(|declared| {
            matches(declared)
                && declared.get_num_args().map_or_else(
                    || declared.get_action().takes_values(),
                    |arity| arity.takes_values(),
                )
        })
    };

    let mut typed = Vec::new();
    let mut consumes_next = false;
    for arg in args {
        if consumes_next {
            consumes_next = false;
            continue;
        }
        if let Some(long) = arg.strip_prefix("--") {
            if !long.is_empty() && !long.contains('=') {
                consumes_next = takes_value(&|declared| declared.get_long() == Some(long));
            }
            continue;
        }
        if let Some(shorts) = arg.strip_prefix('-') {
            if let Some(last) = shorts.chars().last() {
                consumes_next = takes_value(&|declared| declared.get_short() == Some(last));
            }
            continue;
        }
        typed.push(arg.as_str());
        if typed.len() == 2 {
            break;
        }
    }
    typed
}

/// Longest retired path matching the command path a caller typed.
///
/// Matching is longest-first so `git import` resolves to the Git interoperability
/// guidance rather than the bare `import` entry. A caller whose tokens do not
/// match any entry falls through to clap's own reporting.
fn retired_command_signpost(args: &[String]) -> Option<(String, &'static str)> {
    let typed = typed_command_path(args);
    RETIRED_COMMANDS
        .iter()
        .filter(|(path, _)| path.len() <= typed.len() && typed[..path.len()] == **path)
        .max_by_key(|(path, _)| path.len())
        .map(|(path, guidance)| (path.join(" "), *guidance))
}

/// Parse the command line, answering a retired command path by name.
///
/// Anything clap reports that is not a retired path, including `--help` and
/// `--version`, is handed back to clap so its exit codes and rendering stay
/// exactly as they were.
/// The version text, split into the lines the mark is set beside.
///
/// `kin_buildinfo::version()` composes `0.7.2 (<sha> <branch> <time>)`, one
/// line and eighty-two columns wide, so on an eighty-column terminal the one
/// thing `kin --version` has to say wrapped. Split, it reads at both widths the
/// walk measured, and every fact the single line carried is still here.
///
/// The empty first element leaves the mark's top row to the mark, which is what
/// sets the version number against the arm rather than above it.
fn version_lines(version: &str) -> Vec<String> {
    let (number, build) = match version.split_once(" (") {
        Some((number, rest)) => (number, rest.trim_end_matches(')')),
        None => (version, ""),
    };
    let mut lines = vec![String::new(), format!("kin {number}")];
    lines.extend(CATEGORY_ROWS.iter().map(|row| (*row).to_string()));
    // The commit sha gets its own line, and the branch and build time the next.
    // Together they are seventy columns on a release build, whose branch reads
    // `detached`, and ninety when the branch name is long, which wraps at
    // eighty. Splitting them is what makes the bound hold for every branch
    // name rather than for the ones a release happens to have.
    match build.split_once(' ') {
        Some((commit, rest)) => {
            lines.push(commit.to_string());
            lines.push(rest.to_string());
        }
        None => lines.push(build.to_string()),
    }
    lines
}

/// The canon definition as one line, the line `--help` opens with.
///
/// Only tests read it. They hold the first line of [`ORIENTATION`] and the
/// joined [`CATEGORY_ROWS`] to it, so help and version cannot drift apart.
#[cfg(test)]
const CATEGORY_LINE: &str =
    "A graph-native code repository for AI agents and the people who build with them.";

/// The canon definition as the `--version` block sets it, broken where the
/// phrase breaks.
///
/// The definition is eighty columns on its own, and beside the mark it runs
/// past eighty and wraps. Two rows keep every word of it inside the bound the
/// version block holds for every other line.
const CATEGORY_ROWS: [&str; 2] = [
    "A graph-native code repository for AI agents",
    "and the people who build with them.",
];

/// What `kin --version` prints.
///
/// A terminal gets the mark and the split lines. Anything else gets the exact
/// single line it got before, because a redirected `kin --version` is being
/// read by something: the README tells a reader to confirm an install with it
/// on every install path, and the Homebrew tap, the npm launcher, and every
/// install script parse it. Four rows of art in that stream would break them
/// all, so the decision is made once, here, by whether stdout is a terminal.
fn rendered_version(version: &str, style: Option<kin_cli::mark::MarkStyle>) -> String {
    let columns = console::Term::stdout()
        .size_checked()
        .map(|(_, columns)| usize::from(columns));
    rendered_version_at(version, style, columns)
}

/// [`rendered_version`] at a known terminal width. A terminal too narrow for
/// the definition beside the mark gets it rewrapped at words under the same
/// column, rather than broken mid-word by the terminal.
fn rendered_version_at(
    version: &str,
    style: Option<kin_cli::mark::MarkStyle>,
    columns: Option<usize>,
) -> String {
    match style {
        Some(style) => {
            let mut lines = version_lines(version);
            let room = columns.map(|columns| columns.saturating_sub(style.text_column() + 1));
            if let Some(room) =
                room.filter(|room| CATEGORY_ROWS.iter().any(|row| row.len() > *room))
            {
                let definition = CATEGORY_ROWS.join(" ");
                let rewrapped: Vec<String> = kin_cli::screen::hang_wrap(&definition, room)
                    .lines()
                    .map(str::to_string)
                    .collect();
                lines.splice(2..2 + CATEGORY_ROWS.len(), rewrapped);
            }
            kin_cli::mark::beside(style, &lines.iter().map(String::as_str).collect::<Vec<_>>())
                .join("\n")
        }
        None => format!("kin {version}"),
    }
}

/// `--history` takes one of the three modes, by name.
fn parse_history_mode(value: &str) -> Result<kin_core::history_state::HistoryMode, String> {
    kin_core::history_state::HistoryMode::parse(value)
        .ok_or_else(|| format!("`{value}` is not a history mode; use background, none or full"))
}

fn parse_cli_or_report_retired_command() -> Cli {
    match Cli::try_parse() {
        Ok(cli) => cli,
        Err(err) => {
            // clap reports `--version` as an error of its own kind and would
            // print the bare line itself. Taken here so the mark reaches the
            // one surface a reader hits before anything else, while clap keeps
            // deciding what counts as asking for the version.
            if err.kind() == clap::error::ErrorKind::DisplayVersion {
                println!(
                    "{}",
                    rendered_version(
                        kin_buildinfo::version(),
                        kin_cli::mark::MarkStyle::for_stdout()
                    )
                );
                std::process::exit(0);
            }
            // A bare `kin` prints the command index, to stderr, and is the
            // first screen a new user reaches without asking for help. The
            // logo goes above it on stdout, and only when that index is going
            // to a person too. `kin --help` is a `DisplayHelp` and never
            // reaches here.
            if err.kind() == clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
                && std::env::args_os().len() == 1
            {
                kin_cli::banner::print_once(std::io::stderr().is_terminal());
            }
            if matches!(
                err.kind(),
                clap::error::ErrorKind::DisplayHelp
                    | clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
            ) {
                if let Some(help) = help_fitted_to_terminal(&err) {
                    if err.use_stderr() {
                        eprint!("{help}");
                    } else {
                        print!("{help}");
                    }
                    std::process::exit(err.exit_code());
                }
            }
            if err.kind() == clap::error::ErrorKind::InvalidSubcommand {
                let args: Vec<String> = std::env::args().skip(1).collect();
                if let Some((path, guidance)) = retired_command_signpost(&args) {
                    eprintln!("error: `kin {path}` was retired.");
                    eprintln!();
                    eprintln!("  {guidance}");
                    eprintln!();
                    eprintln!(
                        "Run `kin --help` for the current commands, or `kin capabilities` for \
                         repository-v6 readiness."
                    );
                    std::process::exit(2);
                }
            }
            err.exit()
        }
    }
}

fn is_top_level_display_flag() -> bool {
    let mut args = std::env::args_os().skip(1);
    let Some(flag) = args.next() else {
        return false;
    };
    if args.next().is_some() {
        return false;
    }
    matches!(flag.to_str(), Some("--help" | "-h" | "--version" | "-V"))
}

/// The process entry. The outcome of [`run`] becomes an exit status in one
/// place, so a refusal is reported the same way whether or not anyone still
/// reads stderr, and the `println!` and `eprintln!` this file imports from
/// `kin_cli` are the ones that tolerate a reader going away, so nothing here
/// ends early because a pipe closed. Two cases, told apart by the status. A
/// command that ran to its end exits 0, or its own error status, whatever
/// happened to its streams on the way, because its work is done and the
/// status reports the work. A command that stopped at a write outside those
/// macros, an `io::Error` of kind `BrokenPipe` that came back through `?`,
/// exits 141: its work after that write never ran, and 141 is a status nobody
/// can read as success of work that never happened. `kin_cli::broken_pipe`
/// decides the status; this is the process boundary, so it is where the exit
/// happens.
fn main() {
    // First, while the process is still single-threaded: nothing Kin creates
    // may be group or world writable, whatever umask the login handed it.
    // A 0002 umask (USERGROUPS_ENAB) otherwise made ~/.kin group writable and
    // the registry refused it. See `kin_core::umask`.
    kin_core::umask::withhold_shared_write();
    if let Err(error) = run() {
        std::process::exit(kin_cli::broken_pipe::exit_status(&error));
    }
}

fn run() -> Result<()> {
    if is_top_level_display_flag() {
        let _ = parse_cli_or_report_retired_command();
    }
    let startup_args: Vec<String> = std::env::args_os()
        .skip(1)
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    if typed_command_path(&startup_args).first() == Some(&"runtime") {
        let cli = parse_cli_or_report_retired_command();
        if let Command::Runtime {
            action:
                RuntimeAction::Status {
                    json,
                    development,
                    binary,
                },
        } = cli.command
        {
            let code = commands::runtime::run(
                json,
                &commands::runtime::StatusOptions {
                    development,
                    binary,
                },
            )?;
            if code != 0 {
                std::process::exit(code);
            }
            return Ok(());
        }
    }
    if typed_command_path(&startup_args).first() == Some(&"bench-meta")
        && startup_args
            .iter()
            .any(|arg| arg == "--runtime-identity-only")
    {
        let cli = parse_cli_or_report_retired_command();
        if let Command::BenchMeta {
            json: true,
            prepared_state: false,
            runtime_identity_only: true,
        } = cli.command
        {
            return commands::runtime::run_identity_probe();
        }
    }
    if typed_command_path(&startup_args).as_slice() == ["registry", "authority"] {
        let cli = parse_cli_or_report_retired_command();
        if let Command::Registry {
            action:
                Some(RegistryAction::Authority {
                    json,
                    fix: false,
                    initialize: false,
                }),
        } = cli.command
        {
            return tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?
                .block_on(commands::registry::authority(json, false, false));
        }
    }
    let runtime_mode = commands::runtime::prepare_environment()?;
    // Raise this process's open-file soft limit before anything opens a file.
    // macOS ships that limit at 256, which is below what one admission needs,
    // so on a stock Mac `kin init` died on the first real repository a person
    // tried. Silent either way, and it never lowers a limit an operator raised.
    kin_core::file_limit::raise_open_file_limit();
    // Before anything can reach the embedder: a machine where nobody asked for
    // the embedding model, or whose `kin setup` declined it, never fetches it.
    kin_cli::embed_model::install_model_fetch_gate();
    // Select this process's resource profile before anything reads it: the GPU
    // kernel plan and the Metal submission depth are each resolved once per
    // process, and mutating the environment is only safe while the process is
    // still single-threaded. An operator's explicit KIN_RESOURCE_PROFILE wins.
    kin_cli::resource_profile::apply_product_default();
    // ...then let the machine-wide profile `kin setup` recorded take over from
    // that default. Setup is where a person adjusts the profile for their own
    // hardware, and a choice nothing reads is not a choice. kin-daemon applies
    // the SAME one at the same point, so the two environments agree on
    // KIN_RESOURCE_PROFILE by construction and no behavior-env divergence is
    // reported for using the feature.
    kin_cli::resource_profile::apply_host_profile_at();
    // A multi-repository UI must not export its launch directory's model,
    // retrieval or resource settings into every repository daemon it opens.
    // Consult the same parser metadata used for leading command paths, so a
    // global option value named `ui` is not mistaken for the command.
    let workspace_ui_requested = startup_args.iter().any(|arg| arg == "ui")
        && typed_command_path(&startup_args).first() == Some(&"ui");
    // ...then let the repository's recorded profile take over from that default,
    // exactly as kin-daemon does, so the two agree by construction. Without this
    // the daemon runs under the repository's profile and this process does not,
    // and every command in such a repository reports a behavior-env divergence
    // whose remedy cannot clear it.
    if !is_top_level_display_flag() && !workspace_ui_requested {
        if let Ok(cwd) = std::env::current_dir() {
            kin_cli::resource_profile::apply_repository_profile_at(&cwd);
            kin_cli::retrieval_arm::apply_repository_retrieval_at(&cwd);
        }
    }
    // Put Kin's own tool directories on PATH while this process is still
    // single-threaded, so a language server Kin provisioned is reachable by the
    // `which` lookup enrichment discovery performs, and by the daemon this
    // command may spawn, which inherits this environment. Appended rather than
    // prepended: an operator's own toolchain outranks Kin's fallback copy.
    kin_core::tool_prefix::augment_path_with_managed_tools();
    if kin_migrate::run_migration_process_host_if_requested()? {
        return Ok(());
    }
    kin_buildinfo::retain_update_build_identity(&KIN_UPDATE_BUILD_IDENTITY);
    let cli = parse_cli_or_report_retired_command();
    let setup_read_only = matches!(
        &cli.command,
        Command::Setup {
            action: None,
            check: true,
            ..
        } | Command::Setup {
            action: Some(SetupAction::Status { .. }),
            ..
        } | Command::Setup {
            action: Some(SetupAction::Clients { .. }),
            ..
        } | Command::Setup {
            action: Some(SetupAction::Ledger { .. }),
            ..
        } | Command::Setup {
            action: Some(SetupAction::Doctor { fix: false, .. }),
            install_language_servers: false,
            ..
        } | Command::Setup {
            action: Some(SetupAction::Uninstall { dry_run: true, .. }),
            ..
        }
    );
    let update_read_only = matches!(
        &cli.command,
        Command::Update {
            check_only: true,
            cleanup_windows_worker: None,
            ..
        } | Command::Update {
            apply: true,
            dry_run: true,
            cleanup_windows_worker: None,
            ..
        }
    );
    if runtime_mode == kin_core::runtime_identity::RuntimeMode::Development
        && matches!(&cli.command, Command::Update { .. })
        && !update_read_only
    {
        anyhow::bail!(
            "development mode cannot mutate the managed installation; run kin update from the admitted managed runtime"
        );
    }
    if runtime_mode == kin_core::runtime_identity::RuntimeMode::Development
        && ((matches!(&cli.command, Command::Setup { .. }) && !setup_read_only)
            || matches!(
                &cli.command,
                Command::Doctor { fix: true, .. }
                    | Command::Doctor {
                        reclaim_staging: true,
                        ..
                    }
            ))
    {
        anyhow::bail!("development mode cannot run setup or doctor repairs against global clients and shell configuration; use the admitted managed runtime");
    }
    let runtime_exempt = setup_read_only
        || matches!(
            &cli.command,
            Command::Runtime { .. }
                | Command::BenchMeta { .. }
                | Command::Registry {
                    action: Some(RegistryAction::Authority { .. }),
                }
                | Command::Daemon {
                    action: DaemonAction::Status { .. } | DaemonAction::Stop { .. },
                }
                | Command::Update { .. }
                | Command::Capabilities { .. }
                | Command::Describe { .. }
                | Command::Languages { .. }
                | Command::Completions { .. }
                | Command::SemanticOnlyGuard
                | Command::Doctor {
                    fix: false,
                    heal: false,
                    reclaim_staging: false,
                    ..
                }
        );
    if let Some(code) = commands::runtime::route_startup(runtime_mode, runtime_exempt)? {
        std::process::exit(code);
    }
    if !is_top_level_display_flag() && !matches!(&cli.command, Command::Ui { .. }) {
        let model_repository = match &cli.command {
            Command::Mcp {
                action: McpAction::Start { repo, .. },
            } => repo
                .clone()
                .or_else(|| std::env::var_os("KIN_MCP_REPO").map(PathBuf::from))
                .unwrap_or(std::env::current_dir()?),
            _ => std::env::current_dir()?,
        };
        kin_core::models::apply_repository_models_at(&model_repository)?;
        // Asking for an embedding pass opts this repository into the built-in
        // slot when no provider was selected. An engine-free build keeps none.
        #[cfg(feature = "embeddings")]
        if matches!(cli.command, Command::Embed { .. }) && !kin_db::embed::embedding_slot_enabled()
        {
            if let Some(layout) = kin_core::KinLayout::discover(&model_repository) {
                let path = layout.config_path();
                let mut config = kin_core::KinConfig::load(&path)?;
                config.models.0.insert(
                    kin_core::models::ModelSlot::Embeddings,
                    kin_core::models::ModelSlotConfig {
                        provider: kin_core::models::ModelProvider::BuiltIn,
                        ..Default::default()
                    },
                );
                config.save(&path)?;
                // SAFETY: no runtime or worker threads exist at this point.
                unsafe {
                    std::env::set_var("KIN_MODEL_EMBEDDINGS_PROVIDER", "built-in");
                }
                kin_cli::embed_model::note_built_in_selected_by_this_command();
            }
        }
    }
    // The semantic-only guard runs once per tool call of a launched assistant
    // and adjudicates a payload on stdin. It touches no repository, so it
    // returns before the tracing subscriber, the environment audit, the pending
    // MCP repair retry, and the tokio runtime: none of them would change its
    // answer, and all of them would be charged to the subject's tool latency.
    if matches!(cli.command, Command::SemanticOnlyGuard) {
        return commands::assistant_adapter::run_semantic_only_guard();
    }
    let command_name = current_command_name();
    let cwd = std::env::current_dir()?.display().to_string();
    let profile_out = cli
        .profile_out
        .clone()
        .or_else(|| std::env::var_os("KIN_PROFILE_OUT").map(PathBuf::from));
    let profile_summary = cli.profile_summary || env_flag("KIN_PROFILE_SUMMARY");
    let profile_session = profile_out
        .clone()
        .map(|path| kin_cli::profile::ProfileSession::new(command_name.clone(), cwd.clone(), path));

    let mut directives = installed_directives(&command_name);
    // The short `kin status` page is the whole answer on a terminal. The
    // missing-.kinignore advice is for whoever maintains the repository's
    // admission rules, so it stays in the daemon log and in the full record
    // (`kin status --verbose`) rather than opening a person's status page.
    if std::env::var_os("RUST_LOG").is_none()
        && matches!(
            cli.command,
            Command::Status {
                json: false,
                verbose: false,
                ..
            }
        )
        && kin_cli::screen::short_form(false)
    {
        directives.push_str(",kin_index::repository=error");
    }
    tracing_stack(
        &directives,
        profile_session.clone(),
        std::io::stderr,
        // The fmt layer does not sniff for a terminal, so without this it
        // writes colour escapes into a redirected log or a captured transcript.
        std::io::stderr().is_terminal(),
        // An operator who set RUST_LOG asked for the records; nobody else did.
        std::env::var_os("RUST_LOG").is_none(),
    )
    .init();

    // Validate the KIN_* environment surface once logging is live. Unknown names
    // (likely typos) and out-of-range values are surfaced loudly instead of
    // silently no-op'ing; an invalid correctness-relevant value refuses to run
    // rather than mis-behaving. Governed by KIN_ENV_VALIDATION (off/warn/strict).
    if let Err(err) = kin_core::env_registry::enforce_startup_env() {
        eprintln!("kin: {err}");
        std::process::exit(2);
    }

    // A durable MCP marker is an active repair obligation. Ordinary commands
    // retry it; the update command retries while holding the install lock.
    if !runtime_exempt && !matches!(&cli.command, Command::Update { .. }) {
        match commands::update::retry_pending_mcp_repair_from_managed_process() {
            Ok(_) => {}
            Err(error) => eprintln!("kin: MCP repair remains pending: {error:#}"),
        }
    }

    let root_span = tracing::info_span!(
        "kin.command",
        command = %command_name,
        cwd = %cwd
    );

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    let result = runtime.block_on(
        (async move {
            match cli.command {
                Command::Runtime { .. } => {
                    unreachable!("runtime status returned before repository startup")
                }
                Command::Capabilities { json, verbose } => {
                    commands::capabilities::run(json, verbose)
                }
                Command::Init {
                    path,
                    json,
                    no_enrich,
                    defer_enrich,
                    adopt_repository_id,
                    store,
                    verbose,
                    history,
                } => {
                    let code = commands::init::run(
                        path,
                        json,
                        commands::init::InitEnrichment::from_flags(no_enrich, defer_enrich),
                        adopt_repository_id,
                        store,
                        verbose,
                        history,
                    )
                    .await?;
                    // A conversion whose store exists but whose enrichment a
                    // killed daemon left unattested reports that through the
                    // exit code, so a scripted or agent-driven setup can branch
                    // on it without parsing the summary. It is not an error:
                    // the store is durable and the summary above is true.
                    if code != 0 {
                        std::process::exit(code);
                    }
                    Ok(())
                }
                Command::Status {
                    json,
                    wait_quiesce,
                    verbose,
                } => {
                    // A working copy nothing admitted is an answer, not an
                    // error, and it travels in the exit code the way a parked
                    // merge and an unrouted `kin path` already do. The report is
                    // printed either way; the code says whether any of it
                    // describes the files on disk.
                    let code = commands::status::run(
                        json,
                        verbose,
                        std::time::Duration::from_secs(wait_quiesce),
                    )
                    .await?;
                    if code != 0 {
                        std::process::exit(code);
                    }
                    Ok(())
                }
                Command::Resources { action } => match action {
                    ResourcesAction::Set {
                        profile,
                        embed_batch_size,
                        clear,
                    } => commands::resources::set(profile, embed_batch_size, clear),
                    ResourcesAction::Inspect { json, profile } => {
                        commands::resources::run(json, profile).await
                    }
                },
                Command::Commit {
                    message,
                    quiet,
                    amend,
                    entities,
                } => {
                    commands::capabilities::require_ready("commit")?;
                    commands::commit::run(message, quiet, amend, entities).await
                }
                Command::Log { count, json } => commands::log::run(count, json).await,
                Command::Branch { action } => match action {
                    BranchAction::List { json } => commands::branch::list(json).await,
                    BranchAction::Create { name, ref_hex } => {
                        commands::branch::create(commands::branch::parse_branch_ref(
                            name.as_deref(),
                            ref_hex.as_deref(),
                        )?)
                        .await
                    }
                    BranchAction::Delete { name, ref_hex } => {
                        commands::branch::delete(commands::branch::parse_branch_ref(
                            name.as_deref(),
                            ref_hex.as_deref(),
                        )?)
                        .await
                    }
                    BranchAction::Switch {
                        name: Some(target),
                        detach: true,
                        ..
                    } => commands::branch::switch_detached(target).await,
                    BranchAction::Switch { name, ref_hex, .. } => {
                        commands::branch::switch(commands::branch::parse_branch_ref(
                            name.as_deref(),
                            ref_hex.as_deref(),
                        )?)
                        .await
                    }
                },
                Command::Diff {
                    base,
                    head,
                    json,
                    full_bodies,
                } => {
                    // Same code and the same reason as `kin status` above, and
                    // only a workspace endpoint can produce it.
                    let code = commands::diff::run(base, head, json, full_bodies).await?;
                    if code != 0 {
                        std::process::exit(code);
                    }
                    Ok(())
                }
                Command::Eject { yes } => commands::eject::run(yes).await,
                Command::Impact {
                    entity,
                    depth,
                    file,
                    kind,
                    signature,
                    json,
                    dispatch,
                } => {
                    commands::impact::run(entity, depth, file, kind, signature, json, dispatch)
                        .await
                }
                Command::Context {
                    entities,
                    question,
                    budget,
                    assistant,
                    max_focals,
                    json,
                } => {
                    commands::context::run(entities, question, budget, assistant, max_focals, json)
                        .await
                }
                Command::Source(args) => args.print().await,
                // The routed tool's answer is the output either way, and an
                // error answer is the exit status, as `kin path` reports a
                // missing route.
                Command::Ask {
                    hook: Some(client), ..
                } => {
                    let mut raw = String::new();
                    let _ = std::io::Read::read_to_string(&mut std::io::stdin(), &mut raw);
                    if let Some(printed) = commands::ask::hook(&client, &raw).await {
                        println!("{printed}");
                    }
                    Ok(())
                }
                Command::Ask {
                    question,
                    tests,
                    no_tests,
                    scope,
                    budget,
                    json,
                    hook: None,
                } => {
                    let options = commands::ask::AskOptions {
                        include_tests: if tests {
                            Some(true)
                        } else if no_tests {
                            Some(false)
                        } else {
                            None
                        },
                        scope,
                        budget,
                        json,
                    };
                    let answer = commands::ask::run(&question.join(" "), &options).await?;
                    println!("{}", commands::ask::printed(&answer, json));
                    if answer.is_error {
                        std::process::exit(1);
                    }
                    Ok(())
                }
                Command::Describe { command } => {
                    let answer = commands::routed_words::describe(command.as_deref());
                    println!("{}", answer.text);
                    if answer.is_error {
                        std::process::exit(1);
                    }
                    Ok(())
                }
                Command::Call { tool, arguments } => {
                    let answer = commands::routed_words::call(&tool, arguments.as_deref()).await?;
                    println!("{}", answer.text);
                    if answer.is_error {
                        std::process::exit(1);
                    }
                    Ok(())
                }
                Command::ContextbenchLocate {
                    task_file,
                    json,
                    debug,
                } => commands::contextbench_locate::run(task_file, json, debug).await,
                Command::Path {
                    from,
                    to,
                    from_file,
                    to_file,
                    max_depth,
                    limit,
                    direction,
                    include_type_edges,
                    json,
                    compact,
                } => {
                    // No route is an answer, not an error, and it travels in
                    // the exit code the way a parked merge does.
                    let code = commands::path::run(
                        from,
                        to,
                        from_file,
                        to_file,
                        max_depth,
                        limit,
                        direction,
                        include_type_edges,
                        json,
                        compact,
                    )
                    .await?;
                    if code != 0 {
                        std::process::exit(code);
                    }
                    Ok(())
                }
                Command::Places { entity_ids, json } => {
                    commands::places::run(entity_ids, json).await
                }
                Command::Trace {
                    entity,
                    file,
                    kind,
                    json,
                    compact,
                    show_body: _,
                    limit,
                    budget,
                    assistant,
                    max_lines,
                    nearby,
                    transitive,
                } => {
                    if json {
                        commands::trace::run_json(
                            entity,
                            compact,
                            budget,
                            assistant,
                            max_lines,
                            limit.unwrap_or(nearby),
                            transitive,
                            file,
                            kind,
                        )
                        .await
                    } else {
                        commands::trace::run(
                            entity,
                            compact,
                            budget,
                            assistant,
                            max_lines,
                            limit.unwrap_or(nearby),
                            transitive,
                            file,
                            kind,
                        )
                        .await
                    }
                }
                Command::Search {
                    pattern,
                    json,
                    kind,
                    language,
                    show_body,
                    limit,
                    semantic,
                } => {
                    if semantic {
                        if json {
                            commands::search::run_semantic_json(
                                pattern,
                                kind,
                                language,
                                limit.unwrap_or(10),
                            )
                            .await
                        } else {
                            commands::search::run_semantic(
                                pattern,
                                kind,
                                language,
                                limit.unwrap_or(10),
                            )
                            .await
                        }
                    } else {
                        if json {
                            commands::search::run_json(pattern, kind, language, show_body, limit)
                                .await
                        } else {
                            commands::search::run(pattern, kind, language, show_body, limit).await
                        }
                    }
                }
                Command::Scope {
                    ref_string,
                    clear,
                    show,
                    session,
                } => {
                    commands::scope::run(ref_string.as_deref(), clear, show, session.as_deref())
                        .await
                }
                Command::Locate {
                    text,
                    query,
                    file,
                    stdin,
                    json,
                    explain,
                    diagnose,
                    gold,
                    max_files,
                    reference,
                    snippets,
                    no_snippets,
                    next,
                    cursor,
                    page_size,
                    include_tests,
                    surface,
                } => {
                    // Refuse an unknown shape rather than falling back to the
                    // default: a misspelled --surface that silently emitted the
                    // full payload would look exactly like a compact surface
                    // that stopped compacting.
                    let surface = commands::locate_compact::LocateSurface::parse(&surface)
                        .ok_or_else(|| {
                            anyhow::anyhow!(
                                "unknown --surface '{surface}'; expected 'full' or 'compact'"
                            )
                        })?;
                    // Inline snippets default ON for the structured/agent `--json`
                    // surface (so an agent gets code on the first locate);
                    // --diagnose stays lean unless --snippets is explicit;
                    // --no-snippets always wins.
                    let want_snippets = !no_snippets && (snippets || (json && !diagnose));
                    // --diagnose implies --json --explain
                    let json = json || diagnose;
                    let explain = explain || diagnose;
                    let max_files_explicit = max_files.is_some();
                    let max_files_val = max_files.unwrap_or(10);
                    // Resolve the paging cursor: --next reads the persisted cursor
                    // from the prior page; --cursor takes an explicit token.
                    let paging_cursor = if next {
                        Some(commands::locate_cursor::read_persisted_locate_cursor()?)
                    } else {
                        cursor
                    };
                    let paging = commands::locate::LocatePaging {
                        cursor: paging_cursor,
                        page_size,
                        snippet_override: if snippets {
                            Some(true)
                        } else if no_snippets {
                            Some(false)
                        } else {
                            None
                        },
                    };
                    // On a paging request the query text is carried by the cursor
                    // (the daemon holds the cached ranking), so it is optional.
                    let paging_active = paging.cursor.is_some();
                    let problem_text = if stdin {
                        let mut buf = String::new();
                        std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)?;
                        buf
                    } else if let Some(path) = file {
                        std::fs::read_to_string(&path)?
                    } else if let Some(t) = text {
                        t
                    } else if paging_active {
                        // --next / --cursor without inline text: the daemon pages
                        // its cached ranking; text is only a cache-miss fallback.
                        String::new()
                    } else {
                        anyhow::bail!("provide problem text, --file, or --stdin");
                    };
                    if diagnose && surface == commands::locate_compact::LocateSurface::Compact {
                        // Refused rather than silently overridden. --diagnose
                        // exists to show every stage, seed and score, and the
                        // compact surface drops all of them; taking the flag and
                        // ignoring it would print a diagnostic missing the
                        // diagnosis.
                        anyhow::bail!("--diagnose needs the full payload; drop --surface compact");
                    }
                    if diagnose {
                        // Diagnostic mode: capture result, print JSON, then
                        // print gold file comparison to stderr.
                        let result = commands::locate::capture(
                            &problem_text,
                            query.clone(),
                            true, // always explain in diagnose mode
                            max_files_val,
                            max_files_explicit,
                            reference,
                            want_snippets,
                            // Diagnose prints the structured payload, so it wants
                            // the graph-native entity ranking even though it stays
                            // lean on bodies unless --snippets is explicit.
                            true,
                            paging,
                            commands::locate::LocateScope::with_tests(include_tests),
                        )
                        .await?;

                        // Print full JSON to stdout
                        println!("{}", serde_json::to_string_pretty(&result)?);

                        // Print diagnostic comparison to stderr
                        let retrieved: Vec<&str> =
                            result.files.iter().map(|f| f.path.as_str()).collect();
                        let gold_set: std::collections::HashSet<&str> =
                            gold.iter().map(|s| s.as_str()).collect();
                        let overlap: Vec<&&str> = retrieved
                            .iter()
                            .filter(|f| gold_set.contains(**f))
                            .collect();
                        let precision = if retrieved.is_empty() {
                            0.0
                        } else {
                            overlap.len() as f64 / retrieved.len() as f64
                        };
                        let recall = if gold.is_empty() {
                            0.0
                        } else {
                            overlap.len() as f64 / gold.len() as f64
                        };
                        let f1 = if precision + recall > 0.0 {
                            2.0 * precision * recall / (precision + recall)
                        } else {
                            0.0
                        };

                        eprintln!();
                        eprintln!("━━━ DIAGNOSE ━━━━━━━━━━━━━━━━━━━━━━━");
                        if let Some(ref debug) = result.debug {
                            eprintln!(
                                "Track:      {}",
                                debug.scoring_track.as_deref().unwrap_or("?")
                            );
                            if let Some(ref fp) = debug.fast_path {
                                eprintln!("Fast path:  {}", fp);
                            }
                            if !debug.query_terms.is_empty() {
                                eprintln!("Terms:      {:?}", debug.query_terms);
                            }
                        }
                        eprintln!("Retrieved:  {} files", retrieved.len());
                        for (i, f) in result.files.iter().enumerate() {
                            let marker = if gold_set.contains(f.path.as_str()) {
                                "✓ GOLD"
                            } else {
                                "  miss"
                            };
                            eprintln!("  #{:<2} {:60} {:6.3} {}", i + 1, f.path, f.score, marker);
                        }
                        if !gold.is_empty() {
                            eprintln!("Gold:       {} files", gold.len());
                            for g in &gold {
                                let found = retrieved.contains(&g.as_str());
                                let marker = if found { "✓ found" } else { "✗ MISSED" };
                                // Check if gold file appears in any stage
                                let mut stage_info = String::new();
                                if !found {
                                    if let Some(ref debug) = result.debug {
                                        if !debug.stages.is_empty() {
                                            for stage in &debug.stages {
                                                if let Some(entry) =
                                                    stage.files.iter().find(|e| e.path == *g)
                                                {
                                                    stage_info = format!(
                                                        " (seen in {} at score {:.4})",
                                                        stage.name, entry.score
                                                    );
                                                    break;
                                                }
                                            }
                                        }
                                        if stage_info.is_empty() && !debug.resolved_files.is_empty()
                                        {
                                            if let Some(rf) =
                                                debug.resolved_files.iter().find(|r| r.path == *g)
                                            {
                                                stage_info =
                                                    format!(" (resolved at score {:.1})", rf.score);
                                            }
                                        }
                                        if stage_info.is_empty() {
                                            stage_info = " (never seen in pipeline)".to_string();
                                        }
                                    }
                                }
                                eprintln!("  {:60} {}{}", g, marker, stage_info);
                            }
                            eprintln!(
                                "Scores:     P={:.3} R={:.3} F1={:.3}",
                                precision, recall, f1
                            );
                        }
                        eprintln!(
                            "Timing:     {:.0}ms",
                            result
                                .files
                                .first()
                                .map(|_| {
                                    // Use locate_ms if available from debug
                                    0.0 // timing is in the debug output
                                })
                                .unwrap_or(0.0)
                        );
                        eprintln!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
                        Ok(())
                    } else {
                        commands::locate::run(
                            &problem_text,
                            query,
                            json,
                            explain,
                            max_files_val,
                            max_files_explicit,
                            reference,
                            want_snippets,
                            paging,
                            commands::locate::LocateScope::with_tests(include_tests),
                            surface,
                        )
                        .await
                    }
                }
                Command::LocateDebug {
                    text,
                    target,
                    task_file,
                    max_files,
                    json,
                } => commands::locate_debug::run(text, target, task_file, max_files, json).await,
                Command::Embed {
                    batch_size,
                    max_seconds,
                    rebuild,
                    json,
                } => commands::embed::run(batch_size, json, max_seconds, rebuild).await,
                Command::Rename {
                    symbol,
                    new_name,
                    file,
                    line,
                    column,
                    json,
                } => commands::rename::run(symbol, new_name, file, line, column, json).await,
                Command::Refs {
                    entity,
                    kind,
                    file,
                    entity_kind,
                    bulk_json,
                    entities,
                    compact,
                    no_compact,
                    all,
                    json,
                    max_chars,
                    cursor,
                    view,
                } => {
                    // Both requirements are clap's now, so a caller who names
                    // nothing to operate on gets a usage block and exit 2 like
                    // the other 45 leaves, instead of exit 1 and one line.
                    if bulk_json {
                        let entities =
                            entities.expect("clap requires --entities alongside --bulk-json");
                        let effective_compact = compact && !no_compact;
                        commands::refs::run_bulk(entities, kind, effective_compact).await
                    } else {
                        let entity = entity.expect("clap requires an entity without --bulk-json");
                        // The flags are the `Name#kind@path` pin every resolver
                        // reads, so the daemon sees one spelling.
                        let entity = kin_cli::entity_ref::compose_entity_ref(
                            &entity,
                            file.as_deref(),
                            entity_kind.as_deref(),
                        );
                        if max_chars.is_some() || cursor.is_some() || view.is_some() {
                            commands::refs::run_page_with_view(
                                entity,
                                kind,
                                cursor,
                                max_chars,
                                view.as_deref(),
                            )
                            .await
                        } else {
                            commands::refs::run(entity, kind, all, json).await
                        }
                    }
                }
                Command::Ui {
                    path,
                    port,
                    no_open,
                } => commands::workspace_ui::run(path, port, !no_open).await,
                Command::Review {
                    change,
                    json,
                    entities,
                    files,
                    changes,
                    relations,
                    details,
                    action,
                } => {
                    if let Some(review_action) = action {
                        match review_action {
                            ReviewAction::Shadow {
                                range,
                                base,
                                head,
                                title,
                                source_url,
                                author,
                                json,
                            } => {
                                // Clap owns the choice between the positional
                                // range and the --base/--head pair, and pairs
                                // the two flags. What it cannot own is the
                                // shape of the range string, which is checked
                                // here because a value parser is the only other
                                // place to put it.
                                let (base, head) = match (range, base, head) {
                                    (Some(range), _, _) => match range.split_once("..") {
                                        Some((base, head))
                                            if !base.is_empty() && !head.is_empty() =>
                                        {
                                            (base.to_string(), head.to_string())
                                        }
                                        _ => anyhow::bail!(
                                            "invalid range '{}': expected <base>..<head>",
                                            range
                                        ),
                                    },
                                    (None, Some(base), Some(head)) => (base, head),
                                    _ => unreachable!(
                                        "clap requires a range or both --base and --head"
                                    ),
                                };
                                commands::review::shadow_report(
                                    base, head, title, source_url, author, json,
                                )
                                .await
                            }
                            ReviewAction::Create {
                                title,
                                base,
                                head,
                                description,
                            } => {
                                commands::review::create_review(title, base, head, description)
                                    .await
                            }
                            ReviewAction::Decide {
                                review_id,
                                state,
                                comment,
                            } => commands::review::decide_review(review_id, state, comment).await,
                            ReviewAction::Note {
                                review_id,
                                body,
                                scope,
                            } => commands::review::add_note(review_id, body, scope).await,
                            ReviewAction::Discuss {
                                review_id,
                                body,
                                scope,
                            } => commands::review::start_discussion(review_id, body, scope).await,
                            ReviewAction::Reply {
                                discussion_id,
                                body,
                            } => commands::review::reply_discussion(discussion_id, body).await,
                            ReviewAction::Resolve { discussion_id } => {
                                commands::review::resolve_discussion(discussion_id).await
                            }
                            ReviewAction::Assign {
                                review_id,
                                reviewer,
                            } => commands::review::assign_reviewer(review_id, reviewer).await,
                            ReviewAction::List { state } => {
                                commands::review::list_reviews(state).await
                            }
                            ReviewAction::Show { review_id } => {
                                commands::review::show_review(review_id).await
                            }
                            ReviewAction::Guide {
                                change,
                                working,
                                base,
                                json,
                            } => {
                                commands::review::guide(
                                    commands::review::GuideTarget::parse(change, working, base)?,
                                    json,
                                )
                                .await
                            }
                            ReviewAction::Open {
                                change,
                                working,
                                base,
                                port,
                                no_browser,
                            } => {
                                commands::review_ui::run(
                                    commands::review::GuideTarget::parse(change, working, base)?,
                                    port,
                                    !no_browser,
                                )
                                .await
                            }
                        }
                    } else if json {
                        commands::review::run_json(change, entities, files, changes).await
                    } else {
                        commands::review::run(change, entities, files, changes, relations, details)
                            .await
                    }
                }
                Command::History {
                    entity,
                    file,
                    kind,
                    reference,
                    all_revisions,
                } => {
                    // The flags are the `Name#kind@path` pin every resolver
                    // reads, so the daemon sees one spelling.
                    let entity = kin_cli::entity_ref::compose_entity_ref(
                        &entity,
                        file.as_deref(),
                        kind.as_deref(),
                    );
                    commands::history::run(entity, reference, all_revisions).await
                }
                Command::DeriveHistory { path, background } => {
                    let code = commands::history_derive::run_worker(path, background).await?;
                    if code != 0 {
                        std::process::exit(code);
                    }
                    Ok(())
                }
                Command::HostedPublication { action } => {
                    // The exit status carries the outcome a caller branches on,
                    // the way `kin init` reports an unattested conversion: a
                    // destination that holds nothing, one that holds somebody
                    // else's publication and one that cannot be read either way
                    // are three different answers, and the evidence record on
                    // stdout names which one it was.
                    let code = match action {
                        HostedPublicationAction::Publish {
                            manifest,
                            source,
                            intent_out,
                            evidence_out,
                            expect_repository_id,
                            expect_mode,
                        } => commands::hosted_publication::run_publish(
                            commands::hosted_publication::PublishArgs {
                                manifest,
                                source,
                                intent_out,
                                evidence_out,
                                expect_repository_id,
                                expect_mode,
                            },
                        )?,
                        HostedPublicationAction::Verify {
                            manifest,
                            intent,
                            evidence_out,
                            expect_repository_id,
                            expect_mode,
                        } => commands::hosted_publication::run_verify(
                            commands::hosted_publication::VerifyArgs {
                                manifest,
                                intent,
                                evidence_out,
                                expect_repository_id,
                                expect_mode,
                            },
                        )?,
                    };
                    if code != 0 {
                        std::process::exit(code);
                    }
                    Ok(())
                }
                Command::DeadCode {
                    seed,
                    limit,
                    name_pattern,
                } => match seed {
                    Some(query) => {
                        commands::dead_code::run_seeded(query, limit, name_pattern).await
                    }
                    None => commands::dead_code::run().await,
                },
                Command::TraceDataFlow {
                    focal,
                    cursor,
                    depth,
                    direction,
                    limit_per_step,
                    target,
                    no_bodies,
                    max_response_chars,
                    include_type_edges,
                    scope,
                    scope_mode,
                } => {
                    commands::trace_data_flow::run_seeded(
                        focal,
                        depth,
                        direction,
                        limit_per_step,
                        no_bodies.then_some(false),
                        max_response_chars,
                        include_type_edges.then_some(true),
                        target,
                        cursor,
                        scope,
                        scope_mode,
                    )
                    .await
                }
                Command::Deps { all, json } => commands::deps::run(all, json).await,
                Command::Xref { entity } => commands::xref::run(entity).await,
                Command::Spec { action } => match action {
                    SpecAction::Create { intent } => commands::spec::create(intent).await,
                    SpecAction::List { json } => commands::spec::list(json).await,
                    SpecAction::Show { id } => commands::spec::show(id).await,
                },
                Command::Merge { branch, json } => {
                    commands::capabilities::require_ready("merge")?;
                    // A parked merge is not an error and its report is already
                    // printed, so the outcome travels in the exit code the way
                    // `kin init` reports an unattested conversion.
                    let code = commands::merge::run(branch, json).await?;
                    if code != 0 {
                        std::process::exit(code);
                    }
                    Ok(())
                }
                Command::Conflicts { json, show } => {
                    commands::capabilities::require_ready("conflicts")?;
                    commands::conflicts::run(json, show).await
                }
                Command::Resolve {
                    ours,
                    theirs,
                    base,
                    remove,
                    keep_path,
                    file,
                    all_ours,
                    all_theirs,
                    do_continue,
                    abort,
                    expect,
                    json,
                } => {
                    commands::capabilities::require_ready("resolve")?;
                    commands::resolve::run(
                        ours,
                        theirs,
                        base,
                        remove,
                        keep_path,
                        file,
                        all_ours,
                        all_theirs,
                        do_continue,
                        abort,
                        expect,
                        json,
                    )
                    .await
                }
                Command::Stash { action } => match action {
                    StashAction::Push { message, yes } => commands::stash::push(message, yes).await,
                    StashAction::Pop => commands::stash::pop().await,
                    StashAction::List { json } => commands::stash::list(json).await,
                },
                Command::Blame {
                    entity,
                    file,
                    kind,
                    reference,
                    all_revisions,
                } => {
                    // The flags are the `Name#kind@path` pin every resolver
                    // reads, so the daemon sees one spelling.
                    let entity = kin_cli::entity_ref::compose_entity_ref(
                        &entity,
                        file.as_deref(),
                        kind.as_deref(),
                    );
                    commands::blame::run(entity, reference, all_revisions).await
                }
                Command::Agent { action } => {
                    // The agent loop is blocking, and a blocking HTTP client builds and
                    // drops its own runtime. Dropping one inside an async context panics,
                    // so the whole command runs on a plain thread outside this runtime.
                    let code = std::thread::spawn(move || match action {
                        AgentAction::Run {
                            task,
                            model,
                            base_url,
                            api_key_env,
                            repo,
                            mcp_command,
                            out,
                            max_tool_calls,
                            deadline,
                            context_tokens,
                            max_result_bytes,
                            system,
                            temperature,
                            tool_profile,
                        } => commands::agent::run(commands::agent::RunArgs {
                            task,
                            model,
                            base_url,
                            api_key_env,
                            repo,
                            mcp_command,
                            out,
                            max_tool_calls,
                            deadline,
                            context_tokens,
                            max_result_bytes,
                            system,
                            temperature,
                            tool_profile,
                        }),
                        AgentAction::Doctor {
                            base_url,
                            model,
                            repo,
                            mcp_command,
                            api_key_env,
                            tool_profile,
                        } => commands::agent::doctor(
                            base_url,
                            model,
                            repo,
                            mcp_command,
                            api_key_env,
                            tool_profile,
                        ),
                    })
                    .join()
                    .map_err(|_| anyhow::anyhow!("the agent thread panicked"))??;
                    // The run's own taxonomy is reported through the exit code: 2 tool-call
                    // budget, 3 deadline, 4 endpoint, 5 MCP, 7 context window. 6 is retired
                    // with the file tools whose unpublished changes it reported. A caller must
                    // be able to tell a task the agent could not do from an endpoint that was
                    // never there.
                    if code != 0 {
                        std::process::exit(code);
                    }
                    Ok(())
                }
                Command::Mcp { action } => match action {
                    McpAction::Start {
                        global,
                        repo,
                        tool_profile,
                        no_spawn,
                    } => commands::mcp::start(global, repo, tool_profile, no_spawn).await,
                },
                Command::Auth { action } => match action {
                    AuthAction::Login {
                        base_url,
                        no_browser,
                        provider,
                    } => commands::auth::login(base_url, no_browser, provider).await,
                    AuthAction::Logout { base_url } => commands::auth::logout(base_url).await,
                    AuthAction::Whoami { base_url } => commands::auth::whoami(base_url).await,
                    AuthAction::Status { base_url } => commands::auth::status(base_url).await,
                },
                Command::Remote { action } => match action {
                    RemoteAction::List { json } => commands::remote::list(json).await,
                    RemoteAction::Add {
                        name,
                        host,
                        transport,
                        url,
                        publish_review_state,
                        publish_proofs,
                        default,
                    } => {
                        commands::remote::add(
                            name,
                            host,
                            transport,
                            url,
                            publish_review_state,
                            publish_proofs,
                            default,
                        )
                        .await
                    }
                    RemoteAction::PlanPush {
                        remote,
                        url,
                        reference,
                        json,
                    } => {
                        commands::capabilities::require_ready("remote plan-push")?;
                        commands::transfer::plan_push(remote, url, reference, json).await
                    }
                    RemoteAction::Lease {
                        remote,
                        actor_id,
                        ttl_seconds,
                        json,
                    } => commands::remote::lease(remote, actor_id, ttl_seconds, json).await,
                    RemoteAction::Sessions { remote, json } => {
                        commands::remote::sessions(remote, json).await
                    }
                },
                Command::Publish {
                    packages,
                    registry,
                    dry_run,
                } => {
                    let registry = std::env::var("KIN_REGISTRY_URL").unwrap_or(registry);
                    commands::publish::run(packages, registry, dry_run).await
                }
                Command::Push {
                    remote,
                    url,
                    reference,
                    json,
                } => {
                    commands::capabilities::require_ready("push")?;
                    commands::transfer::push(remote, url, reference, json).await
                }
                Command::Pull {
                    remote,
                    url,
                    reference,
                    json,
                } => {
                    commands::capabilities::require_ready("pull")?;
                    commands::transfer::pull(remote, url, reference, json).await
                }
                Command::Clone {
                    url,
                    path,
                    repository,
                    verbose,
                } => {
                    // Exit 7 and 8 mean for a clone what they mean for
                    // `kin init`, whose post-admission work a clone now runs.
                    let code = commands::clone::run(url, path, repository, verbose).await?;
                    if code != 0 {
                        std::process::exit(code);
                    }
                    Ok(())
                }
                Command::Checkout {
                    path,
                    path_hex,
                    change,
                    entity,
                    expected,
                } => {
                    commands::capabilities::require_ready("checkout")?;
                    match entity {
                        Some(entity) => {
                            commands::entity_work::run_restore(entity, change, expected).await
                        }
                        None => commands::checkout::run(path, path_hex, change).await,
                    }
                }
                Command::Verify { action } => match action {
                    VerifyAction::Entity { entity } => commands::verify::run(entity).await,
                    VerifyAction::Plan { entity, depth } => {
                        commands::verify::plan(entity, depth).await
                    }
                    VerifyAction::Change { change_id, depth } => {
                        commands::verify::plan_change(change_id, depth).await
                    }
                    VerifyAction::Summary => commands::verify::summary().await,
                    VerifyAction::Missing => commands::verify::missing().await,
                    VerifyAction::Run {
                        entity,
                        runner,
                        depth,
                    } => commands::verify::run_verification(entity, runner, depth).await,
                },
                Command::Exec {
                    command,
                    shell,
                    keep,
                    discard,
                    strategy,
                } => {
                    commands::capabilities::require_ready("exec")?;
                    commands::session_run::exec(command, shell, keep, discard, strategy).await
                }
                Command::Telemetry { action } => match action {
                    TelemetryAction::Status => commands::telemetry::run_status().await,
                    TelemetryAction::Consent => commands::telemetry::run_consent().await,
                    TelemetryAction::Revoke => commands::telemetry::run_revoke().await,
                    TelemetryAction::Purge => commands::telemetry::run_purge().await,
                },
                Command::Support { json } => commands::support::run(json).await,
                Command::Languages { json } => commands::languages::run(json).await,
                Command::BenchMeta {
                    json,
                    prepared_state,
                    runtime_identity_only: _,
                } => commands::bench_meta::run(json, prepared_state).await,
                Command::PreparedState { action } => match action {
                    PreparedStateAction::Publish { target, json } => {
                        commands::prepared_state::publish(target, json).await
                    }
                    PreparedStateAction::Materialize { source, json } => {
                        commands::prepared_state::materialize(source, json).await
                    }
                },
                Command::Audit {
                    actor,
                    limit,
                    action,
                    since,
                    scope,
                } => {
                    commands::audit::run_with_filters(
                        actor,
                        limit,
                        commands::audit::AuditFilters {
                            action,
                            since,
                            scope,
                        },
                    )
                    .await
                }
                Command::Store { action } => match action {
                    StoreAction::Path { path, json } => commands::store::path(path, json),
                    StoreAction::Attach { store, path, json } => {
                        commands::store::attach(store, path, json)
                    }
                    StoreAction::List { size, json } => commands::store::list(json, size),
                    StoreAction::Prune {
                        yes,
                        unreachable,
                        json,
                    } => commands::store::prune(yes, unreachable, json),
                },
                Command::Backup { action } => match action {
                    BackupAction::Create { tag, output } => {
                        commands::backup::create(tag, output).await
                    }
                    BackupAction::List { json, directory } => {
                        commands::backup::list(json, directory).await
                    }
                    BackupAction::Restore {
                        name,
                        latest,
                        from,
                        target,
                    } => match (from, target) {
                        (Some(carrier), Some(destination)) => {
                            commands::backup::restore_carrier(carrier, destination).await
                        }
                        (None, None) => commands::backup::restore(name, latest).await,
                        _ => anyhow::bail!("--from and --target must be supplied together"),
                    },
                    BackupAction::Delete { name } => commands::backup::delete(name).await,
                },
                Command::Approvals { action } => match action {
                    ApprovalsAction::Show { change_id } => {
                        commands::approvals::show(change_id).await
                    }
                    ApprovalsAction::List => commands::approvals::list().await,
                },
                Command::Security { propagate } => {
                    commands::security::run_with_options(propagate).await
                }
                Command::Semver { base, head, json } => {
                    commands::capabilities::require_ready("semver")?;
                    commands::semver::run(base, head, json)
                }
                Command::Release { action } => match action {
                    ReleaseAction::Plan { offline } => commands::release_orch::plan(offline).await,
                    ReleaseAction::Apply {
                        crate_name,
                        version,
                        repos,
                        no_lock,
                    } => commands::release_orch::apply(crate_name, version, repos, !no_lock).await,
                    ReleaseAction::Intent { repo } => commands::release_orch::intent(repo).await,
                    ReleaseAction::Snapshot {
                        tag,
                        require_proof,
                        require_approval,
                        force,
                    } => {
                        commands::capabilities::require_ready("release snapshot")?;
                        commands::tag::snapshot(tag, require_proof, require_approval, force).await
                    }
                },
                Command::Tag {
                    tag,
                    require_proof,
                    require_approval,
                    force,
                } => {
                    commands::capabilities::require_ready("tag")?;
                    commands::tag::run(tag, require_proof, require_approval, force).await
                }
                Command::Rollback {
                    change_id,
                    feature,
                    discard_later,
                } => {
                    commands::capabilities::require_ready("rollback")?;
                    commands::rollback::run(change_id, feature, discard_later).await
                }
                Command::Revert { .. } => commands::rollback::refuse_revert(),
                Command::Bench { args } => commands::bench::bench_proxy(&args),
                Command::Migrate { source, target } => commands::migrate::run(source, target).await,
                Command::Upgrade { json } => commands::upgrade::run(json).await,
                Command::Cache { action } => match action {
                    CacheAction::Status { json, limit } => {
                        commands::cache::status(json, limit).await
                    }
                    CacheAction::Gc {
                        dry_run,
                        budget_gb,
                        prune_stale_schema,
                    } => commands::cache::gc(dry_run, budget_gb, prune_stale_schema).await,
                },
                Command::Graph { action } => match action {
                    GraphAction::Status {
                        json,
                        dependencies,
                        cursor,
                        max_chars,
                    } => {
                        commands::graph::status_with_request(
                            json,
                            kin_mcp::status_pages::StatusRequest {
                                dependencies,
                                cursor,
                                max_chars,
                            },
                        )
                        .await
                    }
                    GraphAction::Validate => commands::graph::validate().await,
                    GraphAction::Materialize { json } => commands::graph::materialize(json).await,
                    GraphAction::Owed { json } => commands::graph_owed::owed(json).await,
                    // The flags are the `Name#kind@path` pin every resolver
                    // reads, so the daemon sees one spelling.
                    GraphAction::Inspect {
                        name,
                        file,
                        kind,
                        json,
                    } => {
                        let name = kin_cli::entity_ref::compose_entity_ref(
                            &name,
                            file.as_deref(),
                            kind.as_deref(),
                        );
                        commands::graph::inspect(name, json).await
                    }
                    GraphAction::Source(args) | GraphAction::Body(args) => args.print().await,
                    GraphAction::Export {
                        limit,
                        kinds,
                        path,
                        include,
                        out,
                        json,
                    } => {
                        commands::graph_export::export(commands::graph_export::ExportArgs {
                            limit,
                            kinds,
                            path,
                            include,
                            out,
                            json,
                        })
                        .await
                    }
                    GraphAction::Watch { types, json } => {
                        commands::graph_export::watch(commands::graph_export::WatchArgs {
                            types,
                            json,
                        })
                        .await
                    }
                    GraphAction::Viz { port, open, limit } => {
                        commands::graph_viz::run(port, open, limit).await
                    }
                },
                Command::Git { action } => match action {
                    GitAction::Export { output } => commands::git::export(output),
                },
                Command::Intent { action } => match action {
                    IntentAction::List => commands::intent::list().await,
                    IntentAction::Register {
                        scope,
                        lock,
                        task,
                        session,
                    } => commands::intent::register(scope, lock, task, session).await,
                    IntentAction::Release { intent_id } => {
                        commands::intent::release(intent_id).await
                    }
                    IntentAction::Clear { session_id } => commands::intent::clear(session_id).await,
                },
                Command::Traffic { action } => match action {
                    TrafficAction::Show { scope } => commands::traffic::run(scope).await,
                    TrafficAction::Sessions => commands::traffic::sessions().await,
                },
                Command::Agents {
                    action: Some(AgentsAction::RecordEdit { client, wait }),
                    ..
                } => {
                    let mut raw = String::new();
                    let _ = std::io::Read::read_to_string(&mut std::io::stdin(), &mut raw);
                    if let Some(printed) =
                        commands::agent_edits::record_edit(&client, &raw, wait).await
                    {
                        println!("{printed}");
                    }
                    Ok(())
                }
                Command::Agents { json, action: None } => commands::agents::run(json).await,
                Command::Assistant { action } => match action {
                    AssistantAction::Install { assistant } => {
                        commands::assistant::install(assistant).await
                    }
                    AssistantAction::Doctor { assistant } => {
                        commands::assistant::run_doctor(assistant).await
                    }
                    AssistantAction::List { json } => commands::assistant::list(json).await,
                    AssistantAction::Sync => commands::assistant::sync().await,
                    AssistantAction::Configure {
                        sync_mode,
                        enable,
                        disable,
                    } => commands::assistant::configure(sync_mode, enable, disable).await,
                    AssistantAction::Snippets { assistant } => {
                        commands::assistant::snippets(assistant).await
                    }
                    AssistantAction::Hooks { assistant } => {
                        commands::assistant::hooks(assistant).await
                    }
                    AssistantAction::Prompt { assistant, mode } => {
                        commands::assistant::prompt(assistant, mode).await
                    }
                },
                Command::Work { action } => match action {
                    WorkAction::Create {
                        kind,
                        title,
                        description,
                        scope,
                        priority,
                    } => commands::work::create(kind, title, description, scope, priority).await,
                    WorkAction::List {
                        status,
                        kind,
                        scope,
                    } => commands::work::list(status, kind, scope).await,
                    WorkAction::Show { work_id } => commands::work::show(work_id).await,
                    WorkAction::Link { work_id, scope } => {
                        commands::work::link(work_id, scope).await
                    }
                    WorkAction::Decompose {
                        parent_work_id,
                        child_work_id,
                    } => commands::work::decompose(parent_work_id, child_work_id).await,
                    WorkAction::Block {
                        blocked_work_id,
                        blocker_work_id,
                    } => commands::work::block(blocked_work_id, blocker_work_id).await,
                    WorkAction::Implement { work_id, scope } => {
                        commands::work::implement(work_id, scope).await
                    }
                    WorkAction::Status { work_id, status } => {
                        commands::work::status(work_id, status).await
                    }
                    WorkAction::Close { work_id } => commands::work::close(work_id).await,
                    WorkAction::Verify { work_id } => commands::work::verify(work_id).await,
                },
                Command::Note { action } => match action {
                    NoteAction::Add { target, kind, body } => {
                        commands::note::add(target, kind, body).await
                    }
                    NoteAction::List { target } => commands::note::list(target).await,
                    NoteAction::Stale => commands::note::stale().await,
                },
                Command::Feature { title, description } => {
                    commands::work::create("feature".to_string(), title, description, None, None)
                        .await
                }
                Command::Todo { action } => match action {
                    TodoAction::Import { path } => commands::note::todo_import(path).await,
                },
                Command::Open { editor } => {
                    commands::capabilities::require_ready("open")?;
                    commands::session_run::open(editor).await
                }
                Command::PurgeIgnored {
                    confirm,
                    confirm_mass_deletion,
                } => {
                    commands::capabilities::require_ready("purge-ignored")?;
                    commands::purge_ignored::run(confirm, confirm_mass_deletion).await
                }
                Command::Admit => {
                    commands::capabilities::require_ready("admit")?;
                    commands::admit::run().await
                }
                Command::Allow { path, reason } => commands::allow::run(path, reason).await,
                Command::Reconcile {
                    session,
                    confirm_mass_deletion,
                } => {
                    commands::capabilities::require_ready("reconcile")?;
                    commands::reconcile::run(session, confirm_mass_deletion).await
                }
                Command::With {
                    assistant,
                    semantic_only,
                    task,
                } => {
                    commands::capabilities::require_ready("with")?;
                    commands::session_run::with(assistant, semantic_only, task).await
                }
                // Adjudicated before the runtime starts; see the early return
                // at the top of `main`.
                Command::SemanticOnlyGuard => {
                    commands::assistant_adapter::run_semantic_only_guard()
                }
                Command::Shell { strategy } => {
                    commands::capabilities::require_ready("shell")?;
                    commands::session_run::shell(strategy).await
                }
                Command::Overview { compact, json } => {
                    if json {
                        commands::overview::run_json().await
                    } else {
                        commands::overview::run(compact).await
                    }
                }
                Command::Completions { shell } => {
                    // Rendered into memory and written from here. clap_complete
                    // writes straight into the sink it is handed and turns a
                    // failed write into a panic of its own, `failed to write
                    // completion file`, so the write that can meet a closed
                    // pipe has to be one this crate makes: a reader that takes
                    // one line of the script and leaves gets the same clean
                    // exit as one that takes it all.
                    let mut script = Vec::new();
                    clap_complete::generate(shell, &mut Cli::command(), "kin", &mut script);
                    kin_cli::broken_pipe::write_stdout(&script);
                    Ok(())
                }
                Command::Update {
                    skip_verify,
                    channel,
                    expect_version,
                    expect_sha,
                    expect_archive_sha256,
                    check_only,
                    json,
                    ack_restart,
                    runtime_sessions,
                    set_policy,
                    apply,
                    dry_run,
                    unattended,
                    force_window,
                    prepare_windows_worker,
                    windows_worker,
                    cleanup_windows_worker,
                    bootstrap_published,
                    expect_target,
                    expect_archive_name,
                } => {
                    if let Some(record) = cleanup_windows_worker {
                        return commands::update::cleanup_windows_update_worker(&record);
                    }
                    if bootstrap_published {
                        return commands::update::run_bootstrap_published(
                            channel.context("published bootstrap requires an explicit channel")?,
                            expect_version.context("published bootstrap requires a version")?,
                            expect_sha
                                .context("published bootstrap requires an exact source commit")?,
                            expect_archive_sha256
                                .context("published bootstrap requires an archive digest")?,
                            expect_target
                                .context("published bootstrap requires an exact build target")?,
                            expect_archive_name
                                .context("published bootstrap requires an exact archive name")?,
                            apply,
                        )
                        .await;
                    }
                    commands::update::run(
                        skip_verify,
                        channel,
                        expect_version,
                        expect_sha,
                        expect_archive_sha256,
                        check_only,
                        json,
                        ack_restart,
                        runtime_sessions,
                        set_policy,
                        apply,
                        dry_run,
                        unattended,
                        force_window,
                        prepare_windows_worker,
                        windows_worker,
                    )
                    .await
                }
                Command::Registry { action } => match action {
                    Some(RegistryAction::Authority {
                        json,
                        fix,
                        initialize,
                    }) => commands::registry::authority(json, fix, initialize).await,
                    Some(RegistryAction::Daemons { json }) => {
                        commands::registry::daemons(json).await
                    }
                    Some(RegistryAction::Clean) => commands::registry::clean().await,
                    None => commands::registry::list().await,
                },
                Command::Daemon { action } => match action {
                    DaemonAction::Status { path, json } => {
                        commands::daemon::status(json, path.as_deref()).await
                    }
                    DaemonAction::Stop {
                        all,
                        machine,
                        when_unused,
                        json,
                    } => commands::daemon::stop(all, machine, when_unused, json).await,
                    DaemonAction::Sweep {
                        no_wait,
                        json,
                        verbose,
                        enable,
                    } => commands::daemon::sweep(no_wait, json, verbose, enable).await,
                },
                Command::Doctor {
                    fix,
                    remove_other_copies,
                    install_language_servers,
                    json,
                    verbose,
                    drift,
                    heal,
                    reclaim_staging,
                    conversion_source,
                } => {
                    // `--drift` reports the derived projection against graph
                    // truth; `--heal` rematerializes it. Bare `kin doctor`
                    // stays the first-run config health check.
                    if let Some(path) = conversion_source {
                        // A daemon graph read of one file's persisted
                        // coverage, like `kin graph inspect`, so it takes no
                        // readiness gate either.
                        commands::graph::doctor_conversion_source(path, json).await
                    } else if heal {
                        commands::capabilities::require_ready("doctor --heal")?;
                        commands::drift::heal(json).await
                    } else if drift {
                        commands::capabilities::require_ready("doctor --drift")?;
                        commands::drift::run(json).await
                    } else if reclaim_staging {
                        // Deliberately no `require_ready`. This deletes a
                        // directory a crashed `kin init` left behind, on a path
                        // where the repository was never created, which is
                        // precisely a machine with nothing ready on it. Gating
                        // it on readiness would refuse the only case it exists
                        // for.
                        commands::health::reclaim_stranded_stages(json)
                    } else {
                        commands::setup::doctor_with(
                            fix,
                            install_language_servers,
                            json,
                            verbose,
                            remove_other_copies,
                        )
                        .await
                    }
                }
                Command::Vfs { action } => match action {
                    VfsAction::On { mode } => commands::projection::on(mode).await,
                    VfsAction::Off => commands::projection::off().await,
                    VfsAction::Status { json } => commands::projection::status(json).await,
                },
                Command::Notify {
                    action,
                    title,
                    body,
                    level,
                    key,
                    cooldown,
                    latch,
                    json,
                } => {
                    let code = match action {
                        Some(NotifyAction::Clear { key, json }) => {
                            commands::notify::clear(&key, json)
                        }
                        Some(NotifyAction::Status { json }) => commands::notify::status(json),
                        Some(NotifyAction::Authorize { wait, json }) => {
                            commands::notify::authorize(std::time::Duration::from_secs(wait), json)
                        }
                        None => {
                            // clap cannot express "required unless a subcommand
                            // was given", so the pairing is checked here.
                            match (title.as_deref(), body.as_deref()) {
                                (Some(title), Some(body)) => commands::notify::send(
                                    title,
                                    body,
                                    &level,
                                    key.as_deref(),
                                    cooldown,
                                    latch,
                                    json,
                                ),
                                _ => Err(anyhow::anyhow!(
                                    "--title and --body are both required to send a notification"
                                )),
                            }
                        }
                    }?;
                    // Suppressed and undelivered are reported through the exit
                    // code so callers can branch without parsing output.
                    if code != 0 {
                        std::process::exit(code);
                    }
                    Ok(())
                }
                Command::Setup {
                    action,
                    intent,
                    mode,
                    shell,
                    auto_daemon,
                    no_interactive,
                    skip_mcp_check,
                    clients,
                    prompt_hook,
                    install_language_servers,
                    resource_profile,
                    embedding_model,
                    embedding_provider,
                    skip_path,
                    tool_profile,
                    verbose,
                    check,
                    shell_only,
                    json,
                } => {
                    if mode.is_some() {
                        eprintln!(
                            "kin setup: --mode is ignored. Kin has one repository format, so \
                             there is no mode to choose; drop it from the command."
                        );
                    }
                    match action {
                        Some(_) if shell_only => Err(anyhow::anyhow!(
                            "`kin setup --shell-only` runs on its own; drop the subcommand"
                        )),
                        None if shell_only => {
                            commands::setup::run_shell_only(shell.as_deref(), skip_path, json)
                        }
                        Some(SetupAction::Clients { json }) => commands::setup::clients(json),
                        Some(SetupAction::Status { json, verbose }) => {
                            commands::setup::status(json, verbose).await
                        }
                        Some(SetupAction::Doctor {
                            fix,
                            remove_other_copies,
                            json,
                        }) => {
                            commands::setup::doctor_with(
                                fix,
                                install_language_servers,
                                json,
                                false,
                                remove_other_copies,
                            )
                            .await
                        }
                        Some(SetupAction::Ledger { json }) => commands::setup::ledger_status(json),
                        Some(SetupAction::Uninstall {
                            all,
                            dry_run,
                            force,
                            json,
                        }) => commands::setup::uninstall(all, dry_run, force, json).await,
                        // `kin setup --check` is shorthand for the first-run health
                        // check without running the wizard.
                        None if check => commands::setup::doctor(false, false, false).await,
                        None => {
                            commands::setup::run_wizard(commands::setup::WizardOptions {
                                shell,
                                auto_daemon,
                                no_interactive,
                                intent,
                                skip_mcp_check,
                                prompt_hook,
                                install_language_servers,
                                resource_profile,
                                embedding_model,
                                embedding_provider,
                                skip_path,
                                tool_profile,
                                verbose,
                                clients,
                            })
                            .await
                        }
                    }
                }
                Command::Secret { action } => match action {
                    SecretAction::Set { name } => commands::secret::set(name).await,
                    SecretAction::List => commands::secret::list().await,
                    SecretAction::Delete { name } => commands::secret::delete(name).await,
                    SecretAction::SetRepo { name } => commands::secret::set_repo(name).await,
                    SecretAction::ListRepo => commands::secret::list_repo().await,
                },
                Command::Pipeline { action } => match action {
                    PipelineAction::List => commands::pipeline::list().await,
                    PipelineAction::Run { name } => commands::pipeline::run_pipeline(name).await,
                    PipelineAction::Logs { run_id } => commands::pipeline::logs(run_id).await,
                    PipelineAction::Cancel { run_id } => commands::pipeline::cancel(run_id).await,
                },
                Command::HostedRelease { action } => match action {
                    HostedReleaseAction::Create { tag, name, notes } => {
                        commands::release_cmd::create(tag, name, notes).await
                    }
                    HostedReleaseAction::List => commands::release_cmd::list().await,
                    HostedReleaseAction::Upload { release_id, file } => {
                        commands::release_cmd::upload(release_id, file).await
                    }
                },
            }
        })
        .instrument(root_span),
    );

    if let Some(session) = profile_session {
        if let Err(err) = session.write_report() {
            eprintln!(
                "warning: failed to write Kin profile to {}: {err}",
                session.output_path().display()
            );
        } else if profile_summary {
            eprintln!("{}", session.render_summary(12));
        }
    }

    result
}

fn current_command_name() -> String {
    Cli::command()
        .try_get_matches_from(std::env::args_os())
        .ok()
        .and_then(|matches| matches.subcommand_name().map(|value| value.to_string()))
        .unwrap_or_else(|| "help".to_string())
}

fn env_flag(name: &str) -> bool {
    std::env::var(name)
        .ok()
        .map(|value| {
            matches!(
                value.as_str(),
                "1" | "true" | "TRUE" | "yes" | "YES" | "on" | "ON"
            )
        })
        .unwrap_or(false)
}

/// Commands that run one long uninterruptible phase over a whole repository.
///
/// For everything else the default stays `warn`, because a short command that
/// prints its own result has nothing to report in between. These two can spend
/// hours inside a single call, and at that length no output at all is
/// indistinguishable from a wedged process: a full-history admission was once
/// killed after 11h43m of silence that turned out to be ordinary progress.
const PROGRESS_REPORTING_COMMANDS: [&str; 2] = ["init", "clone"];

/// Targets whose events are progress within an admission phase, not outcomes.
///
/// These report the same work the phase ladder is already drawing a line for,
/// repeatedly, while that line is on screen. Formatting them as log records put
/// ANSI escapes and the internal span path they were emitted under straight
/// through a redrawn progress display, so [`AdmissionProgressLayer`] takes them
/// and the fmt layer is filtered to leave them alone.
fn is_periodic_admission_progress(target: &str) -> bool {
    target == "kin_db::storage::history_replay"
}

/// The admission targets whose one event a command's own result restates.
///
/// Both carry terminal records: an admission that finished, and a recovered
/// stale staging. They are raised to `info` for `init` and `clone` because
/// [`AdmissionProgressLayer`] reads them, and until now the fmt layer rendered
/// them too. On `kin init` that put a line like
///
/// ```text
/// 2026-09-06T15:58:17Z  INFO kin_core::git_init: admitted exact Git repository as
/// graph-owned Kin authority path=... repository=... workspace=... generation=1
/// observed_trees=932 observed_entries=35028 sealed_bodies=1195 ...
/// seal=cca6fdfb47522ce9...
/// ```
///
/// between the phase ladder and the human result, eight machine fields and a
/// sixty-four-character seal, on the first command a new reader ever runs. Both
/// commands already print the path, the repository id, the workspace id and the
/// generation in their own result blocks, in that reader's own terms, so the
/// record restates what the next twelve lines say properly.
///
/// Only the fmt layer drops them. The progress layer still sees them, `RUST_LOG`
/// still brings them back in full, and nothing changes for any other command,
/// because no other command raises these targets above `warn` in the first
/// place.
const ADMISSION_TERMINAL_TARGETS: [&str; 2] = ["kin_core::init", "kin_core::git_init"];

/// Whether one event is an admission record the command restates itself.
///
/// Keyed on the event, never on its target. Those two targets carry five
/// callsites between them, not two: the two admission records, an
/// `initialized`/`recovered`/`reclaimed` trio about staging, and a `warn` about
/// a stage whose owner lock could not be attempted. One of the trio is the
/// notice telling a reader that `kin init` kept earlier staging directories and
/// that they cost disk, and it has no other sink at all. A target-keyed
/// suppression takes every one of them, so the reader loses a warning and a
/// disk notice to make a log line quieter.
///
/// The two records are the only events on these targets that carry both a
/// `repository` and a `workspace` field at `INFO`, which is what identifies
/// them: the staging trio carries `count` and `parent`, and the refusal is a
/// `warn` with `path` and `error`. The level is part of the test so a future
/// `warn` carrying the same fields is not swallowed by this.
fn is_terminal_admission_record(metadata: &tracing::Metadata<'_>) -> bool {
    if !ADMISSION_TERMINAL_TARGETS.contains(&metadata.target()) {
        return false;
    }
    if *metadata.level() != tracing::Level::INFO {
        return false;
    }
    let fields = metadata.fields();
    fields.field("repository").is_some() && fields.field("workspace").is_some()
}

/// Render an admission progress event onto the live phase line.
///
/// The event's own message is not reused. `history_replay` reports elapsed
/// whole seconds, so its terminal line reads "validated 6,413 changes in 0s"
/// for any replay under a second and says nothing useful about a replay that
/// took two minutes. The phase line already carries its own elapsed time, so
/// what is wanted from the event is how far along it is, which is what its
/// `validated` and `total` fields carry.
///
/// With no ladder open the line is printed plainly instead of dropped, so
/// running an admission phase's instrumentation outside the ladder still says
/// something, just without escapes or span paths.
#[derive(Default)]
struct AdmissionProgressLayer {
    /// How many times each phase has opened in this process.
    passes: std::sync::Mutex<std::collections::HashMap<String, u32>>,
}

#[derive(Default)]
struct AdmissionProgressFields {
    message: Option<String>,
    /// Which replay is reporting. Three phases report through this one target,
    /// so the phase, not the target, is what tells their counts apart.
    phase: Option<String>,
    /// What one unit of that phase is.
    unit: Option<String>,
    validated: Option<u64>,
    total: Option<u64>,
}

/// The line one admission progress event puts on the phase line.
///
/// Every replay that reports here gets its own words. They used to share one,
/// `N/T changes validated`, whatever the phase, unit or pass: a full-history
/// first save checks every commit's tree and replays its history twice, all
/// over the same number of commits, so the line ran `5376/5562 changes
/// validated` and then fell back to `2048/5562 changes validated` under the
/// same label, which reads as validation going backwards. Each phase and pass
/// now has its own label, and each one's count only ever rises.
///
/// A phase that opens again in the same process says which pass it is on. The
/// first save replays its history twice over the same changes, once to prove
/// entity revisions and once to prove the transaction, and the second pass's
/// count starts from zero.
///
/// `None` when the event carries nothing to say. A phase this does not know
/// still gets its own unit, so an added phase never borrows another's words.
fn admission_progress_line(fields: &AdmissionProgressFields, pass: u32) -> Option<String> {
    let unit = fields.unit.as_deref().unwrap_or("changes");
    let mut done = match fields.phase.as_deref() {
        Some("history_replay") => "changes replayed".to_string(),
        Some("git_projection_replay") => "commit trees checked".to_string(),
        Some("external_object_admission") => "Git objects admitted".to_string(),
        _ => format!("{unit} validated"),
    };
    if pass > 1 {
        done.push_str(&format!(", pass {pass}"));
    }
    match (fields.validated, fields.total) {
        (Some(validated), Some(total)) => Some(format!("{validated}/{total} {done}")),
        // The phase's terminal records carry no total. A refusal says so; a
        // finish is the count the phase line ends on, said the same way.
        (Some(validated), None)
            if fields
                .message
                .as_deref()
                .is_some_and(|message| message.contains("refused")) =>
        {
            Some(format!("refused after {validated} {done}"))
        }
        (Some(validated), None) => Some(format!("{validated} {done}")),
        // The phase's opening record: nothing done yet, out of this many.
        (None, Some(total)) => Some(format!("0/{total} {done}")),
        (None, None) => fields.message.clone(),
    }
}

impl tracing::field::Visit for AdmissionProgressFields {
    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        match field.name() {
            "validated" => self.validated = Some(value),
            "total" => self.total = Some(value),
            _ => {}
        }
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.message = Some(format!("{value:?}"));
        }
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        match field.name() {
            "message" => self.message = Some(value.to_string()),
            "phase" => self.phase = Some(value.to_string()),
            "unit" => self.unit = Some(value.to_string()),
            _ => {}
        }
    }
}

/// Which pass of its phase `fields` reports, counting the phase's opening
/// records in `passes`. A phase's opening record is the one that carries a
/// total and no count yet.
fn pass_of(
    passes: &mut std::collections::HashMap<String, u32>,
    fields: &AdmissionProgressFields,
) -> u32 {
    let Some(phase) = fields.phase.as_deref() else {
        return 1;
    };
    let opened = passes.entry(phase.to_string()).or_insert(0);
    if fields.validated.is_none() && fields.total.is_some() {
        *opened += 1;
    }
    (*opened).max(1)
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for AdmissionProgressLayer {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        if !is_periodic_admission_progress(event.metadata().target()) {
            return;
        }
        let mut fields = AdmissionProgressFields::default();
        event.record(&mut fields);
        let pass = match self.passes.lock() {
            Ok(mut passes) => pass_of(&mut passes, &fields),
            Err(_) => 1,
        };
        let Some(detail) = admission_progress_line(&fields, pass) else {
            return;
        };
        if !kin_core::report_admission_progress(&detail) {
            eprintln!("  {detail}");
        }
    }
}

/// Admission targets raised to `info` for [`PROGRESS_REPORTING_COMMANDS`].
///
/// Deliberately target-scoped rather than crate-wide. `kin_core=info` would also
/// surface every unrelated event those crates emit, which is how a default
/// filter becomes noise nobody reads.
///
/// What each target actually delivers today, because the difference matters to
/// anyone watching a long run:
///
/// - `kin_core::init` and `kin_core::git_init` carry three callsites between
///   them, and all three are **terminal**: two report a completed admission and
///   one reports recovered stale initialization stages. They add at most three
///   lines, and none of them appears mid-flight. They are enabled so the
///   admission surfaces its own outcome and so no further `kin-cli` change is
///   needed later.
/// - `kin_db::storage::history_replay` is the only periodic emitter, throttled
///   at its source. It is **live**, and has been since this workspace's KinDB
///   pin moved: the module ships in the pinned version, so the directive that
///   was written as pre-wired and inert now matches. That is how raw log
///   records first appeared inside the phase ladder's display, and it is why
///   [`AdmissionProgressLayer`] exists rather than the fmt layer rendering
///   them. Anything added to this list that emits mid-flight rather than
///   terminally needs the same treatment, or it will scribble over the ladder
///   the same way.
///
/// So a long admission does report mid-flight progress, on the phase line
/// itself rather than as separate records.
///
/// `kin_db::storage::snapshot` is deliberately not here, despite carrying three
/// live conditional `info!` sites. All three sit on `SnapshotManager` reopen and
/// reload paths, one after loading a vector sidecar, one on delta replay at
/// open, and one where pending deltas bypass the locate cache, so each of them
/// needs a snapshot that already exists. An admission never has one, because
/// `kin init` refuses outright when `.kin` is already present
/// (`reject_existing_repository`, run before any admission work in
/// `commands/init.rs`) rather than rebuilding authority over it. There is
/// nothing for an admission to reopen, replay, or load a sidecar from, so
/// naming the module would add a fourth directive that matches nothing on
/// either of these two commands.
///
/// That reason is structural and survives a refactor of who imports what. The
/// supporting type-name fact points the same way but is weaker: admission
/// builds authority through `RepositoryAuthorityManager`, which `kin-core`'s
/// `init.rs` and `git_init.rs` open, and the kin-db module defining that
/// manager holds no reference to `SnapshotManager` to reach those callsites
/// through. A future kin-db could invalidate that observation without failing
/// anything here, which is why it is the secondary note rather than the reason.
///
/// Where those events do fire, the daemon already runs at `info` and records
/// them, and on ordinary graph-reading commands they report finished counts
/// rather than progress, so raising them there would restate the mistake this
/// allowlist exists to avoid.
const ADMISSION_PROGRESS_TARGETS: [&str; 3] = [
    "kin_core::init",
    "kin_core::git_init",
    "kin_db::storage::history_replay",
];

/// The filter directives this process installs, `RUST_LOG` winning when set.
fn installed_directives(command: &str) -> String {
    match std::env::var_os("RUST_LOG") {
        Some(directives) => directives.to_string_lossy().into_owned(),
        None => default_filter_directives(command),
    }
}

/// Compose this process's subscriber from the layers the run asked for.
///
/// `--profile-out` ADDS the profiling layer. It does not replace the logging
/// stack, and the two are independent: installing one instead of the other made
/// a profiled run the quietest run there is, with `RUST_LOG` having no effect,
/// the admission ladder losing its ticks, and an operator who reached for the
/// profiler to explain a slow conversion losing the output that would have
/// explained it. It read as a quiet run rather than as a flag conflict.
///
/// The env filter is attached per layer rather than to the registry, and that is
/// what makes the composition possible at all. A registry-wide filter gates
/// every layer under it, the profiling layer included, and the default
/// directives are `warn`, so filtering the whole stack would hand back a profile
/// of an ordinary command with no `info` span in it. The profiling layer carries
/// no filter and records what it is given, which is what the JSON report is for;
/// each logging layer carries the directives the operator asked for.
fn tracing_stack<W>(
    directives: &str,
    profile_session: Option<kin_cli::profile::ProfileSession>,
    writer: W,
    ansi: bool,
    quiet_admission_records: bool,
) -> impl tracing::Subscriber + Send + Sync + 'static
where
    W: for<'writer> tracing_subscriber::fmt::MakeWriter<'writer> + Send + Sync + 'static,
{
    tracing_subscriber::registry()
        .with(profile_session.map(kin_cli::profile::ProfilingLayer::new))
        .with(AdmissionProgressLayer::default().with_filter(EnvFilter::new(directives)))
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(writer)
                .with_ansi(ansi)
                .with_filter(EnvFilter::new(directives).and(
                    tracing_subscriber::filter::filter_fn(move |metadata| {
                        // Two separate reasons a record does not belong on the
                        // fmt layer, named, then one negation over them. The
                        // reasons are what a reader of this filter needs, and
                        // `!a && !b` spelled out inline is the same expression
                        // clippy's nonminimal_bool refuses either way, so the
                        // names carry the meaning and the shape stays minimal.
                        let scribbles_on_the_ladder =
                            is_periodic_admission_progress(metadata.target());
                        let restated_by_the_command =
                            quiet_admission_records && is_terminal_admission_record(metadata);
                        !(scribbles_on_the_ladder || restated_by_the_command)
                    }),
                )),
        )
}

/// The directive string [`installed_directives`] returns, absent `RUST_LOG`.
///
/// Split out so the choice is testable without mutating process environment,
/// which no test can do without racing every other test in the binary.
///
/// Profiling is deliberately absent here, and now for a better reason than the
/// one that used to be written down: a profiled run installs these same
/// directives on these same logging layers, so there is no profiling branch for
/// this function to carry.
fn default_filter_directives(command: &str) -> String {
    if !PROGRESS_REPORTING_COMMANDS.contains(&command) {
        return "warn".to_string();
    }
    let mut directives = String::from("warn");
    for target in ADMISSION_PROGRESS_TARGETS {
        directives.push(',');
        directives.push_str(target);
        directives.push_str("=info");
    }
    directives
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    /// `kin exec` takes no scope. A scoped session workspace is refused by the
    /// daemon until its selected artifact set can be authenticated outside the
    /// editable session, so the command materializes the whole session. Read off
    /// the clap definition rather than a parse, because `exec`'s command
    /// positional accepts hyphen values and could swallow a stray flag into the
    /// command it runs instead of rejecting it.
    #[test]
    fn exec_carries_no_scope_flag() {
        std::thread::Builder::new()
            .stack_size(16 * 1024 * 1024)
            .spawn(|| {
                let cli = Cli::command();
                let exec = cli
                    .find_subcommand("exec")
                    .expect("kin exec is a subcommand");
                let ids: Vec<String> = exec
                    .get_arguments()
                    .map(|arg| arg.get_id().as_str().to_string())
                    .collect();
                // Positive control: the lookup sees exec's real flags, so the
                // absence below is not a lookup that saw nothing.
                assert!(ids.iter().any(|id| id == "strategy"), "{ids:?}");
                assert!(!ids.iter().any(|id| id == "scope"), "{ids:?}");
            })
            .unwrap()
            .join()
            .unwrap();
    }

    #[test]
    fn recovery_restore_requires_a_complete_fresh_target_pair() {
        std::thread::Builder::new()
            .stack_size(16 * 1024 * 1024)
            .spawn(|| {
                assert!(
                    Cli::try_parse_from(["kin", "backup", "create", "--output", "backup"]).is_ok()
                );
                assert!(Cli::try_parse_from([
                    "kin",
                    "backup",
                    "list",
                    "--directory",
                    "backups",
                    "--json"
                ])
                .is_ok());
                assert!(Cli::try_parse_from([
                    "kin",
                    "backup",
                    "restore",
                    "--from",
                    "/backup",
                    "--target",
                    "/fresh/.kin"
                ])
                .is_ok());
                for args in [
                    vec!["kin", "backup", "restore", "--from", "/backup"],
                    vec!["kin", "backup", "restore", "--target", "/fresh/.kin"],
                    vec![
                        "kin",
                        "backup",
                        "restore",
                        "--latest",
                        "--from",
                        "/backup",
                        "--target",
                        "/fresh/.kin",
                    ],
                    vec![
                        "kin",
                        "backup",
                        "restore",
                        "old-name",
                        "--from",
                        "/backup",
                        "--target",
                        "/fresh/.kin",
                    ],
                ] {
                    assert!(Cli::try_parse_from(args).is_err());
                }
            })
            .unwrap()
            .join()
            .unwrap();
    }

    /// Every flag a doctor fix line tells a user to type must exist.
    ///
    /// `kin doctor` shipped a fix line reading "`kin reconcile --admit`" from
    /// v0.6.0 (FIR-3003). `reconcile` takes `session` and
    /// `--confirm-mass-deletion`, so clap rejects `--admit` outright: the one
    /// action the report offered could not be performed, and nothing noticed
    /// for three minor versions because a fix line is prose to every test that
    /// reads it.
    ///
    /// This asserts the prose against the parser. The strings live in the lib
    /// and the clap definition lives in this binary, so the test reads
    /// `health.rs` from disk rather than importing it; that is also why it
    /// checks its own reading, since a scan that found nothing would pass.
    #[test]
    fn every_flag_a_doctor_fix_line_names_exists_in_the_cli() {
        // `Cli::command()` builds the whole derive tree at once and overflows a
        // test thread's default 2 MiB stack on this CLI, which ABORTS the
        // process. An abort is a red that graded nothing, and from an exit code
        // alone it reads exactly like a caught bug, so the body runs on a
        // thread with room and the assertions below are actually reached.
        std::thread::Builder::new()
            .stack_size(32 * 1024 * 1024)
            .spawn(fix_line_flags_exist)
            .expect("spawn the checking thread")
            .join()
            .expect("the fix-line check must not panic in the harness thread");
    }

    fn fix_line_flags_exist() {
        // Every command surface, not just health.rs. The narrow scan was written
        // for doctor's fix lines and found four more defects in that one file on
        // its first run; three more sit in files it never read, so the scope is
        // the whole commands directory (FIR-3009).
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/src/commands");
        let mut sources: Vec<(String, String)> = Vec::new();
        let mut stack = vec![std::path::PathBuf::from(dir)];
        while let Some(at) = stack.pop() {
            for entry in std::fs::read_dir(&at).expect("the commands directory is readable") {
                let path = entry.expect("a readable directory entry").path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    let name = path
                        .strip_prefix(dir)
                        .unwrap_or(&path)
                        .to_string_lossy()
                        .into_owned();
                    sources.push((
                        name,
                        std::fs::read_to_string(&path).expect("a readable source file"),
                    ));
                }
            }
        }
        // A scan that reads no files passes every assertion below for the wrong
        // reason, and the count is pinned rather than the emptiness so a walk
        // that quietly stops descending is visible too.
        assert!(
            sources.len() >= 40,
            "the walk found only {} source files under {dir}, which is too few to be reading \
             the directory it thinks it is",
            sources.len()
        );

        // Backticked `kin ...` spans are how a fix line or hint names a command.
        // Comment lines are dropped whole, before the split, because a doc
        // comment quotes commands it is not telling anyone to run: the prose
        // `kin 0.5.40` is a version and `kin health` is a surface name, and
        // grading either reports a defect that does not exist. Dropping the
        // lines rather than the spans keeps backtick parity self-consistent.
        let mut commands: Vec<(String, String)> = Vec::new();
        for (file, source) in &sources {
            // A Rust string literal continues across lines with a trailing
            // backslash, which eats the newline and the next line's indent. A
            // backticked span split that way reads as `kin \` and gets reported
            // as a command nobody wrote, so the continuation is rejoined first.
            let mut graded = String::with_capacity(source.len());
            let mut continuing = false;
            for line in source.lines() {
                if !continuing && line.trim_start().starts_with("//") {
                    continue;
                }
                let piece = if continuing { line.trim_start() } else { line };
                let joins = piece.ends_with('\\');
                graded.push_str(piece.strip_suffix('\\').unwrap_or(piece));
                if !joins {
                    graded.push('\n');
                }
                continuing = joins;
            }
            for span in graded.split('`').skip(1).step_by(2) {
                let span = span.trim();
                let Some(rest) = span.strip_prefix("kin ") else {
                    continue;
                };
                // A format placeholder is not a command. `kin {}` is a template
                // whose subcommand arrives at runtime, so there is nothing here
                // to check against clap and reporting it is a false finding.
                if rest.contains('{') || rest.contains('}') {
                    continue;
                }
                commands.push((file.clone(), rest.to_string()));
            }
        }

        // The scan must find something, or every assertion below is vacuous.
        // Pinned by count rather than by emptiness so a refactor that quietly
        // drops most of them is visible too.
        assert!(
            commands.len() >= 200,
            "the scan found only {} backticked `kin ...` spans under the commands directory, \
             which is too few to be reading the tree it thinks it is",
            commands.len()
        );

        let root = Cli::command();
        let mut offenders: Vec<String> = Vec::new();
        for (file, command) in &commands {
            // Walk the subcommand path, then check every flag against the
            // command that path resolved to. A word that is not a known
            // subcommand ends the path: it is a positional value, like the `.`
            // in `kin init .`.
            let mut node = &root;
            let mut flags: Vec<&str> = Vec::new();
            for word in command.split_whitespace() {
                if let Some(flag) = word.strip_prefix("--") {
                    let flag =
                        flag.trim_end_matches(|c: char| !c.is_ascii_alphanumeric() && c != '-');
                    if !flag.is_empty() {
                        flags.push(flag);
                    }
                    continue;
                }
                if flags.is_empty() {
                    match node.find_subcommand(word) {
                        Some(next) => node = next,
                        // A node that HAS subcommands expects one, so an
                        // unknown word there is a command that does not exist
                        // rather than a positional value. Checking only flags
                        // would pass `kin daemon zzz` silently, which is the
                        // same defect this guard exists for wearing a
                        // subcommand instead of a flag. A node with no
                        // subcommands takes positionals, like the `.` in
                        // `kin init .`, so the check does not apply there.
                        None if node.has_subcommands() => {
                            offenders.push(format!(
                                "{file}: `kin {command}` names the subcommand {word}, which \
                                 `{}` does not have",
                                node.get_name()
                            ));
                            break;
                        }
                        None => {}
                    }
                }
            }
            for flag in flags {
                // `help` and `version` are clap's own, generated from
                // `#[command(version = ...)]` rather than declared as fields, so
                // they are absent from get_arguments() and a check that only
                // walks that list calls the working `kin --version` a defect.
                let builtin = flag == "help" || flag == "version";
                // A long ALIAS is a real spelling of the flag: `--continue` is
                // declared as `#[arg(long, alias = "continue")] do_continue`, so
                // reading get_long() alone rejects a command that runs.
                let known = builtin
                    || node.get_arguments().any(|arg| {
                        arg.get_long() == Some(flag)
                            || arg
                                .get_all_aliases()
                                .is_some_and(|aliases| aliases.contains(&flag))
                    });
                if !known {
                    offenders.push(format!(
                        "{file}: `kin {command}` names --{flag}, which `{}` does not take",
                        node.get_name()
                    ));
                }
            }
        }

        assert!(
            offenders.is_empty(),
            "a user-facing string tells a user to type a command the CLI rejects:\n  {}",
            offenders.join("\n  ")
        );
    }

    /// The admission commands must carry every configured target without
    /// `RUST_LOG` being set.
    #[test]
    fn progress_reporting_commands_raise_admission_targets_to_info() {
        for command in PROGRESS_REPORTING_COMMANDS {
            let directives = default_filter_directives(command);
            assert!(
                directives.starts_with("warn"),
                "{command} must keep warn as its floor: {directives}"
            );
            for target in ADMISSION_PROGRESS_TARGETS {
                assert!(
                    directives.contains(&format!("{target}=info")),
                    "{command} must report {target} progress: {directives}"
                );
            }
        }
    }

    /// The filter is keyed on the clap subcommand name, so a renamed command
    /// would silently stop reporting progress with every other test still green.
    /// This CLI does rename subcommands, so pin the names against clap itself.
    #[test]
    fn progress_reporting_commands_are_real_subcommands() {
        on_cli_test_stack(|| {
            let command = Cli::command();
            let known: Vec<String> = command
                .get_subcommands()
                .map(|sub| sub.get_name().to_string())
                .collect();
            for expected in PROGRESS_REPORTING_COMMANDS {
                assert!(
                    known.iter().any(|name| name == expected),
                    "{expected:?} is not a kin subcommand; known: {known:?}"
                );
            }
        });
    }

    /// `--force-window` names the window bounds as numbers a user reads, and a
    /// clap doc comment cannot interpolate a constant without a compile-time
    /// formatting crate, which is not a dependency worth adding for a help
    /// string. So the literals are pinned to the constants here instead: move
    /// either bound and this test names the help text still claiming the old one.
    ///
    /// The class is worth stating, because this change was repairing it
    /// elsewhere. Three sentences in this file described update behaviour that
    /// had been removed, and nothing compared them against the code, so they
    /// stayed green through every gate. Help text is written once and re-read by
    /// users rather than by CI.
    #[test]
    fn the_update_help_names_the_real_deferral_bounds() {
        on_cli_test_stack(|| {
            let command = Cli::command();
            let update = command
                .get_subcommands()
                .find(|sub| sub.get_name() == "update")
                .expect("kin update is a subcommand");
            let arg = update
                .get_arguments()
                .find(|arg| arg.get_id().as_str() == "force_window")
                .expect("--force-window is an argument of kin update");
            // Read both halves. clap splits a doc comment into short and long
            // help at the first blank line, so asserting on one of them would
            // pass or fail on where the paragraph breaks rather than on what
            // the text says.
            let help = format!(
                "{} {}",
                arg.get_help()
                    .map(|help| help.to_string())
                    .unwrap_or_default(),
                arg.get_long_help()
                    .map(|help| help.to_string())
                    .unwrap_or_default(),
            );
            for bound in [
                commands::update::UNATTENDED_DEFERRAL_WINDOW_HOURS,
                commands::update::UNATTENDED_DEFERRAL_FLOOR_HOURS,
            ] {
                assert!(
                    help.contains(&format!("{bound}h")),
                    "--force-window help must name the {bound}h bound, found: {help}"
                );
            }
        });
    }

    /// Ordinary commands print their own result, so raising them would only add
    /// noise to every invocation.
    #[test]
    fn ordinary_commands_keep_the_quiet_default() {
        for command in ["locate", "commit", "status", "log", "help"] {
            assert!(
                !PROGRESS_REPORTING_COMMANDS.contains(&command),
                "{command} is not a long single-phase command"
            );
            assert_eq!(
                default_filter_directives(command),
                "warn",
                "{command} must stay quiet by default"
            );
        }
    }

    #[test]
    fn locate_ranking_and_routed_coordinate_calls_have_distinct_cli_contracts() {
        on_cli_test_stack(|| {
            let ranked = Cli::try_parse_from(["kin", "locate", "src/main.rs"])
                .expect("standalone locate accepts problem text");
            assert!(
                matches!(ranked.command, Command::Locate { text: Some(ref text), .. }
                if text == "src/main.rs")
            );
            let arguments = r#"{"query":"src/main.rs"}"#;
            let routed = Cli::try_parse_from(["kin", "call", "locate", arguments])
                .expect("routed locate accepts the MCP JSON contract");
            assert!(
                matches!(routed.command, Command::Call { ref tool, arguments: Some(ref args), .. }
                if tool == "locate" && args == arguments)
            );
            let cli = Cli::command();
            let locate = cli.find_subcommand("locate").expect("locate help");
            assert!(locate
                .get_long_about()
                .unwrap()
                .to_string()
                .contains("kin call locate"));
        });
    }

    /// A malformed directive is silently dropped by `EnvFilter`, which would
    /// disable progress reporting while every other test still passed. Parse the
    /// exact string the binary installs and require each target to survive.
    #[test]
    fn every_default_directive_parses_and_is_retained() {
        for command in ["init", "clone", "locate", "help"] {
            let directives = default_filter_directives(command);
            let filter = tracing_subscriber::filter::EnvFilter::builder()
                .parse(&directives)
                .unwrap_or_else(|error| {
                    panic!("default directives {directives:?} must parse: {error}")
                });
            let rendered = filter.to_string();
            for directive in directives.split(',') {
                assert!(
                    rendered.contains(directive),
                    "directive {directive:?} was dropped from {rendered:?}"
                );
            }
        }
    }

    // Behavioral probes for the default log filter.
    //
    // The assertions above check the shape of the directive string, and a string
    // can be correct while admitting nothing. `EnvFilter` resolves directives
    // against a callsite at runtime, so a reordered list, an added global
    // directive, or a target shadowed by a broader one all leave the string
    // intact and every assertion above green while the event is dropped. These
    // probes install what production installs and assert on what actually
    // reaches a writer.
    //
    // They build the filter from `default_filter_directives` rather than calling
    // `installed_directives`, deliberately: that function consults `RUST_LOG`, and
    // a probe inheriting the developer's or the runner's environment would fail
    // or pass vacuously depending on it. `EnvFilter::new` over the pure
    // directive string is exactly what production installs when `RUST_LOG` is
    // unset.

    /// Collects everything a subscriber writes, standing in for the terminal.
    #[derive(Clone)]
    struct CapturedLog(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for CapturedLog {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .expect("log buffer poisoned")
                .extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CapturedLog {
        type Writer = CapturedLog;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// The filter a plain run of `command` installs, absent `RUST_LOG`.
    fn installed_filter(command: &str) -> EnvFilter {
        EnvFilter::new(default_filter_directives(command))
    }

    /// Run `emit` under `filter` and report whether anything reached the writer.
    ///
    /// The caller emits rather than naming a target, because `tracing`'s macros
    /// build a static callsite and so require a literal target. A helper taking
    /// `&'static str` does not compile, which is why the probes below spell out
    /// one emission per target instead of looping over
    /// `ADMISSION_PROGRESS_TARGETS`. Looping over commands stays available,
    /// since a command reaches the filter as an ordinary argument.
    fn survives(filter: EnvFilter, emit: impl FnOnce()) -> bool {
        let buffer = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry()
            .with(filter)
            .with(tracing_subscriber::fmt::layer().with_writer(CapturedLog(buffer.clone())));
        tracing::subscriber::with_default(subscriber, emit);
        let captured = buffer.lock().expect("log buffer poisoned").len();
        captured > 0
    }

    /// Forces an edit in this file whenever the target list changes length.
    ///
    /// That is the whole of what it guarantees, and it is less than it looks.
    /// `ADMISSION_PROGRESS_TARGETS` is declared `[&str; 3]`, so `.len()` is a
    /// compile-time constant and this assertion cannot fail on its own. What
    /// trips it is the edit that adding a target requires: changing the array's
    /// length parameter forces changing this literal, which puts the author in
    /// the module where the probes live, next to the reason a new entry needs
    /// one. It is a reminder, not a coupling, and it says nothing about
    /// identity. Swapping one target for another holds the length at 3 and
    /// leaves this test green.
    ///
    /// A swapped target is caught by
    /// `an_admission_command_admits_every_progress_target` instead, whose probes
    /// name their targets as literals. `EnvFilter` matches a directive against
    /// an event's target by prefix, so a renamed entry stops prefix-matching the
    /// probe that names the old one, the event falls back to the `warn` floor,
    /// and the probe stops surviving.
    #[test]
    fn every_admission_target_has_a_probe() {
        assert_eq!(
            ADMISSION_PROGRESS_TARGETS.len(),
            3,
            "add a probe below when adding an admission target"
        );
    }

    /// Every progress-reporting command must admit the whole target set on its
    /// own, which is stronger than the set being admitted somewhere.
    ///
    /// `default_filter_directives` emits one identical string for every member
    /// of `PROGRESS_REPORTING_COMMANDS` today, so probing `init` for one target
    /// and `clone` for another would pass. It would also keep passing the moment
    /// that function grew a per-command branch, which is a natural extension the
    /// first time `clone` wants a transport target `init` has no use for. The
    /// set would then be satisfied across two commands with neither carrying it,
    /// and the name of this test would be a claim its body no longer made.
    #[test]
    fn an_admission_command_admits_every_progress_target() {
        for command in PROGRESS_REPORTING_COMMANDS {
            assert!(
                survives(installed_filter(command), || tracing::info!(
                    target: "kin_core::init",
                    "probe"
                )),
                "kin {command} must admit the admission receipt"
            );
            assert!(
                survives(installed_filter(command), || tracing::info!(
                    target: "kin_core::git_init",
                    "probe"
                )),
                "kin {command} must admit Git admission output"
            );
            // Live, not pre-wired: the module ships in the pinned KinDB and
            // emits during the commit phase. It has to survive the filter to
            // reach `AdmissionProgressLayer`, which is what draws it onto the
            // phase line instead of letting the fmt layer print it.
            assert!(
                survives(installed_filter(command), || tracing::info!(
                    target: "kin_db::storage::history_replay",
                    "probe"
                )),
                "kin {command} must admit the replay progress target"
            );
        }
    }

    /// The periodic replay target reaches the progress layer and not the fmt
    /// layer, and every other admission target keeps printing as a record.
    ///
    /// Both halves matter and they fail differently. If the replay target were
    /// not excluded from fmt it would print raw, with ANSI and the internal
    /// span path, over the redrawn phase line, which is the defect. If the
    /// exclusion were written broadly enough to catch `kin_core::git_init` it
    /// would swallow the admission receipt, and the command would finish
    /// saying nothing about what it admitted.
    #[test]
    fn only_the_periodic_replay_target_is_taken_off_the_record_layer() {
        assert!(is_periodic_admission_progress(
            "kin_db::storage::history_replay"
        ));
        for printed in [
            "kin_core::init",
            "kin_core::git_init",
            "kin_db::storage::snapshot",
            "kin_db::storage::repository",
            "kin_cli",
        ] {
            assert!(
                !is_periodic_admission_progress(printed),
                "{printed} must keep printing as a record"
            );
        }
    }

    /// `--profile-out` adds the profiling layer to the logging stack rather
    /// than replacing it.
    ///
    /// Installing one instead of the other made a profiled run the quietest run
    /// there is. On one 3,111 second conversion `RUST_LOG=info,kin_db=info` had
    /// no effect at all and only one of the commit phase's four sub-step ticks
    /// reached the log, so the single run meant to explain where the time went
    /// produced the least output of any run.
    ///
    /// All three halves are asserted, because each fails differently. A stack
    /// that logs but never profiles is the same defect pointed the other way. A
    /// stack that logs everything has lost the filter, which is how the phase
    /// ladder gets scribbled over. And the span has to reach the report, not
    /// merely be created, or `--profile-out` writes an empty ladder.
    #[test]
    fn profiling_adds_a_layer_rather_than_replacing_the_logging_stack() {
        // The session is read in memory through `report()`; nothing is written,
        // so this path never has to exist.
        let session = kin_cli::profile::ProfileSession::new(
            "init",
            "/tmp",
            std::env::temp_dir().join("kin-profile-composition-test.json"),
        );
        let buffer = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let subscriber = tracing_stack(
            &default_filter_directives("init"),
            Some(session.clone()),
            CapturedLog(buffer.clone()),
            false,
            // Composition is what is under test here, so the admission record
            // this asserts on is left reaching the fmt layer. The suppression
            // has its own test beside this one.
            false,
        );

        tracing::subscriber::with_default(subscriber, || {
            tracing::info_span!("kin.init.commit_bootstrap_transaction").in_scope(|| {
                tracing::info!(target: "kin_core::init", "admitted the repository");
                tracing::debug!(target: "kin_core::init", "under the installed floor");
            });
        });

        let logged = String::from_utf8(buffer.lock().expect("log buffer poisoned").clone())
            .expect("captured log is utf-8");
        assert!(
            logged.contains("admitted the repository"),
            "a profiled run must keep the logging its unprofiled twin has; captured {logged:?}"
        );
        assert!(
            !logged.contains("under the installed floor"),
            "the installed directives must still filter under profiling; captured {logged:?}"
        );

        let report = session.report();
        assert!(
            report
                .spans
                .iter()
                .any(|span| span.name == "kin.init.commit_bootstrap_transaction"),
            "the profile must carry the span the run opened; spans={:?}",
            report
                .spans
                .iter()
                .map(|span| span.name.as_str())
                .collect::<Vec<_>>()
        );
    }

    /// `kin init` no longer prints its own admission record at the reader.
    ///
    /// The record is a terminal one, so it landed between the phase ladder and
    /// the human result with eight machine fields and a sixty-four-character
    /// seal on it. The result block that follows already names the path, the
    /// repository, the workspace and the generation, so the record restated
    /// them in a shape nobody reading a first `kin init` can use.
    ///
    /// Both arms, because dropping the record everywhere would take it from the
    /// operator who asked for it, and that failure is invisible until someone
    /// needs the log.
    #[test]
    fn the_admission_record_is_quiet_by_default_and_returns_under_rust_log() {
        let quiet = captured_with_quiet_admission(true);
        assert!(
            !quiet.contains("admitted exact Git repository"),
            "the default `kin init` still prints its own admission record; captured {quiet:?}"
        );

        let asked_for = captured_with_quiet_admission(false);
        assert!(
            asked_for.contains("admitted exact Git repository"),
            "RUST_LOG must still deliver the record; captured {asked_for:?}"
        );
    }

    /// Only the two admission records are quietened, and nothing else on
    /// those targets.
    ///
    /// The first shape of this keyed on the target, and those two targets carry
    /// five callsites, not two. It swallowed a `warn` about a stage whose owner
    /// lock could not be attempted, and the notice telling a reader that
    /// `kin init` kept earlier staging directories and that they cost disk,
    /// which has no other sink at all. A reader lost a warning and a disk
    /// notice so that a log line could be quieter.
    ///
    /// Every one of the five shapes is exercised here, because the ones that
    /// must survive are the point.
    #[test]
    fn the_suppression_takes_the_records_and_leaves_the_notices() {
        let quiet = captured_with_quiet_admission(true);

        assert!(
            !quiet.contains("admitted exact Git repository"),
            "the git admission record must be quiet by default: {quiet:?}"
        );
        assert!(
            !quiet.contains("initialized unborn kin repository authority"),
            "the native admission record must be quiet by default: {quiet:?}"
        );
        assert!(
            quiet.contains("could not prove"),
            "the retained-staging disk notice has no other sink and must still print: {quiet:?}"
        );
        assert!(
            quiet.contains("owner lock could not be attempted"),
            "a warn on these targets must never be swallowed: {quiet:?}"
        );
        assert!(
            quiet.contains("reclaimed stranded repository initialization stages"),
            "the staging trio must still print: {quiet:?}"
        );
        assert!(
            quiet.contains("a warning that happens to name a repository"),
            "only INFO records are quietened, so a warn carrying the same fields must still \
             print: {quiet:?}"
        );
    }

    /// The phase ladder's own progress is untouched by the suppression.
    ///
    /// The two live on the same directives, so a fix that quietened the record
    /// by lowering `kin_core::git_init` out of the filter would take the
    /// ladder's input with it, and the symptom would be a `kin init` that shows
    /// no phases at all.
    #[test]
    fn quieting_the_admission_record_leaves_the_periodic_progress_alone() {
        assert!(
            !ADMISSION_TERMINAL_TARGETS.contains(&"kin_db::storage::history_replay"),
            "the periodic progress target must not be swept up in the suppression"
        );
        assert!(
            is_periodic_admission_progress("kin_db::storage::history_replay"),
            "the periodic target must still be the one the ladder reads"
        );
        for target in ADMISSION_TERMINAL_TARGETS {
            assert!(
                ADMISSION_PROGRESS_TARGETS.contains(&target),
                "{target} is suppressed on the fmt layer but never raised to info, so the \
                 suppression is describing a record that was never emitted"
            );
        }
    }

    /// Run one admission record through the stack and return what was written.
    fn captured_with_quiet_admission(quiet: bool) -> String {
        let buffer = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let subscriber = tracing_stack(
            &default_filter_directives("init"),
            None,
            CapturedLog(buffer.clone()),
            false,
            quiet,
        );
        tracing::subscriber::with_default(subscriber, || {
            // The five shapes these two targets actually carry. The two
            // records name a repository and a workspace at info; the staging
            // trio names a count; the refusal is a warn.
            tracing::info!(
                target: "kin_core::git_init",
                repository = "r", workspace = "w", seal = "cca6fdfb",
                "admitted exact Git repository as graph-owned Kin authority"
            );
            tracing::info!(
                target: "kin_core::init",
                repository = "r", workspace = "w", head = "h",
                "initialized unborn kin repository authority"
            );
            tracing::info!(
                target: "kin_core::init", count = 2, parent = "/tmp",
                "kin init kept 2 earlier stages because it could not prove them unused"
            );
            tracing::warn!(
                target: "kin_core::init", path = "/tmp/x", error = "denied",
                "retaining repository stage whose owner lock could not be attempted"
            );
            // A warn carrying the record's own fields. This is what the level
            // test is for, and without this case removing that check changed
            // nothing observable: the mutation survived, and the check read as
            // guarded when it was not.
            tracing::warn!(
                target: "kin_core::init",
                repository = "r", workspace = "w",
                "a warning that happens to name a repository and a workspace"
            );
            tracing::info!(
                target: "kin_core::init", count = 1, bytes = 10, parent = "/tmp",
                "reclaimed stranded repository initialization stages"
            );
        });
        let captured = buffer.lock().expect("log buffer poisoned").clone();
        String::from_utf8(captured).expect("captured log is utf-8")
    }

    /// `kin --version` splits into lines that fit the terminal it is read on.
    ///
    /// The composed single line is eighty-two columns, so the one thing this
    /// command has to say wrapped on an eighty-column terminal. Every fact it
    /// carried is still here, on lines that fit.
    #[test]
    fn the_version_block_keeps_every_fact_and_fits_eighty_columns() {
        let composed = "0.7.2 (dca8e950e99f6a1cb9afe4359611e2da288004f2 detached \
                        2026-09-06T08:24:29Z)";
        let lines = version_lines(composed);
        assert_eq!(lines[0], "", "the mark keeps its top row");
        assert_eq!(lines[1], "kin 0.7.2");
        // The literal, not the constant. Comparing the constant against itself
        // is an assertion that cannot fail, and it would sit here looking like
        // one that could.
        assert_eq!(lines[2], "A graph-native code repository for AI agents");
        assert_eq!(lines[3], "and the people who build with them.");
        assert_eq!(lines[4], "dca8e950e99f6a1cb9afe4359611e2da288004f2");
        assert_eq!(lines[5], "detached 2026-09-06T08:24:29Z");

        // Both a release build, whose branch reads `detached`, and a working
        // branch with a long name. The second is not hypothetical: a build off
        // a branch named like this one reported a version line of ninety
        // columns, which the release-shaped input alone could never have
        // caught.
        let working = "0.7.3 (0123456789abcdef0123456789abcdef01234567 \
                       a-working-branch-with-a-long-name 2026-01-02T03:04:05Z)";
        for version in [composed, working] {
            for paint in [
                kin_cli::mark::Paint::Truecolor,
                kin_cli::mark::Paint::Indexed,
                kin_cli::mark::Paint::Basic,
                kin_cli::mark::Paint::None,
            ] {
                let style = kin_cli::mark::MarkStyle::new(kin_cli::mark::Glyphs::Unicode, paint);
                for line in rendered_version(version, Some(style)).lines() {
                    let width = console::measure_text_width(line);
                    assert!(
                        width <= 80,
                        "the version line {line:?} is {width} columns wide, so it wraps at 80"
                    );
                }
            }
        }
    }

    /// On a terminal too narrow for the definition beside the mark, the
    /// definition is rewrapped at words under the text column, in both the
    /// half-block and the painted-cell marks, and no word is split.
    #[test]
    fn the_version_definition_wraps_at_words_on_a_narrow_terminal() {
        let version = "0.8.2 (0123456789abcdef0123456789abcdef01234567 main 2026-10-11T00:00:00Z)";
        for style in [
            kin_cli::mark::MarkStyle::new(
                kin_cli::mark::Glyphs::Unicode,
                kin_cli::mark::Paint::None,
            ),
            kin_cli::mark::MarkStyle::in_cells(
                kin_cli::mark::Glyphs::Unicode,
                kin_cli::mark::Paint::Indexed,
            ),
        ] {
            let rendered = rendered_version_at(version, Some(style), Some(44));
            let plain: Vec<String> = rendered
                .lines()
                .map(|line| console::strip_ansi_codes(line).into_owned())
                .collect();
            let column = style.text_column();
            let definition: Vec<String> = plain
                .iter()
                .filter(|line| !line.contains("kin 0.8.2"))
                .map(|line| {
                    line.chars()
                        .skip(column)
                        .collect::<String>()
                        .trim_end()
                        .to_string()
                })
                .filter(|text| {
                    !text.is_empty()
                        && text
                            .chars()
                            .all(|c| c.is_ascii_alphabetic() || " -.".contains(c))
                })
                .collect();
            assert_eq!(
                definition.join(" "),
                CATEGORY_ROWS.join(" "),
                "{style:?}: the definition reads whole: {plain:#?}"
            );
            for text in &definition {
                assert!(
                    column + text.len() < 44,
                    "{style:?}: {text:?} overflows 44 columns"
                );
            }
            let wide = rendered_version_at(version, Some(style), Some(100));
            assert!(
                wide.contains(CATEGORY_ROWS[0]),
                "a wide terminal keeps the two rows"
            );
        }
    }

    /// `--help` and `--version` lead with the same line.
    ///
    /// The canon line is written three times: inside [`ORIENTATION`] where a
    /// clap attribute needs a `const`, as [`CATEGORY_LINE`], and as the two
    /// [`CATEGORY_ROWS`] the `--version` block sets beside the mark. `concat!`
    /// takes literals rather than constants, so they cannot be built from one
    /// another without a dependency this crate does not carry. This is what
    /// keeps them from drifting apart, which is the failure a reader would
    /// meet as two different taglines on two commands.
    #[test]
    fn the_help_and_the_version_lead_with_the_same_line() {
        assert!(
            ORIENTATION.starts_with(&format!("{CATEGORY_LINE}\n")),
            "help opens with {:?} and --version with {CATEGORY_LINE:?}",
            ORIENTATION.lines().next().unwrap_or_default()
        );
        assert_eq!(
            CATEGORY_ROWS.join(" "),
            CATEGORY_LINE,
            "the --version rows must read as the help's opening line"
        );
    }

    /// A version string with no build detail still renders.
    ///
    /// `kin_buildinfo::version()` composes the parenthesised half from build
    /// environment variables, and a build that set none of them hands back a
    /// bare number. Splitting on a separator that is not there must not panic
    /// on the one command an install script runs to check the install.
    #[test]
    fn a_version_with_no_build_detail_still_splits() {
        let lines = version_lines("0.7.2");
        assert_eq!(lines[1], "kin 0.7.2");
        assert_eq!(lines[4], "");
        assert_eq!(lines.len(), 5, "an absent build detail adds no second line");
    }

    /// With no terminal, `--version` is the exact line it has always been.
    ///
    /// The README tells a reader to confirm every install path with this
    /// command, and the Homebrew tap, the npm launcher and every install script
    /// parse what it prints. `MarkStyle::for_stdout` answers `None` off a
    /// terminal; this pins what that answer produces, byte for byte, without
    /// depending on whether the harness running it happens to hold one.
    #[test]
    fn a_redirected_version_stays_the_single_line_callers_parse() {
        let composed = "0.7.2 (dca8e950 detached 2026-09-06T08:24:29Z)";
        let rendered = rendered_version(composed, None);
        assert_eq!(
            rendered, "kin 0.7.2 (dca8e950 detached 2026-09-06T08:24:29Z)",
            "a redirected --version must be the line every install path already parses"
        );
        assert_eq!(rendered.lines().count(), 1);
        assert!(!rendered.contains('\u{1b}'), "no escapes reach a pipe");
    }

    /// Each replay that reports onto the phase line says its own count in its
    /// own words, read from the event's `phase` and `unit`, and a phase that
    /// opens again says which pass it is on. The first save of a 5,562-commit
    /// history replays it twice, and both passes said `changes validated`, so
    /// the line read as validation falling from `5376/5562` back to
    /// `2048/5562`.
    #[test]
    fn each_replay_phase_and_pass_reports_its_own_count_in_its_own_words() {
        use super::{admission_progress_line, pass_of, AdmissionProgressFields};
        let fields = |phase: &str, unit: &str, validated: Option<u64>, total: Option<u64>| {
            AdmissionProgressFields {
                message: Some(format!("kindb: validating {phase}")),
                phase: Some(phase.to_string()),
                unit: Some(unit.to_string()),
                validated,
                total,
            }
        };
        let event = |phase: &str, unit: &str, validated: Option<u64>, total: Option<u64>| {
            admission_progress_line(&fields(phase, unit, validated, total), 1)
                .expect("a replay event is worth a line")
        };
        assert_eq!(
            event("history_replay", "changes", Some(5376), Some(5562)),
            "5376/5562 changes replayed"
        );
        assert_eq!(
            event(
                "git_projection_replay",
                "Git commits",
                Some(1024),
                Some(5562)
            ),
            "1024/5562 commit trees checked"
        );
        assert_eq!(
            event(
                "external_object_admission",
                "Git objects",
                Some(9),
                Some(61)
            ),
            "9/61 Git objects admitted"
        );
        // The opening and closing records say the same thing in the same
        // words, and never the raw `kindb:` message.
        assert_eq!(
            event("history_replay", "changes", None, Some(5562)),
            "0/5562 changes replayed"
        );
        assert_eq!(
            event("history_replay", "changes", Some(5562), None),
            "5562 changes replayed"
        );
        let refused = admission_progress_line(
            &AdmissionProgressFields {
                message: Some("kindb: refused after validating 12 changes in 0s".to_string()),
                ..fields("history_replay", "changes", Some(12), None)
            },
            1,
        );
        assert_eq!(
            refused.as_deref(),
            Some("refused after 12 changes replayed")
        );
        // A phase this does not know keeps its own unit.
        assert_eq!(
            event("some_new_replay", "widgets", Some(1), Some(2)),
            "1/2 widgets validated"
        );

        // The first save's sequence, through the same pass counting the layer
        // does: the trees, then the history twice over the same changes.
        let mut passes = std::collections::HashMap::new();
        let mut line = |phase: &str, unit: &str, validated: Option<u64>, total: Option<u64>| {
            let event = fields(phase, unit, validated, total);
            let pass = pass_of(&mut passes, &event);
            admission_progress_line(&event, pass).expect("a replay event is worth a line")
        };
        let sequence = [
            line("git_projection_replay", "Git commits", None, Some(5562)),
            line(
                "git_projection_replay",
                "Git commits",
                Some(4096),
                Some(5562),
            ),
            line("history_replay", "changes", None, Some(5562)),
            line("history_replay", "changes", Some(5376), Some(5562)),
            line("history_replay", "changes", Some(5562), None),
            line("history_replay", "changes", None, Some(5562)),
            line("history_replay", "changes", Some(2048), Some(5562)),
        ];
        assert_eq!(
            sequence,
            [
                "0/5562 commit trees checked",
                "4096/5562 commit trees checked",
                "0/5562 changes replayed",
                "5376/5562 changes replayed",
                "5562 changes replayed",
                "0/5562 changes replayed, pass 2",
                "2048/5562 changes replayed, pass 2",
            ]
        );
    }

    /// With no admission ladder open, a progress event is printed rather than
    /// dropped.
    ///
    /// The sink reports whether a live phase took the line, and that return is
    /// the only thing standing between "routed onto the phase line" and
    /// "silently discarded". A sink that always answered true would make an
    /// admission's instrumentation vanish outside the ladder, and nothing about
    /// the display would look wrong while it did.
    #[test]
    fn a_progress_report_with_no_ladder_open_is_not_swallowed() {
        assert!(
            !kin_core::report_admission_progress("4096/6413 changes validated"),
            "no ladder is open in a unit test, so the sink must decline the line"
        );
    }

    #[test]
    fn an_ordinary_command_denies_the_progress_targets() {
        assert!(
            !survives(installed_filter("locate"), || tracing::info!(
                target: "kin_core::git_init",
                "probe"
            )),
            "a short command must not gain admission output"
        );
        assert!(
            !survives(installed_filter("locate"), || tracing::info!(
                target: "kin_core::init",
                "probe"
            )),
            "a short command must not gain the admission receipt"
        );
    }

    #[test]
    fn the_quiet_floor_holds_for_every_target_not_named() {
        // Why the raised set is an allowlist rather than a crate-wide or global
        // level: unnamed info stays exactly as silent as it was, the embedding
        // hot path most of all, and warnings still reach the user from anywhere.
        assert!(
            !survives(installed_filter("init"), || tracing::info!(
                target: "kin_db::embed",
                "probe"
            )),
            "the embedding hot path must not become visible by default"
        );
        assert!(
            !survives(installed_filter("init"), || tracing::info!(
                target: "reqwest::connect",
                "probe"
            )),
            "dependency info must not become visible by default"
        );
        assert!(
            survives(installed_filter("init"), || tracing::warn!(
                target: "kin_db::embed",
                "probe"
            )),
            "warnings must still reach the user from every target"
        );
    }

    #[test]
    fn a_bare_warn_default_drops_the_admission_targets() {
        // Root cause, and the guard against vacuity. Without it the probes above
        // would pass just as well against a directive that admitted everything,
        // and would say nothing about the behavior they exist to pin.
        assert!(
            !survives(EnvFilter::new("warn"), || tracing::info!(
                target: "kin_core::git_init",
                "probe"
            )),
            "the bare warn default this replaced must be shown to drop admission output"
        );
        assert!(
            !survives(EnvFilter::new("warn"), || tracing::info!(
                target: "kin_core::init",
                "probe"
            )),
            "the bare warn default this replaced must be shown to drop the receipt"
        );
    }

    fn on_cli_test_stack(test: impl FnOnce() + Send + 'static) {
        std::thread::Builder::new()
            .stack_size(16 * 1024 * 1024)
            .spawn(test)
            .expect("spawn CLI test thread")
            .join()
            .expect("CLI test thread must succeed");
    }

    #[test]
    fn cli_definition_is_valid() {
        // clap's debug_assert recurses over the full command tree, which has
        // outgrown the default 2 MiB test-thread stack; give it a dedicated
        // thread with room to validate every subcommand.
        std::thread::Builder::new()
            .stack_size(16 * 1024 * 1024)
            .spawn(|| Cli::command().debug_assert())
            .expect("spawn cli validation thread")
            .join()
            .expect("cli definition validation must succeed");
    }

    /// Public first-run commands are executable contracts, not examples the
    /// parser is allowed to drift away from.
    ///
    /// These are the exact command shapes named by README.md and
    /// docs/quickstart.md. Parsing only `kin setup` or only the leaf actions is
    /// weaker than the contract: the intent value, JSON flag, review range and
    /// default option values are all part of what a new developer copies.
    #[test]
    fn documented_onboarding_commands_parse_to_their_public_contract() {
        on_cli_test_stack(|| {
            let editor = Cli::try_parse_from(["kin", "setup", "--intent", "editor"])
                .expect("the documented editor intent must parse");
            assert!(matches!(
                editor.command,
                Command::Setup {
                    action: None,
                    intent: Some(intent),
                    ..
                } if intent == "editor"
            ));

            let setup_status = Cli::try_parse_from(["kin", "setup", "status", "--json"])
                .expect("the documented setup status command must parse");
            assert!(matches!(
                setup_status.command,
                Command::Setup {
                    action: Some(SetupAction::Status { json: true, .. }),
                    ..
                }
            ));

            let status = Cli::try_parse_from(["kin", "status"])
                .expect("the documented status command must parse");
            assert!(matches!(
                status.command,
                Command::Status {
                    json: false,
                    wait_quiesce: 0,
                    verbose: false
                }
            ));

            let overview = Cli::try_parse_from(["kin", "overview"])
                .expect("the documented overview command must parse");
            assert!(matches!(
                overview.command,
                Command::Overview {
                    compact: false,
                    json: false
                }
            ));

            let review = Cli::try_parse_from(["kin", "review", "shadow", "BASE_BRANCH..HEAD"])
                .expect("the documented shadow-review range must parse");
            assert!(matches!(
                review.command,
                Command::Review {
                    change: None,
                    json: false,
                    entities: None,
                    files: None,
                    changes: None,
                    relations: false,
                    details: false,
                    action: Some(ReviewAction::Shadow {
                        range: Some(range),
                        base: None,
                        head: None,
                        title: None,
                        source_url: None,
                        author: None,
                        json: false,
                    }),
                } if range == "BASE_BRANCH..HEAD"
            ));

            let quickstart = include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../docs/quickstart.md"
            ));
            let readme = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../README.md"));
            for command in [
                "kin setup --intent editor",
                "kin setup status",
                "kin status",
                "kin overview",
                "kin review shadow",
            ] {
                assert!(
                    quickstart.contains(command),
                    "docs/quickstart.md must carry the parser-checked `{command}` surface"
                );
                assert!(
                    readme.contains(command),
                    "README.md must carry the parser-checked `{command}` surface"
                );
            }
            // The README names the page a person reads; the machine-readable
            // report is the quickstart's to document.
            assert!(
                quickstart.contains("kin setup status --json"),
                "docs/quickstart.md must carry the parser-checked `kin setup status --json` surface"
            );
            let setup_status_page = Cli::try_parse_from(["kin", "setup", "status"])
                .expect("the documented setup status page must parse");
            assert!(matches!(
                setup_status_page.command,
                Command::Setup {
                    action: Some(SetupAction::Status { json: false, .. }),
                    ..
                }
            ));
        });
    }

    /// `--shell-only` is the spelling for the shell step alone, `--json` belongs
    /// to it, and the client choice it replaces for that purpose keeps parsing.
    #[test]
    fn setup_shell_only_parses_beside_the_client_choice() {
        on_cli_test_stack(|| {
            let shell_only = Cli::try_parse_from(["kin", "setup", "--shell-only", "--json"])
                .expect("kin setup --shell-only --json must parse");
            assert!(matches!(
                shell_only.command,
                Command::Setup {
                    action: None,
                    shell_only: true,
                    json: true,
                    clients: None,
                    ..
                }
            ));

            // The app's scripted prefix still combines with it.
            Cli::try_parse_from([
                "kin",
                "setup",
                "--no-interactive",
                "--intent",
                "agent",
                "--skip-mcp-check",
                "--shell-only",
            ])
            .expect("the scripted setup prefix must combine with --shell-only");

            for refused in [
                vec!["kin", "setup", "--shell-only", "--clients", "cursor"],
                vec!["kin", "setup", "--shell-only", "--prompt-hook"],
                vec!["kin", "setup", "--json"],
            ] {
                assert!(
                    Cli::try_parse_from(&refused).is_err(),
                    "{refused:?} must be refused"
                );
            }

            let no_clients = Cli::try_parse_from(["kin", "setup", "--clients", ""])
                .expect("kin setup --clients \"\" must keep parsing");
            assert!(matches!(
                no_clients.command,
                Command::Setup {
                    shell_only: false,
                    clients: Some(_),
                    ..
                }
            ));

            let clients = Cli::try_parse_from(["kin", "setup", "clients", "--json"])
                .expect("kin setup clients --json must keep parsing");
            assert!(matches!(
                clients.command,
                Command::Setup {
                    action: Some(SetupAction::Clients { json: true }),
                    json: false,
                    ..
                }
            ));
        });
    }

    /// The formerly documented subcommand spelling stays rejected and the
    /// production signpost points at the canonical option spelling instead of
    /// clap's edit-distance suggestion of the unrelated `doctor` subcommand.
    #[test]
    fn stale_setup_editor_form_is_rejected_with_the_canonical_replacement() {
        on_cli_test_stack(|| {
            let error = Cli::try_parse_from(["kin", "setup", "editor"])
                .err()
                .expect("setup intents must not become compatibility subcommands");
            assert_eq!(error.kind(), clap::error::ErrorKind::InvalidSubcommand);

            let args = vec!["setup".to_string(), "editor".to_string()];
            let (path, guidance) = retired_command_signpost(&args)
                .expect("production must replace clap's misleading suggestion");
            assert_eq!(path, "setup editor");
            assert!(
                guidance.contains("`kin setup --intent editor`"),
                "the rejection must name the command a developer can run: {guidance}"
            );
        });
    }

    #[test]
    fn setup_uninstall_all_is_explicit_and_composable_with_safety_flags() {
        on_cli_test_stack(|| {
            let cli = Cli::try_parse_from([
                "kin",
                "setup",
                "uninstall",
                "--all",
                "--dry-run",
                "--force",
                "--json",
            ])
            .expect("the explicit full-uninstall surface must parse");
            assert!(matches!(
                cli.command,
                Command::Setup {
                    action: Some(SetupAction::Uninstall {
                        all: true,
                        dry_run: true,
                        force: true,
                        json: true,
                    }),
                    ..
                }
            ));
        });
    }

    #[test]
    fn workspace_is_not_a_cli_surface() {
        on_cli_test_stack(|| {
            for args in [
                vec!["kin", "workspace"],
                vec!["kin", "workspace", "list"],
                vec!["kin", "workspace", "create", "demo"],
                vec!["kin", "workspace", "switch", "demo"],
                vec!["kin", "workspace", "delete", "demo"],
                vec!["kin", "workspace", "rename", "demo", "renamed"],
            ] {
                assert!(
                    Cli::try_parse_from(args).is_err(),
                    "the descriptor-only workspace command must not be parseable"
                );
            }

            let mut command = Cli::command();
            let help = command.render_long_help().to_string();
            for supported in [
                "Run a command in an exact graph-derived session workspace",
                "Launch an editor over an exact graph-derived session workspace",
                "Launch an assistant in an exact graph-derived session workspace",
                "Open a shell in an exact graph-derived session workspace",
            ] {
                assert!(
                    help.contains(supported),
                    "supported graph-backed session surface missing from help: {supported}"
                );
            }
        });
    }

    #[test]
    fn git_interop_exposes_only_exact_export() {
        on_cli_test_stack(|| {
            for args in [
                &["kin", "import", "https://example.com/repo.git"][..],
                &["kin", "git", "import"][..],
                &["kin", "git", "sync"][..],
                &["kin", "git", "sync", "--in-place"][..],
            ] {
                assert!(
                    Cli::try_parse_from(args).is_err(),
                    "removed Git compatibility command must not be parseable: {args:?}"
                );
            }

            let cli = Cli::try_parse_from(["kin", "git", "export", "--output", "../export.git"])
                .expect("exact Git export remains the explicit interoperability surface");
            assert!(matches!(
                cli.command,
                Command::Git {
                    action: GitAction::Export { output }
                } if output.as_path() == std::path::Path::new("../export.git")
            ));
            assert!(Cli::try_parse_from(["kin", "git", "export", "--in-place"]).is_err());
        });
    }

    /// A work item names the changes to roll back, so requiring a change on the
    /// same invocation made the flag unreachable: the caller had to already know
    /// the answer the flag exists to compute.
    #[test]
    fn work_item_rollback_parses_without_a_change_argument() {
        on_cli_test_stack(|| {
            let cli = Cli::try_parse_from(["kin", "rollback", "--feature", "some-work-id"])
                .expect("naming a work item must be a complete rollback invocation");
            assert!(matches!(
                cli.command,
                Command::Rollback {
                    change_id: None,
                    feature: Some(ref work_id),
                    ..
                } if work_id == "some-work-id"
            ));

            let cli = Cli::try_parse_from(["kin", "rollback", "abc123"])
                .expect("naming a change stays a complete rollback invocation");
            assert!(matches!(
                cli.command,
                Command::Rollback {
                    change_id: Some(ref change),
                    feature: None,
                    ..
                } if change == "abc123"
            ));
        });
    }

    /// `--discard-later` is off by default, and naming it does not need a
    /// change id in the same slot the flag itself occupies.
    #[test]
    fn rollback_discard_later_flag_parses_and_defaults_off() {
        on_cli_test_stack(|| {
            let cli = Cli::try_parse_from(["kin", "rollback", "abc123"])
                .expect("a bare change id stays a complete rollback invocation");
            assert!(matches!(
                cli.command,
                Command::Rollback {
                    discard_later: false,
                    ..
                }
            ));

            let cli = Cli::try_parse_from(["kin", "rollback", "abc123", "--discard-later"])
                .expect("the flag stays a complete rollback invocation");
            assert!(matches!(
                cli.command,
                Command::Rollback {
                    discard_later: true,
                    ..
                }
            ));
        });
    }

    /// `revert` used to be a `rollback` alias that suggested a single-change
    /// undo. It is its own command now, so a Git habit reaches a refusal
    /// that names rollback instead of running rollback's destructive path
    /// under a misleading name.
    #[test]
    fn revert_is_its_own_command_not_a_rollback_alias() {
        on_cli_test_stack(|| {
            let cli = Cli::try_parse_from(["kin", "revert", "abc123"])
                .expect("revert parses on its own so it can explain itself");
            assert!(
                matches!(cli.command, Command::Revert { change_id: Some(ref id) } if id == "abc123")
            );
        });
    }

    /// Not supplying the thing to operate on is one mistake, and it produced
    /// two contracts: 45 leaves exited 2 with a usage block through clap, and a
    /// handful exited 1 with a bare line. A caller keying on exit 2 for "I
    /// called this wrong" misclassified the second set.
    ///
    /// These four moved to clap. `kin locate` and `kin scope` deliberately did
    /// not: `locate --next` takes its query from a persisted cursor and `scope`
    /// reads KIN_SESSION_ID, so a parse-time requirement would reject
    /// invocations that work. Their argument is not optional to the command,
    /// only to the command line, which is not something clap can express.
    #[test]
    fn a_missing_required_argument_is_a_usage_error_not_a_runtime_one() {
        on_cli_test_stack(|| {
            for argv in [
                &["kin", "refs"][..],
                &["kin", "rollback"][..],
                &["kin", "resolve"][..],
                &["kin", "review", "shadow"][..],
            ] {
                let error = Cli::try_parse_from(argv)
                    .err()
                    .unwrap_or_else(|| panic!("{argv:?} must not parse"));
                assert_eq!(
                    error.exit_code(),
                    2,
                    "{argv:?} must fail the way every other misuse does"
                );
                let rendered = error.to_string();
                assert!(
                    rendered.contains("Usage:"),
                    "{argv:?} must print usage: {rendered}"
                );
            }

            // Every shape that did work still parses. A required-group that
            // over-reaches would take these with it, and each is the reason its
            // command's requirement is conditional rather than plain.
            for argv in [
                &["kin", "refs", "Foo"][..],
                &["kin", "refs", "--bulk-json", "--entities", "a,b"][..],
                &["kin", "resolve", "--abort"][..],
                &["kin", "resolve", "--all-ours", "--json"][..],
                &["kin", "resolve", "--ours", "x", "--theirs", "y"][..],
                &["kin", "review", "shadow", "main..head"][..],
                &["kin", "review", "shadow", "--base", "a", "--head", "b"][..],
                // Untouched on purpose: both take their input from somewhere
                // the command line cannot see.
                &["kin", "locate", "--next"][..],
                &["kin", "scope"][..],
            ] {
                assert!(
                    Cli::try_parse_from(argv).is_ok(),
                    "{argv:?} must stay a complete invocation"
                );
            }

            // Half-supplied pairs are refused at parse time too, rather than
            // reaching a command body that has to re-derive what is missing.
            for argv in [
                &["kin", "refs", "--bulk-json"][..],
                &["kin", "review", "shadow", "--base", "a"][..],
                &["kin", "review", "shadow", "--head", "b"][..],
                &["kin", "rollback", "abc123", "--feature", "w-1"][..],
            ] {
                let error = Cli::try_parse_from(argv)
                    .err()
                    .unwrap_or_else(|| panic!("{argv:?} must not parse"));
                assert_eq!(error.exit_code(), 2, "{argv:?} must be a usage error");
            }
        });
    }

    #[test]
    fn reference_override_view_is_explicit_and_requires_json() {
        on_cli_test_stack(|| {
            let cli = Cli::try_parse_from([
                "kin",
                "refs",
                "00000000-0000-4000-8000-000000000001",
                "--view",
                "overrides",
                "--json",
            ])
            .unwrap();
            assert!(
                matches!(cli.command, Command::Refs { json: true, view: Some(value), .. }
                if value == "overrides")
            );
            for argv in [
                vec!["kin", "refs", "method", "--view", "overrides"],
                vec!["kin", "refs", "method", "--json", "--view", "subclasses"],
                vec![
                    "kin",
                    "refs",
                    "method",
                    "--json",
                    "--view",
                    "overrides",
                    "--all",
                ],
            ] {
                assert!(Cli::try_parse_from(argv).is_err());
            }
        });
    }

    #[test]
    fn reference_page_flags_require_json_and_preserve_the_requested_page() {
        on_cli_test_stack(|| {
            let cli = Cli::try_parse_from([
                "kin",
                "refs",
                "send",
                "--json",
                "--max-chars",
                "4000",
                "--cursor",
                "held",
            ])
            .unwrap();
            assert!(
                matches!(cli.command, Command::Refs { json:true, max_chars:Some(4000), cursor:Some(value), .. } if value == "held")
            );
            for argv in [
                vec!["kin", "refs", "send", "--cursor", "held"],
                vec!["kin", "refs", "send", "--max-chars", "4000"],
                vec!["kin", "refs", "send", "--json", "--all", "--cursor", "held"],
            ] {
                assert_eq!(Cli::try_parse_from(argv).err().unwrap().exit_code(), 2);
            }
        });
    }

    #[test]
    fn review_details_expands_human_output_without_changing_json_mode() {
        on_cli_test_stack(|| {
            for argv in [
                vec!["kin", "review", "--details"],
                vec!["kin", "review", "--details", "--json"],
                vec!["kin", "review", "--details", "--relations"],
            ] {
                let cli = Cli::try_parse_from(argv).expect("review detail options must parse");
                assert!(matches!(cli.command, Command::Review { details: true, .. }));
            }
            let cli = Cli::try_parse_from(["kin", "review"]).unwrap();
            assert!(matches!(
                cli.command,
                Command::Review {
                    details: false,
                    relations: false,
                    ..
                }
            ));
        });
    }

    /// `--relations` lists what the text review only counts. The JSON answer
    /// has a fixed contract that does not carry the list, so asking for both is
    /// a usage error rather than a flag that silently does nothing.
    #[test]
    fn review_relations_lists_in_text_and_is_refused_beside_json() {
        on_cli_test_stack(|| {
            let cli = Cli::try_parse_from(["kin", "review", "--relations"])
                .expect("`kin review --relations` must parse");
            assert!(matches!(
                cli.command,
                Command::Review {
                    relations: true,
                    json: false,
                    action: None,
                    ..
                }
            ));

            let plain = Cli::try_parse_from(["kin", "review"]).expect("`kin review` must parse");
            assert!(matches!(
                plain.command,
                Command::Review {
                    relations: false,
                    ..
                }
            ));

            let error = Cli::try_parse_from(["kin", "review", "--relations", "--json"])
                .err()
                .expect("`--relations` beside `--json` must not parse");
            assert_eq!(error.exit_code(), 2, "{error}");
        });
    }

    #[test]
    fn file_resolution_accepts_repeated_pairs_and_rejects_missing_bodies() {
        on_cli_test_stack(|| {
            let cli = Cli::try_parse_from([
                "kin",
                "resolve",
                "--file",
                "src/a=b.rs",
                "/tmp/a=b.rs",
                "--file",
                "notes.txt",
                "/tmp/notes.txt",
                "--json",
            ])
            .expect("explicit file resolutions are a complete invocation");
            match cli.command {
                Command::Resolve { file, json, .. } => {
                    assert_eq!(
                        file,
                        ["src/a=b.rs", "/tmp/a=b.rs", "notes.txt", "/tmp/notes.txt"]
                    );
                    assert!(json);
                }
                _ => panic!("expected resolve"),
            }
            for argv in [
                &["kin", "resolve", "--file"][..],
                &["kin", "resolve", "--file", "src/code.rs"][..],
                &["kin", "resolve", "--file", "src/code.rs", "--json"][..],
            ] {
                let error = Cli::try_parse_from(argv).err().expect("a body is required");
                assert_eq!(error.exit_code(), 2);
            }
        });
    }

    /// FIR-2938. The server has read `provider` on `/auth/login` for as long
    /// as it has had more than one, and the web page offers both, but the CLI
    /// sent nothing, so a terminal user could not reach the GitHub sign-in at
    /// all. Three properties, and the third is the one that makes the flag
    /// safe to add: nobody who types what they typed yesterday moves.
    #[test]
    fn auth_login_can_ask_for_a_provider_and_still_defaults_to_google() {
        on_cli_test_stack(|| {
            let chosen = Cli::try_parse_from(["kin", "auth", "login", "--provider", "github"])
                .expect("a provider the CLI can send must parse");
            assert!(
                matches!(
                    chosen.command,
                    Command::Auth {
                        action: AuthAction::Login {
                            provider: commands::auth::AuthProvider::Github,
                            ..
                        }
                    }
                ),
                "--provider github must reach the login command"
            );

            let defaulted = Cli::try_parse_from(["kin", "auth", "login"])
                .expect("the invocation that shipped must keep parsing");
            assert!(
                matches!(
                    defaulted.command,
                    Command::Auth {
                        action: AuthAction::Login {
                            provider: commands::auth::AuthProvider::Google,
                            ..
                        }
                    }
                ),
                "a login that names no provider must stay the Google login it was"
            );

            // A name the CLI cannot send is refused here rather than at the
            // sign-in page, where the only report is a redirect carrying
            // authError=provider-unavailable that no terminal ever shows.
            let error = Cli::try_parse_from(["kin", "auth", "login", "--provider", "gitlab"])
                .err()
                .expect("a provider the CLI cannot send must not parse");
            assert_eq!(error.exit_code(), 2, "an unknown provider is a usage error");
            let rendered = error.to_string();
            for expected in ["google", "github"] {
                assert!(
                    rendered.contains(expected),
                    "the refusal must list what can be asked for: {rendered}"
                );
            }
        });
    }

    #[test]
    fn every_retired_command_path_names_its_replacement() {
        on_cli_test_stack(|| {
            for (path, _) in RETIRED_COMMANDS {
                let mut argv = vec!["kin"];
                argv.extend_from_slice(path);
                assert!(
                    Cli::try_parse_from(&argv).is_err(),
                    "a retired command path must stay unparseable: {path:?}"
                );

                let typed: Vec<String> = path.iter().map(|token| token.to_string()).collect();
                let (matched, guidance) = retired_command_signpost(&typed)
                    .unwrap_or_else(|| panic!("retired path {path:?} must be signposted"));
                assert_eq!(matched, path.join(" "));
                assert!(
                    guidance.contains("kin "),
                    "guidance for {path:?} must name a replacement command: {guidance}"
                );
            }
        });
    }

    #[test]
    fn retired_signposting_prefers_the_longest_path_and_ignores_live_commands() {
        on_cli_test_stack(|| {
            let git_import = ["git".to_string(), "import".to_string()];
            let (matched, guidance) =
                retired_command_signpost(&git_import).expect("`kin git import` is retired");
            assert_eq!(matched, "git import");
            assert!(
                guidance.contains("git export"),
                "the Git path must name export as the remaining direction: {guidance}"
            );

            let bare_import = [
                "import".to_string(),
                "https://example.com/r.git".to_string(),
            ];
            let (matched, _) =
                retired_command_signpost(&bare_import).expect("`kin import` is retired");
            assert_eq!(matched, "import");

            // Leading global flags precede the command path a caller typed.
            let flagged = ["--profile-summary".to_string(), "gc".to_string()];
            assert_eq!(
                retired_command_signpost(&flagged).map(|(matched, _)| matched),
                Some("gc".to_string())
            );

            // A flag that takes a value consumes the token after it, so that
            // token names a file and not a command. The retired name here is
            // the caller's profile path, and `frobnicate` is what they typed.
            let value_named_after_a_retired_command = [
                "--profile-out".to_string(),
                "run".to_string(),
                "frobnicate".to_string(),
            ];
            assert!(
                retired_command_signpost(&value_named_after_a_retired_command).is_none(),
                "a flag value must not be read as a command a caller typed"
            );

            // The command path still resolves past a consumed value, in both
            // the separated and the inline spelling.
            for spelling in [
                vec![
                    "--profile-out".to_string(),
                    "/tmp/profile.json".to_string(),
                    "import".to_string(),
                ],
                vec![
                    "--profile-out=/tmp/profile.json".to_string(),
                    "import".to_string(),
                ],
            ] {
                assert_eq!(
                    retired_command_signpost(&spelling).map(|(matched, _)| matched),
                    Some("import".to_string()),
                    "a retired command behind a flag value must still be signposted: {spelling:?}"
                );
            }

            // A live command, and an unknown name with no replacement to name,
            // both fall through to clap's own reporting.
            for live in [
                vec!["status".to_string()],
                vec!["git".to_string(), "export".to_string()],
                vec!["cache".to_string(), "gc".to_string()],
                vec!["frobnicate".to_string()],
            ] {
                assert!(
                    retired_command_signpost(&live).is_none(),
                    "must not signpost {live:?}"
                );
            }
        });
    }

    #[test]
    fn help_orients_a_caller_to_the_everyday_path() {
        on_cli_test_stack(|| {
            let mut command = Cli::command();
            let help = command.render_long_help().to_string();
            for anchor in [
                "Start here:",
                "kin init",
                "kin status",
                "kin commit",
                "kin capabilities",
            ] {
                assert!(
                    help.contains(anchor),
                    "help must orient a caller with {anchor:?}"
                );
            }
        });
    }

    /// The orientation has to come before the command list, not merely exist.
    ///
    /// It existed for as long as the list did, at the bottom, under eighty
    /// subcommand lines. On the eighty-column terminal the walk was measured
    /// on, `kin` printed commands from line five to line ninety-two and the
    /// orientation at line ninety-four, so the only lines that tell a reader
    /// what to type first were the ones a fifty-row terminal had already
    /// scrolled away. Containment alone passes on both layouts, which is why
    /// this asserts the order.
    #[test]
    fn the_orientation_precedes_the_command_list() {
        on_cli_test_stack(|| {
            let mut command = Cli::command();
            let help = command.render_long_help().to_string();
            let orientation = help
                .find("Start here:")
                .expect("help carries the orientation");
            let commands = help
                .find("Get started:")
                .expect("help carries the command list");
            assert!(
                orientation < commands,
                "the orientation must precede the task-grouped command index"
            );
        });
    }

    /// Help leads with a locked line, not with a retired construction.
    ///
    /// `about = "Kin semantic VCS"` was the first line `kin --help` printed.
    /// The brand book's canon page lists "semantic version control" under
    /// "Retired - do not ship", and carries the Category line asserted here for
    /// exactly this job. A surface needing a headline takes a locked line; it
    /// does not get a new one.
    #[test]
    fn help_leads_with_the_canon_category_line() {
        on_cli_test_stack(|| {
            let mut command = Cli::command();
            let help = command.render_long_help().to_string();
            assert!(
                help.starts_with(
                    "A graph-native code repository for AI agents and the people who build \
                     with them."
                ),
                "help opens with {:?}",
                help.lines().next().unwrap_or_default()
            );
            for retired in ["semantic VCS", "semantic version control"] {
                assert!(
                    !help.contains(retired),
                    "help carries the retired construction {retired:?}"
                );
            }
        });
    }

    /// The legend teaches a marker, so it may only appear while a marker does.
    ///
    /// Asserting the bare string was what let it outlive the thing it
    /// describes: every gate closed, every marker came off, and the sentence
    /// stayed, telling a caller to look for a signal that no command carried.
    #[test]
    fn the_open_gate_legend_appears_exactly_when_a_gate_does() {
        on_cli_test_stack(|| {
            let mut command = Cli::command();
            let help = command.render_long_help().to_string();
            let inventory =
                commands::capabilities::inventory().expect("capability inventory must parse");
            let gated = inventory.commands.iter().any(|capability| {
                capability.status == commands::capabilities::CapabilityStatus::OpenGate
            });

            assert_eq!(
                help.contains(OPEN_GATE_MARKER),
                gated,
                "the legend must track the inventory, which reports gated={gated}"
            );

            // The conditional is what is under test, so exercise both arms
            // rather than only the one this inventory happens to select.
            assert!(after_help().contains("kin capabilities"));
            assert!(!AFTER_HELP.contains(OPEN_GATE_MARKER));
            assert!(OPEN_GATE_LEGEND.contains(OPEN_GATE_MARKER));
            assert!(
                format!("{AFTER_HELP}{OPEN_GATE_LEGEND}").contains(OPEN_GATE_MARKER),
                "the gated arm must carry the legend it exists to add"
            );
        });
    }

    const OPEN_GATE_MARKER: &str = "[OPEN GATE]";

    /// Every help entry carrying the open-gate marker, keyed by the command path
    /// a caller types to reach it.
    ///
    /// Flags are qualified by the command that owns them, matching how the
    /// capability inventory names `doctor --heal`, so a marker on a flag and a
    /// marker on a subcommand are comparable against the same inventory.
    fn open_gate_marked_entries(
        command: &clap::Command,
        prefix: &str,
        marked: &mut std::collections::BTreeSet<String>,
    ) {
        let qualify = |entry: &str| {
            if prefix.is_empty() {
                entry.to_string()
            } else {
                format!("{prefix} {entry}")
            }
        };
        for arg in command.get_arguments() {
            let Some(long) = arg.get_long() else { continue };
            let carries = arg
                .get_help()
                .or_else(|| arg.get_long_help())
                .is_some_and(|help| help.to_string().contains(OPEN_GATE_MARKER));
            if carries {
                marked.insert(qualify(&format!("--{long}")));
            }
        }
        for subcommand in command.get_subcommands() {
            let path = qualify(subcommand.get_name());
            let carries = subcommand
                .get_about()
                .or_else(|| subcommand.get_long_about())
                .is_some_and(|about| about.to_string().contains(OPEN_GATE_MARKER));
            if carries {
                marked.insert(path.clone());
            }
            open_gate_marked_entries(subcommand, &path, marked);
        }
    }

    #[test]
    fn open_gate_markers_name_exactly_the_fail_closed_commands() {
        on_cli_test_stack(|| {
            // The after-help sentence promises that a marked command is
            // fail-closed on repository-v6, which makes the marker a contract
            // against the capability inventory rather than decoration. A gate
            // that opens without its marker coming off steers a caller away
            // from a command that now runs, which is the misdirection the
            // marker exists to prevent.
            let mut marked = std::collections::BTreeSet::new();
            open_gate_marked_entries(&Cli::command(), "", &mut marked);

            let inventory =
                commands::capabilities::inventory().expect("capability inventory must parse");
            let fail_closed: std::collections::BTreeSet<String> = inventory
                .commands
                .iter()
                .filter(|capability| {
                    capability.status == commands::capabilities::CapabilityStatus::OpenGate
                })
                .map(|capability| capability.command.clone())
                .collect();

            assert_eq!(
                marked, fail_closed,
                "help must mark exactly the commands `kin capabilities` reports as open gates"
            );

            // The marker has to survive rendering, since the rendered surface
            // is what the after-help sentence governs.
            let mut command = Cli::command();
            let rendered = command.render_long_help().to_string();
            let flattened = rendered.split_whitespace().collect::<Vec<_>>().join(" ");
            for subcommand in Cli::command().get_subcommands() {
                let name = subcommand.get_name();
                assert_eq!(
                    flattened.contains(&format!("{name} {OPEN_GATE_MARKER}")),
                    fail_closed.contains(name),
                    "rendered help must mark `kin {name}` exactly when the inventory calls it \
                     an open gate"
                );
            }
        });
    }

    #[test]
    fn commit_amend_preserves_message_unless_overridden() {
        on_cli_test_stack(|| {
            assert!(Cli::try_parse_from(["kin", "commit"]).is_err());
            let cli = Cli::try_parse_from(["kin", "commit", "--amend"]).unwrap();
            assert!(matches!(
                cli.command,
                Command::Commit {
                    amend: true,
                    message: None,
                    ..
                }
            ));
            let cli =
                Cli::try_parse_from(["kin", "commit", "--amend", "-m", "correction"]).unwrap();
            assert!(
                matches!(cli.command, Command::Commit { amend: true, message: Some(message), .. } if message == "correction")
            );
            let cli = Cli::try_parse_from(["kin", "commit", "-m", "new change"]).unwrap();
            assert!(matches!(
                cli.command,
                Command::Commit {
                    amend: false,
                    message: Some(_),
                    ..
                }
            ));
        });
    }

    /// `--entity` repeats, never rides with an amend, and makes a checkout
    /// restore one entity instead of a path.
    #[test]
    fn commit_and_checkout_take_entities() {
        on_cli_test_stack(|| {
            let cli = Cli::try_parse_from([
                "kin", "commit", "-m", "keep two", "--entity", "a", "--entity", "b",
            ])
            .unwrap();
            assert!(
                matches!(&cli.command, Command::Commit { entities, amend: false, .. } if entities == &["a", "b"])
            );
            assert!(
                Cli::try_parse_from(["kin", "commit", "--amend", "--entity", "a"]).is_err(),
                "an amend replaces a whole change and chooses no entities"
            );
            let cli = Cli::try_parse_from([
                "kin",
                "checkout",
                "--entity",
                "a",
                "--change",
                "c",
                "--expected",
                "f",
            ])
            .unwrap();
            assert!(matches!(
                &cli.command,
                Command::Checkout { entity: Some(entity), expected: Some(_), path: None, .. } if entity == "a"
            ));
            assert!(Cli::try_parse_from(["kin", "checkout", "src", "--entity", "a"]).is_err());
            assert!(Cli::try_parse_from(["kin", "checkout", "--expected", "f"]).is_err());
        });
    }

    /// A stranger forms the write-through assumption at help, not at commit.
    ///
    /// The npm0549 green stranger read `kin commit --help`, took `kin commit`
    /// for a git commit, and only met the correction in the line a commit
    /// prints, after they had already reasoned about the repository as if git
    /// held the change (FIR-2627). The commit-time line was the only surface
    /// carrying the fact, so help now carries the same sentence, spliced from
    /// the same macro. This asserts on the RENDERED long help rather than the
    /// const, because a const nothing renders is a fact nobody reads.
    #[test]
    fn commit_help_says_the_commit_lands_in_kin_authority_and_git_stays_dirty() {
        on_cli_test_stack(|| {
            let command = Cli::command();
            let mut commit = command
                .get_subcommands()
                .find(|subcommand| subcommand.get_name() == "commit")
                .expect("kin commit must exist")
                .clone();
            let rendered = commit.render_long_help().to_string();
            let flattened = rendered.split_whitespace().collect::<Vec<_>>().join(" ");

            // Positive control. A help surface that failed to render at all
            // would satisfy every `!contains` below, so the check first proves
            // it is reading the help it means to read, on a line that predates
            // this change.
            assert!(
                flattened.contains("Commit message"),
                "the control line must be present, or this test is asserting about nothing: \
                 {flattened}"
            );

            // The sentence itself, taken from the constant the commit prints
            // rather than restated, so a reworded commit line fails here.
            let note = commands::commit_progress::AUTHORITY_NOT_GIT_NOTE;
            let flat_note = note.split_whitespace().collect::<Vec<_>>().join(" ");
            assert!(
                flattened.contains(&flat_note),
                "help must carry the commit-time authority line verbatim: {flattened}"
            );
            assert!(
                flattened.contains("not in Git")
                    && flattened.contains("git status")
                    && flattened.contains("kin eject"),
                "help must name the authority, the dirty tree and the way back: {flattened}"
            );

            // Falsification. A phrase that was never written must be absent, so
            // a `contains` that passes on anything cannot be what made the
            // assertions above green.
            assert!(
                !flattened.contains("writes through to git"),
                "the check must be able to fail: {flattened}"
            );
        });
    }

    /// The two surfaces are one string, not two that agree today.
    ///
    /// `concat!` folds the same macro expansion into both, so this cannot fail
    /// while the code compiles. It is here for the reader who edits one of them
    /// and wants to know what enforces the other.
    #[test]
    fn the_commit_help_paragraph_ends_in_the_line_a_commit_prints() {
        assert!(
            commands::commit_progress::COMMIT_LONG_ABOUT
                .ends_with(commands::commit_progress::AUTHORITY_NOT_GIT_NOTE),
            "the help paragraph must end in the commit-time line"
        );
        assert!(
            !commands::commit_progress::AUTHORITY_NOT_GIT_NOTE.is_empty(),
            "an empty note would make the assertion above pass on anything"
        );
    }

    /// A server reached through `docker exec` or over a remote boundary can
    /// never bind the host paths its client announces, so it holds the
    /// repository it was started with instead. An operator registering that
    /// shape has to be able to learn it from the command itself rather than from
    /// a refusal mid-session, which is why the behavior is documented where the
    /// registration is written.
    #[test]
    fn mcp_start_help_documents_that_a_bound_server_holds_unresolvable_client_roots() {
        on_cli_test_stack(|| {
            let command = Cli::command();
            let start = command
                .get_subcommands()
                .find(|subcommand| subcommand.get_name() == "mcp")
                .expect("kin mcp must exist")
                .get_subcommands()
                .find(|subcommand| subcommand.get_name() == "start")
                .expect("kin mcp start must exist")
                .clone();
            let help = start
                .get_long_about()
                .or_else(|| start.get_about())
                .expect("kin mcp start must carry help text")
                .to_string();
            let flattened = help.split_whitespace().collect::<Vec<_>>().join(" ");

            assert!(
                flattened.contains("ignores client workspace roots it cannot resolve"),
                "help must state that a bound server ignores roots it cannot resolve: {flattened}"
            );
            assert!(
                flattened.contains("docker exec"),
                "help must name the container registration this supports: {flattened}"
            );

            // Falsification: a phrase that was never written must be absent, so
            // a `contains` that passes on anything cannot be what made the two
            // assertions above green.
            assert!(
                !flattened.contains("ignores client workspace roots entirely"),
                "the check must be able to fail: {flattened}"
            );
        });
    }

    /// The registration this help documents has to be one an operator can paste
    /// into a stock image.
    ///
    /// It was not. `docker exec` with a bare command resolves it against the
    /// image's own `ENV PATH`, which carries neither `~/.kin/bin` nor an npm
    /// user prefix, so the documented shape died with `exec: "kin": executable
    /// file not found in $PATH` on npm-served 0.5.45 in a Debian 12 container
    /// (FIR-2553). The error names Docker, so a reader has no way back to the
    /// cause from the failure. Documenting a command that cannot run is worse
    /// than documenting none, which is why the absolute-path form is asserted
    /// here rather than left to review.
    #[test]
    fn mcp_start_help_registers_an_absolute_binary_path_for_docker_exec() {
        on_cli_test_stack(|| {
            let command = Cli::command();
            let start = command
                .get_subcommands()
                .find(|subcommand| subcommand.get_name() == "mcp")
                .expect("kin mcp must exist")
                .get_subcommands()
                .find(|subcommand| subcommand.get_name() == "start")
                .expect("kin mcp start must exist")
                .clone();
            let help = start
                .get_long_about()
                .or_else(|| start.get_about())
                .expect("kin mcp start must carry help text")
                .to_string();
            let flattened = help.split_whitespace().collect::<Vec<_>>().join(" ");

            assert!(
                flattened
                    .contains("docker exec -i -w /work/repo <container> /absolute/path/to/kin"),
                "help must register an absolute path to the binary, since a bare `kin` is not on \
                 the PATH docker exec uses: {flattened}"
            );
            assert!(
                flattened.contains("executable file not found in $PATH"),
                "help must quote the failure a bare registration produces, so a reader who hits \
                 it can find their way back: {flattened}"
            );
            assert!(
                flattened.contains("~/.kin/bin/kin"),
                "help must say where the managed binary is, or the absolute path it asks for is \
                 one the reader still has to hunt for: {flattened}"
            );

            // The npm reinstall that looks like the obvious fix and is not: an
            // image with one shared HOME sends root's install back into the
            // user prefix while reporting success.
            assert!(
                flattened.contains("npm install -g --prefix /usr/local @kinlab/kin"),
                "help must name the prefix-explicit install, since the plain root reinstall \
                 reports success and lands nothing on the default PATH: {flattened}"
            );

            // Falsification: a phrase that was never written must be absent, so
            // a `contains` that matches anything cannot be what made the
            // assertions above green.
            assert!(
                !flattened.contains("docker exec resolves the managed binary automatically"),
                "the check must be able to fail: {flattened}"
            );
        });
    }

    #[test]
    fn update_release_expectation_tuple_is_complete_and_works_with_check_only() {
        on_cli_test_stack(|| {
            let digest = "a".repeat(64);
            let uppercase_digest = "C".repeat(64);
            for args in [
                vec!["kin", "update", "--expect-version", "0.2.22"],
                vec![
                    "kin",
                    "update",
                    "--expect-sha",
                    "0123456789abcdef0123456789abcdef01234567",
                ],
                vec!["kin", "update", "--expect-archive-sha256", digest.as_str()],
                vec![
                    "kin",
                    "update",
                    "--expect-version",
                    "0.2.22",
                    "--expect-sha",
                    "0123456789abcdef0123456789abcdef01234567",
                ],
            ] {
                assert!(Cli::try_parse_from(args).is_err());
            }

            let cli = Cli::try_parse_from([
                "kin",
                "update",
                "--channel",
                "stable",
                "--expect-version",
                "0.2.22",
                "--expect-sha",
                "0123456789ABCDEF0123456789ABCDEF01234567",
                "--expect-archive-sha256",
                uppercase_digest.as_str(),
                "--check-only",
                "--json",
            ])
            .expect("complete release expectation must be accepted for check-only automation");
            match cli.command {
                Command::Update {
                    expect_version,
                    expect_sha,
                    expect_archive_sha256,
                    check_only,
                    json,
                    ..
                } => {
                    assert_eq!(expect_version.unwrap(), semver::Version::new(0, 2, 22));
                    assert_eq!(
                        expect_sha.unwrap(),
                        "0123456789abcdef0123456789abcdef01234567"
                    );
                    assert_eq!(expect_archive_sha256.unwrap(), "c".repeat(64));
                    assert!(check_only);
                    assert!(json);
                }
                _ => panic!("expected update command"),
            }
        });
    }

    #[test]
    fn restart_ack_rejects_release_pins_at_argument_parsing() {
        on_cli_test_stack(|| {
            let digest = "a".repeat(64);
            assert!(Cli::try_parse_from([
                "kin",
                "update",
                "--expect-version",
                "0.2.22",
                "--expect-sha",
                "0123456789abcdef0123456789abcdef01234567",
                "--expect-archive-sha256",
                digest.as_str(),
                "--ack-restart",
            ])
            .is_err());
        });
    }

    #[test]
    fn exec_shell_mode_is_explicit_and_direct_mode_preserves_cli_parts() {
        on_cli_test_stack(|| {
            let direct = Cli::try_parse_from([
                "kin",
                "exec",
                "--",
                "printf",
                "value with spaces",
                "literal;semicolon",
            ])
            .unwrap();
            match direct.command {
                Command::Exec { command, shell, .. } => {
                    assert!(!shell);
                    assert_eq!(
                        command,
                        vec!["printf", "value with spaces", "literal;semicolon"]
                    );
                }
                _ => panic!("expected exec command"),
            }

            let shell = Cli::try_parse_from([
                "kin",
                "exec",
                "--shell",
                "--",
                "printf '%s' \"$KIN_REPO_ID\"",
            ])
            .unwrap();
            match shell.command {
                Command::Exec { command, shell, .. } => {
                    assert!(shell);
                    assert_eq!(command, vec!["printf '%s' \"$KIN_REPO_ID\""]);
                }
                _ => panic!("expected exec command"),
            }

            assert!(
                Cli::try_parse_from(["kin", "run", "--", "true"]).is_err(),
                "the pre-release `run` compatibility alias must not remain"
            );
        });
    }
}

#[cfg(test)]
mod help_tests;

#[cfg(test)]
mod routed_words_tests;
