# Guarded entity source edits

`source` returns `source_base` when it can bind the complete body to a
verified source span and one local workspace authority sample. Preserve that
object unchanged with the text you read. An absent or `null` value means no supported writable base
was established; it is not permission to invent one. Historical and hosted-only
reads do not mint a local edit base. A current read of an entity whose span carries
no recorded source digest answers `source_base_unavailable` beside the body: Kin
cannot guard a change to that entity until it is re-derived. An older repository
may need `kin upgrade`. Reread afterward and proceed only if Kin issues a
`source_base`; if the repository is already current or no base is issued, stop
and report the unresolved source verification gap.

A body over 12 KB is served as an excerpt by default. `full_body: true` returns a
body of at most 16 KB whole, with its `source_base`, and an excerpt of such a body
answers `source_base_unavailable: source_excerpt` in place of a base: read the
whole body before changing it. A body over 16 KB is never served whole, so it is
served in excerpts with `full_body` too, and each excerpt carries the body's
`source_base` for a patch. Every anchor of a patch must stand once in the whole
body, so a patch copied from an excerpt changes only what its reader saw. A whole
replacement of such a body is refused. Internal reads, including the one a write's
ticket resolves against, always read the whole body.

On the agent profiles, which answer in plain words, `source` serves the base as a
short ticket, `{"ticket": "sb2.<entity>.<generation>.<digest>"}`, and the whole
object stays on machine replies (the `full` profile, `KIN_MCP_MACHINE_REPLIES=1`,
or a client that asks for `machineReplies`). Copy the ticket into `source_base`
unchanged, as you would the object. The server holds no state for it: when a
write carries one, it reads the entity's current base and resolves the ticket when
that base binds what the ticket binds, which is the repository, the workspace and
its selected head, the entity, its artifact, and the length and digest of the body
the read served. The
write then carries the entity's current base, so every check below applies to it.
A ticket never carries an old body onto a new one: a body that changed since the
read is refused as `source_base_changed` before anything is staged, and one fresh
`source` read and a resend is the fix. A ticket that was not copied exactly is
refused as `source_base_ticket_invalid`. A ticket an older runtime minted, `sb1`,
bound the whole workspace instant and is still read that way: it resolves only
while nothing in the workspace has moved.

Every whole-entity replacement carries a source base. An `update` with no payload,
or an explicit `Entity` payload with a body, is refused as `source_base_required`
before anything is begun or applied, and one fresh `source` read and a
resend with the guarded form is the fix.

Build a guarded operation from the source response, then send it through the
normal transaction tools or `mutate`:

```js
const operation = {
  verb: "update",
  target: source.entity_id,
  payload: { EntitySourceBase: source.source_base },
  body: replacementText, // Complete replacement entity source.
  description: "Explain the intended change"
};
```

For a localized edit, `patch` avoids resending the complete entity. Use the same
source base and literal text copied from the exact source read:

```js
const operation = {
  verb: "patch",
  target: source.entity_id,
  payload: {
    EntitySourcePatch: {
      source_base: source.source_base,
      edits: [{ old_text: "return cached;", new_text: "return cached ?? fallback;" }]
    }
  },
  description: "Use the fallback when the cache is empty"
};
```

Kin places a replacement body at the entity's own indentation. `source` serves a body
from the entity's first character, so a method's first line has no indentation and its
later lines carry the source's. Send the new code as it was served, at the margin, or
indented as a whole: its lines after the first move as one block to the entity's
indentation, each keeping its own depth. A line inside a string keeps its text, except
a docstring's, which moves with the code. Blank lines sent around the code are dropped.

`target` names the entity the `source_base` was read from. Its id is the exact form, and
any address a reply prints for that entity is accepted in its place: the start of its id
(`#535ce03c`), with or without the label before it, the `module :: name` label, or the
name. The base decides which entity a write changes, so a name the entity shares with
another declaration still means the one that was read. A target that is some other
entity is refused with the address of the entity the base names.

