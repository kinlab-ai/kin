# kin-search

> Lexical search for Kin's semantic retrieval.

`kin-search` is a small, in-memory BM25 text index with code-aware
tokenization and no external search-engine dependency. It splits identifiers
(camelCase / snake_case) so source symbols are matched the way developers
write them.

It is a low-level retrieval primitive in the open Kin local stack. Higher
layers, including `kin-db`, compose it with vector retrieval and graph
structure; Kin's ranking and proof-weighting policy lives above this crate, not
inside it.

[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)
[![Part of Kin](https://img.shields.io/badge/part%20of-Kin-6E56CF.svg)](https://github.com/kinlab-ai/kin)

## What is Kin?

Kin is a graph-native code repository for AI agents and the people who build with
them. It stores source, recorded code relationships, and versioned history as
repository state. The graph is the repository model, not a search index maintained
beside another repository.

Start at **[kinlab-ai/kin](https://github.com/kinlab-ai/kin)** · **[kinlab.ai](https://kinlab.ai)**

## Build

```bash
cargo build
cargo test
```

## Usage

```rust
use kin_search::TextIndex;

// Build an in-memory BM25 index over any key type.
let index: TextIndex<u64> = TextIndex::new();

// Index documents by weighted text fields (name, signature, path, and so on).
index.upsert(1, &[("render_frame", 5.0), ("graphics pipeline step", 2.0)])?;
index.upsert(2, &[("parse_tokens", 5.0), ("lexer token stream", 2.0)])?;

// camelCase and snake_case identifiers are split automatically.
let hits = index.fuzzy_search("renderFrame", 10)?;
for (id, score) in hits {
    println!("{id}: {score:.3}");
}
```

## Key types

- `TextIndex<Id>`: the index, generic over a document-key type.
- `DocId`: blanket trait for usable key types (`Copy + Eq + Hash + Send + Sync + Debug + 'static`).
- `Searchable`: implement to auto-extract searchable fields from your own types.
- `tokenize()`: the public code-aware tokenizer.

## License

[Apache-2.0](LICENSE).
