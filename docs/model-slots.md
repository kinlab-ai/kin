# Optional graph models

Kin's graph queries work without a model. Every model slot defaults to `none`,
and the default CLI and daemon carry no local inference engine, weight loader,
Hub downloader or tokenizer. They include HTTP clients for explicitly selected
external embedding and reranking providers.

Repository configuration lives in `.kin/config.toml`:

```toml
[models.embeddings]
provider = "external"
model = "my-embedding-model"
base_url = "http://localhost:1234/v1"
# Optional: name an environment variable, never store its secret here.
api_key_env = "MY_MODEL_API_KEY"

[models.reranking]
provider = "none"
```

The registered slots are `embeddings`, `reranking`, `answer_completeness` and
`call_target`. The last two reserve the common configuration contract for
future graph stages; configuring them does not implement those stages.

Each slot accepts `none`, `built-in` or `external`. An external slot requires a
model identity and an HTTP(S) base URL. Embeddings use `/embeddings`; external
reranking uses a Cohere-compatible `/rerank` request with `model`, `query`,
`documents` and `top_n`. Results refer to original candidate indices. There is
no file search or generated repository answer behind either client.

Environment variables override repository settings. Their names are
`KIN_MODEL_<SLOT>_PROVIDER`, `KIN_MODEL_<SLOT>_MODEL`,
`KIN_MODEL_<SLOT>_BASE_URL` and `KIN_MODEL_<SLOT>_API_KEY_ENV`, with the slot in
upper case, for example `KIN_MODEL_EMBEDDINGS_PROVIDER`. Existing
`KIN_EMBED_*`, `KIN_LOCATE_SCORER` and `KIN_LOCATE_SCORER_MODEL` settings remain
compatibility aliases for one release. New slot variables take precedence.
Restart a running repository daemon after changing a process-level setting;
Kin discloses disagreement between the command and its daemon.

Local embeddings remain an explicit build option:

```sh
cargo build --locked --release -p kin-cli -p kin-daemon --features embeddings
# On macOS, use --features metal for local GPU and Accelerate support.
```

Such a build still defaults to `none`. Select the `built-in` embedding provider,
or run `kin embed` to opt the current repository in. Downloading weights also
requires explicit consent through `kin embed` or setup. Selecting an external
provider never fetches local weights.

The default build has no local model, so there `kin embed` runs only with an
external provider selected. Without one it refuses before any work and names
what is missing, and `kin init` says search by meaning is off instead of
recommending it.

Models only reorder or filter graph candidates. A transport response containing
an unknown index, duplicate row or non-finite score is refused. A model stage
that withholds candidates must disclose its counts. `_kin.model_slots` carries
producer-observed slot, provider, model and candidate counts, rather than echoing
configuration. Model filtering bounds absence claims: a score cannot certify
that code is absent from the repository.

Selecting `none` preserves prepared vector sidecars and their model identity.
Selecting another provider or model checks embedding-space compatibility before
those vectors can serve a query or accept another embedding batch. `kin
bench-meta` distinguishes local inference support, HTTP embedding support and
the selected embedding provider.

Core CLI and daemon release archives use the Cargo defaults above: graph/vector
retrieval and explicitly configured HTTP providers, with no local inference
engine. Source builds can opt into the built-in engine with `--features embeddings`
(or `--features metal` on macOS). CI covers these permutations separately. Hosted
OCI images carry the `hosted-optional-inference` build profile for explicitly
configured tenant model work; that profile does not select a model by default.