Do not send `body` or `destination` with a patch. Every `old_text` must be
nonempty and occur exactly once within the original entity body. Anchors are
literal UTF-8 text, not regexes, line numbers, or file searches. An anchor that is not
in the body as written is read once more with its lines moved by the entity's
indentation, which is how the same lines read when they are shown at another
indentation than the source's. When that fits exactly one place, `new_text` is moved the
same way and replaces the lines there. When it fits several places the patch is refused as ambiguous,
and when it fits none the refusal quotes the code nearest the anchor as the entity holds
it. All edits address
the same original body, never earlier replacements; overlapping, missing,
ambiguous, empty and no-op edits are refused. `new_text` may be empty. Stage at
most one operation per entity; put several anchors in its `edits` array. Different
entities in one transaction share the original authority sample and commit
atomically. A stale source base refuses the entire transaction and retains the
staged patch just as it retains a guarded full body.

Choose the smallest correct form: an anchored patch generally helps a localized
edit in a large body; a guarded full-body update can be smaller for a tiny entity
or a broad rewrite. Both retain the same source-base protection. Neither form
changes the entity's name or kind.

An edit changes the code of the entity it names, and whatever that code declares inside
itself is part of it: a function added, changed or removed inside the edited function, a
method inside an edited class. A declaration beside the edited one is not. The refusal
names what the edit would add or drop and the verb that does it, `create` with
`EntityCreate` or `remove` with `EntityRemove`.

Source that does not parse is never committed. Every update, patch, creation and removal
is read as its language before anything is stored, and a change that brings a syntax
problem is refused with the problem and its line in what was sent. For Python that
includes indentation the language refuses: a block that is not indented under its
header, or a statement indented unlike the statements beside it. A syntax problem the
source already had beside the change is not held against the change. An older runtime may reject `EntitySourcePatch`; do not drop
the guard or fall back to a raw-file semantic edit.

The MCP write boundary accepts the ticket object or the complete base object,
never a bare string. After resolving a ticket, `EntitySourceBase` uses the
complete object's closed schema, `kin.entity.source_base.v1`, published
as `entitySourceBase` by `@kin/boundary-contracts` and embedded in MCP tool discovery.
Unknown fields and versions are refused. Older runtimes reject the unknown typed
payload; clients must not retry it as an unguarded edit.

V1 records repository and workspace identity, workspace generation, selected-head
identity, workspace tree, entity and artifact IDs, source blob hash, exact byte
range and body hash. The head and body hashes use Kin's BLAKE3 blob digest. These
are optimistic concurrency expectations, not authorization credentials.

A base holds while the entity it names still has exactly the bytes that were read,
in the same repository and workspace and on the same selected head. The writer
checks the repository and workspace identity, the selected head, that the entity
still exists with the same artifact, and that its current body has the base's
length and body hash. It does not require the workspace tree, the file's blob hash
or the entity's byte offsets to be the ones the read recorded: a commit that
changed another entity moves all three and changes none of the bytes this edit
replaces. So three entities read up front can be changed in three separate writes,
each carrying the base its own read returned. A base naming a generation the
workspace has not reached is refused.

The selected head is the branch the workspace is on, by name. A commit on that
branch leaves it as it was, and a switch to another branch does not, so an edit
read on one branch never lands on another, even over identical bytes. A detached
head names its target, which every commit moves: there a base holds only until the
next commit.

