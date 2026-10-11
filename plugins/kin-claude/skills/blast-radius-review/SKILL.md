---
name: blast-radius-review
description: Review a change by its blast radius, using Kin's graph to find what downstream code the change can reach. Use when reviewing a diff or pull request, deciding whether an edit is safe, or answering "what else breaks if I change this" in a repository admitted to Kin.
---

# Reviewing a change by its blast radius

A line diff says what text moved. It does not say what the change can reach. Kin's graph
does, because it holds the call, import, and reference edges between entities, so the
downstream set is walked rather than guessed.

Use this when you are reviewing someone else's change, sizing your own before you make it,
or being asked whether an edit is safe.

## The workflow

**1. Name the entities the change touches.** Work from the diff's file and symbol names.
Resolve each one with `locate` by name to get its stable entity id, kind, and signature.
Overloads and same-named symbols in different files are different entities, so carry the
ids forward rather than the names.

**2. Walk the downstream set.** Call `impact` on each changed entity. It returns
the downstream entities the change can reach through the relation graph. That set, not the
diff, is the review surface. `neighbors` with `direction` set to `in` gives the
same shape when you want the immediate dependents at a chosen depth.

**3. Confirm the call sites.** `refs` returns everything that imports, calls, or
references the entity. Use it to separate real callers from name collisions, and to catch
callers that `impact` reached at a depth you did not walk.

**4. Follow the paths that carry data.** For anything touching a value's shape, a parsing
step, or a security boundary, run `trace` from the changed entity. It returns the
ordered chain of steps rather than a flat neighborhood, so you can see how far a changed
value actually travels and where it is consumed.

**5. Read only what the radius earned.** Pull `context` on the affected entities
you must actually judge, and `source` when you need one body. Do not read whole
files to reconstruct context the pack already carries.

**6. Ask who owns the history.** `history` reports an entity's change count,
its latest change, and the approvals recorded against it. A hot entity with no recorded
approvals is worth flagging on its own.

## Report the radius, and the gaps

Lead with what the change can reach and how confident that number is. Name the changed
entities, the downstream entities by risk rather than by count, the paths that carry data
across a boundary, and anything you could not resolve.

Read `_kin.verdict` and `_kin.meaning` on each response before you state a number. A
`complete` answer is whole and proven. A `complete except the sites listed below` answer
names every place left to check in `left_to_check`. A `partial` answer may be missing rows,
and its meaning says why; an empty partial answer is not proof that nothing is affected.
Report a graph gap as a gap. Do not close it with a grep and present the result as proven.

A clean walk is a real finding: a change whose downstream set is empty and whose graph was
authoritative is a small change, and saying so plainly is more useful than hedging.

## When you need the fuller review surface

Kin also ships `semantic_diff` for an entity-level diff, `semantic_review` for a one-call
review covering diff, impact, and risk, and `shadow_gate_report` for a report-only merge
gate over a base-to-head range. Those are outside the default agent tool profile. Start the
server with `KIN_MCP_TOOL_PROFILE=full` to expose them, or run `kin review` from the CLI.
