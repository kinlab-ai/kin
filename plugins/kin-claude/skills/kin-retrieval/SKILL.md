---
name: kin-retrieval
description: Read a codebase through Kin's semantic graph instead of grep and whole-file reads. Use when finding where something lives, what a symbol does, who calls it, or which code implements a described behavior, and when a repo has been admitted to Kin (a .kin/ directory exists).
---

# Reading a repository through the Kin graph

Kin parses a repository into entities, relationships, changes, and provenance, and answers
questions from that graph. Where grep matches strings and a file read spends context on
lines you did not need, these tools return the declaration, its neighborhood, and the
graph state that produced the answer.

Answer semantic questions from the graph. If the graph does not model the relevant
content, report the coverage gap and the supported conversion or enrichment step
needed to address it. Do not substitute grep, raw filesystem search, whole-file reads,
or artifact bodies for a missing semantic answer. Import, reconcile, and materialization
are separate operations, not retrieval fallbacks.

## Pick the tool by what you know

Every Kin tool is a plain verb with options, named the way the `kin` CLI names it.

You know the name, or only the behavior. Use `locate` with one `query`. With the default
`mode: "auto"`, a query that looks like an identifier or a dotted name (`parse_config`,
`app.run`) matches parsed declarations by name, kind, and language, so it returns
functions, classes, structs, traits, enums, interfaces, types, and constants rather than
every line that mentions the word. Anything else is ranked against the vector index as a
description of the behavior, the retrieval that powers `kin locate`, which needs the
running daemon and an embedded graph and reports its coverage in the response. Pass
`mode: "name"` or `mode: "description"` to choose, and `mode: "literal"` for exact text,
punctuation included. Each hit carries its file path, line range, signature, and a stable
entity id. Results are semantic entities; file granularity is unavailable.

You have an entity and need to understand it. Use `context`. It bundles the target with
its caller and import neighborhood into one prompt-sized payload, which replaces a chain of
separate reads. `source` returns just the implementation body when the pack is more than
you need.

You need callers. Use `refs` for everything that imports, calls, or references a symbol.
Use `neighbors` when you want the dependency structure around an entity, with `direction`
of `out` for what it depends on, `in` for what depends on it, and `both` for the merged
view.

You need the path a value travels. Use `trace`. It walks the call and data-flow chain from
a `focal` entity and returns it as an ordered list of steps. Give `to` as well and it
returns how the one entity reaches the other, as hops.

You need history. Use `history` for who changed an entity, how many times, what the latest
change was, and which approvals are recorded.

A query has no matching semantic entity. Read the coverage evidence and report missing
conversion coverage. Whole artifacts and importer-created file-module nodes are not source
read substitutes. Choose an entity body or bounded context, then reuse it instead of
reading the same source again through several tools.

## Read the envelope before you trust the answer

Every response carries a `_kin` envelope. It names the runtime that answered
(`repo-daemon` for the live graph, `offline-in-process` for the fallback store), the graph
generation and reconciliation state, embedding coverage, and any degraded flags. There is
no configuration under which a semantic answer is quietly backfilled from raw file search.

Read `_kin.verdict` and `_kin.meaning` first. `complete` means the answer is whole and
proven, so use it without checking it again. `complete except the sites listed below` means
only the sites in `left_to_check` are open, each with its code and a reason; check a site
only if its reason could apply. Lookups of a name built at run time, counted in `searched`,
are a blind spot of every tool that reads code, not a site to check. `partial` means there
may be more, and `meaning` says why. A complete answer was checked against every file Kin
holds, and each row shows the line of code that proves it, so re-reading source to confirm
the rows only costs time.
An empty result is an absence only when its verdict is `complete`. Report the stated
limitation when the answer is partial. A name-resolution miss also does not prove that a
resolved entity has no references.

If `locate` by description reports thin coverage, embeddings are still building. Run
`kin embed` in the repository, or use `locate` by name and the structural tools in the
meantime, which do not depend on the vector index.

## A working order

Start with one `locate` to find the entity. Pull `context` on the best hit. Add `refs` if
you need the call sites, or `impact` if you are about to change something shared. That is usually two or three
calls, and it is enough. Stop and answer rather than sweeping the tree.