The guard is the entity's own bytes, and nothing around them. A base for a method
survives edits elsewhere in its class. A base for a class goes stale when any
method inside it changes, because the class's bytes changed. A Python class over 16 KB
is the exception: it is served as its class-level code, and its base guards that code
alone ([Class-level code](#class-level-code)). What a reader saw
beside the entity, the signature of a function it calls for one, may have changed
since, and no base says so: read again whatever an edit depends on. A base never
silently rebases an old body onto the newest entity: a changed body is refused.

The exact daemon writer checks this expectation under its coordination and
authority mutation locks, before creating the publication fence. It reads the
entity's current span and bytes from the one commit base the write is planned and
published against, so no other commit can land between the comparison and the
write it guards. A conflict returns
JSON with schema `kin.entity.source_base_conflict.v1`, code `source_base_conflict`,
`applied: false`, the transaction ID and `staged_operations_retained: true`.
Acknowledged staged work survives restart. Compare the current source with your
retained draft, then submit resolved work in a new transaction. A keyed mutation
also needs a new request ID for changed work. Explicitly abort the old transaction
when it is no longer useful. A storage failure that prevents retention cannot
claim `staged_operations_retained: true`. In every case one fresh `source`
read, compared with your draft, and a resend with the base it returns is the fix.

Receipt recovery precedes freshness checks: retrying a transaction that already
published returns its original receipt and never reapplies its old body over a
newer publication. Request deduplication and caller-read freshness remain separate
guarantees.

The existing exact body-edit constraints still apply, including supported syntax,
entity identity preservation and no implicit declaration insertion/removal. This
contract does not implement editor draft storage or make invalid text publishable.

## Repository bases

Work addressed to a source unit rather than to an existing entity, a unit-addressed
`EntityCreate` or a `UnitImports`, carries a `repository_base` instead of a source base.
It is the workspace instant you last observed:

```js
const repositoryBase = {
  schema: "kin.repository.base.v1",
  context: {
    repository_id, workspace_id, workspace_generation,
    workspace_head_hash, workspace_tree_hash
  }
};
```

`context` has exactly the shape of a source base's `context`. `status` returns the current one
for HEAD, and every successful commit returns the next one beside its `created_entities`. Send it unchanged. Like a source base it is an optimistic
concurrency expectation, not an authorization credential. Unit work is planned against the
workspace tree, so the check compares the repository, workspace, head and tree the base names
with the current ones; an empty repository's first creation is still stale-checked. A
workspace generation that advanced over the same head and tree, such as a toolchain run that
published nothing, is not a conflict.

A stale repository base refuses the whole transaction with JSON schema
`kin.repository.base_conflict.v1`, code `repository_base_conflict`, `applied: false`, the
transaction ID, `current_repository_base` (the base authority holds now) and `next_step`.

A transaction may mix entity edits guarded by source bases with unit work guarded by a
repository base, and every base in it must be current. The current repository base does not
refresh an entity's source base, so when the transaction carried any operation with a
`source_base`, the refusal lists them in `source_reads_required`, each as its operation index and
`entity_id`. Re-read each listed entity with `source`, rebuild that
operation from the read with the fresh `source_base` it returns, and resend every operation
with `current_repository_base`. When nothing is listed, the unit-addressed operations need only
`current_repository_base`, and the resend is one step with no extra read. It is safe because unit
work names its unit and declarations by identity: a name taken since is refused again at the
retry, and every declaration already in the unit keeps its exact bytes.

`mutate` without a `request_id` aborts the refused transaction and answers
`staged_operations_retained: false` and `transaction_aborted: true`, since the caller still holds
the operations it sent. A transaction staged with `transaction` (action `stage`) keeps its operations
(`staged_operations_retained: true`) until you abort it.

## Module-level code

A module's own code is the code no entity of the module owns: its imports and the
top-level statements outside every function and class. A module is never read or
replaced as one body, because its span is its file. `source` on a Python module returns
its own code instead, in file order, as `body`. Each function, class and value over 1 KB
the module holds is one marker line:

```text
import os
import typing as t

LIMIT = 3
__all__ = ["get_debug_flag"]

# kin:entity function get_debug_flag #5f3a9c1e
```

A marker opens with the language's comment and the word `kin:entity`, then names the
entity's kind, its name and the short id `source` reads it by. Nothing in it depends on
the entity's body. Every line without that word is the module's code, byte for byte. Only the outermost entities
are marked: a method is inside its class's marker. A value of at most 1 KB, such as
`LIMIT = 3`, stays in the code as the statement it is; a larger one is a marker, read and
patched by its own id. The reply never holds an entity's body, so a module whose file is
tens of kilobytes reads as its few kilobytes of module-level code. The 12 KB excerpt rule
and `full_body` apply to that code as they do to any body.

The read returns a `source_base`, a ticket on the agent profiles, like an entity read. It
guards the code as served, its length and its digest, and nothing wider: the base's span
is as long as that code, not the file. So it holds while the module's own code is what was
read, in the same repository and workspace and on the same selected head. An edit to the
body of a function or class in the module does not touch it, and neither does a commit to
another module. It goes stale when the module's own code changes, or when the module
gains, loses or renames an entity, since a marker then appears, goes or changes. One fresh
`source` read of the module is the fix.

Change the code with the `patch` an entity takes, targeted at the module:

```js
const operation = {
  verb: "patch",
  target: source.entity_id, // the module
  payload: {
    EntitySourcePatch: {
      source_base: source.source_base,
      edits: [{ old_text: "import os\n", new_text: "import os\nimport re\n" }]
    }
  },
  description: "Import re for the new pattern check"
};
```

An import is module-level code like any other statement: add one by anchoring on a
neighboring line, and remove one by replacing its line with nothing. Kin does not
rewrite or reorder a Python module's imports, so their grouping, their order and every
conditional import stay as the patch leaves them.

Every `old_text` is literal text of the served code, occurs in it exactly once, and reads
the code as served, never an earlier replacement. The edits are applied to that code. The
result must still hold every marker, unchanged and in order, and each marker is then put
back as its entity's exact bytes. An anchor may therefore include a marker when
`new_text` repeats it, which is how code is placed above the first function of a module
that opens with one:

```js
{ old_text: "# kin:entity function main", new_text: "import sys\n\n\n# kin:entity function main" }
```

The commit reparses the module and publishes the change, the working file and the graph
together, exactly as an entity edit does. An import relation the new code introduces, and
one it retires, are part of that same commit.

A patch is refused, with nothing written, when:

- an anchor is not in the module-level code, or occurs more than once;
- an anchor is inside an entity. The refusal names the entity and its id, and `source`
  on that id serves the body to patch instead;
- the result changes, drops, repeats or reorders a marker, or `new_text` writes one;
- the result would add or remove a function or class. Create one with `EntityCreate` and
  remove one with `EntityRemove`. A top-level value, such as a constant, may be added,
  changed or removed;
- the patched module does not parse.

A module that holds no code yet, such as an empty `__init__.py`, is served as an empty
`body` with a `source_base`, and has no text to anchor on. Give it its first code with a
patch whose one edit has an empty `old_text` and the code as `new_text`:

```js
{ old_text: "", new_text: "from .core import run\n" }
```

An empty `old_text` is accepted nowhere else: not beside other edits, not for a module
that holds any code or any entity, and not for an entity body. There is no position
argument; a module with code is anchored on that code.

A module is never replaced whole: `update` with a `body` is refused for a module
whatever base it carries. A module of any language other than Python is served as its
member inventory, as is a module whose code cannot be cut exactly right now, which says
why in `source_base_unavailable`.

## Class-level code

A class's own code is the code no member of the class owns: its header, its docstring
and the statements of its body outside every method and nested class. A class of up to
16 KB is read and written as one entity. A larger Python class cannot be served whole,
so `source` returns its own code as `body`, in source order, with each member as one
marker line at the member's indentation:

```text
class Flask(App):
    """The application object."""

    default_config = ImmutableDict({"DEBUG": None})

    # kin:entity method Flask.__init__ #1c2d3e4f

    session_interface = SecureCookieSessionInterface()

    # kin:entity method Flask.run #9a8b7c6d
```

A marker names the member's kind, its name and the short id `source` reads it by. Every
line without the word `kin:entity` is the class's code, byte for byte, wherever among the
members it stands. The reply never holds a member's body. The 12 KB excerpt rule and
`full_body` apply to that code as they do to any body.

The read returns a `source_base` that guards the code as served, its length and its
digest. It holds through an edit to any member's body, and goes stale when the class's
own code changes or the class gains, loses or renames a member. Change the code with a
`patch` targeted at the class, with the rules a module patch follows: each `old_text` is
literal text of the served code and occurs in it once, and every marker is still in the
result, unchanged and in order.

A patch to class-level code is refused, with nothing written, when:

- an anchor is inside a member. The refusal names the member and its id, and `source` on
  that id serves the body to patch instead;
- the result changes, drops, repeats or reorders a marker, or `new_text` writes one;
- the result would declare a method or a class. Create a method with `EntityCreate` and
  `placement: "member"`, which takes the class's `source_base`;
- a line of the result would stand outside the class;
- the patched code does not parse.

Such a class is never replaced whole: `update` with a `body` is refused for it. A class
with no member is its own code already and stays an ordinary entity.
