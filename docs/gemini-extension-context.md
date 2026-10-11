# Kin: read the repository from the graph

Kin parses a repository into entities, relationships, changes, and provenance, and answers
from that graph. Work on targeted semantic entities. Missing parsed coverage is a graph
gap to report or repair at conversion, never a reason to read or search whole files.

## Pick the tool by what you know

Every tool is a plain verb with options, named the way the `kin` CLI names the same
capability.

- **You know the symbol name, or only the behavior.** `locate` takes one `query`. With the
  default `mode: "auto"`, a query that looks like an identifier or a dotted name
  (`parse_config`, `app.run`) matches parsed declarations by name, kind, and language, and
  a whole repository-relative file coordinate (`src/main.rs`, or `./main.rs` for a root
  coordinate) looks up recorded file-module surfaces at that exact graph coordinate.
  It never guesses a basename or returns fuzzy filename neighbors on a miss. Select a
  returned UUID, then `source` returns a Python module's own code (its imports and
  top-level statements, with each function and class as one `kin:entity` line to read
  by its id), and any other module's recorded member inventory and coverage gap;
  neither establishes that every declaration was parsed. Bare dotted names keep
  their name meaning. Other text is ranked as a description of the behavior. Pass
  `mode: "name"` or `mode: "description"` to choose, and `mode: "literal"` for exact text,
  punctuation included. Ranking by description needs the running daemon and an embedded
  graph, and it reports its own coverage.
  The CLI spelling of this MCP contract is `kin call locate` with the same JSON
  arguments (`kin describe locate` lists them). Standalone `kin locate` remains
  the issue-text ranking command with ranking diagnostics and cursors; it does
  not select MCP's auto/name/description modes.
- **You need to understand an entity.** `context` bundles it with its caller and import
  neighborhood. Its default retains code, identities, relationships and coverage;
  `details: true` adds source positions and signatures already quoted in a retained body.
  Repeat the same context arguments for those details against the current graph; this
  does not recover code omitted by a response budget or clear its coverage gaps.
  `source` returns just the body. A body over 12 KB comes back as an excerpt with
  `next_cursor`; send it as `cursor` with the same `entity_id` for the next excerpt.
- **You need callers.** `refs` returns everything that imports, calls, or references a
  symbol. `neighbors` walks the dependency structure, with `direction` of `out`, `in`, or
  `both`. Follow `next_cursor` with the same query and filters to receive all reference
  pages. A partial page cannot prove absence; reconstruct fragmented semantic records before
  using them. A stale cursor needs a fresh query, and completing pages does not clear the
  answer's semantic caveats.

- **You need the path a value travels.** `trace` returns the ordered call and data-flow
  chain from a `focal` entity. Give `to` as well for the routes from one entity to another.
- **You are changing shared code.** `impact` walks the relation graph to the downstream
  entities the change can reach.
- **You need history.** `history` reports an entity's changes, its latest change, and
  recorded approvals.
- **The graph has no matching entity.** Check the coverage in the response. Report missing
  conversion coverage instead of substituting a whole artifact or file-module body.

The names these verbs replaced, such as `semantic_search`, `find_references` and
`kin_mutate`, are still accepted on a call for this release.

## Read the verdict first

Every answer opens with `_kin.verdict` and `_kin.meaning`. The verdict is one of three plain
words, and the meaning starts with the answer and says what proves it and what is left to check:

- `complete`: the answer is whole and proven. Use it as it is, without checking it again
  with other tools. Kin checked it against every file it holds, and each row shows the line of
  code that proves it, so re-reading source to confirm the rows only costs time.
- `complete except the sites listed below`: every row is proven, and the only places that
  could add to it are listed in `left_to_check`, each with its code and a reason. Check a
  site only if its reason could apply.

Code that looks a name up while the program runs, by a name nothing in the code spells, can
reach any function, so no tool that reads code without running it can follow it. `searched`
states that edge of the analysis and counts those places rather than listing them. They are
not sites to check, and they do not make an answer less complete.

A reply cut to fit says so in `delivered`, with the exact call that returns the rest; a cut
list is a whole answer delivered in pages, not a partial one. Rows a type proved are in
`references`, rows matched by name alone in `name_only`, and calls that pass the entity to
other code as an argument in `passed_to_calls`, each with its code.
- `partial`: there may be more than the answer shows, and `meaning` says why. An empty
  partial answer is not proof that something is missing.

A text search (`locate` with mode `literal`) is complete over the text it reads: `meaning`
says how many entities hold the text and `delivered` which ones the page lists and the exact
call for the next page. `status` carries no verdict, because it reports the repository's
health rather than any one answer.

`_kin.searched` says what the answer covered, for example every code file in the
repository, tests and examples included. A semantic answer is never quietly backfilled from
raw file search. The `full` profile serves the machine form of every answer, with its
verdict codes and the evidence they were computed from.

## If the tools have no graph

The server answers from the Kin store of the working directory's repository. If there is none, run `kin init .`
in the repository, or set `KIN_MCP_AUTO_INIT=1` and let the server do it. Then run
`kin embed` to build the vector index `locate` needs to rank by description. The structural tools work as
soon as admission finishes. `kin graph status` reports coverage at any time.

## A working order

Find the entity with `locate`. Choose `source` for its body or `context` for its bounded
neighborhood; do not fetch both when one already answered the question. Use `refs` or
`impact` when the change is shared. Reuse the returned body and source base; do not sweep the repository.

## Make a guarded entity edit

Read `source` and retain its complete `source_base` unchanged. For a localized change in a
large entity, use `mutate` with
`verb: "patch"`, `target: source.id`, and
`payload: {EntitySourcePatch: {source_base: source.source_base, edits:
[{old_text: "unique exact text", new_text: "replacement"}]}}` plus a description.
Omit `body` and `destination`. Every anchor must occur exactly once in the
original entity body; all anchors address that original body and must not
overlap. Use one operation per entity. A stale base or a missing/ambiguous anchor
refuses the transaction without partially applying it.

For a tiny entity or broad rewrite, a guarded full-body `update` can be smaller:
use `payload: {EntitySourceBase: source.source_base}` and the complete new `body`.
Neither form may silently discard the source guard or fall back to raw-file
semantic edits. An excerpt of a body up to 16 KB carries no `source_base`, so read
that entity with `full_body: true` before editing it; a longer body is served only
in excerpts, each carrying a `source_base` for a patch. [Guarded source edits](mcp-source-bases.md) gives the full contract.
