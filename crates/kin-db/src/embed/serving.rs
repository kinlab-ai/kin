// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! How each embedding model Kin knows is served: the pooling that turns its
//! final hidden states into one vector, the task prefixes its queries and
//! documents carry, and how the vector is normalized.
//!
//! These settings are part of the model, not of Kin. A model embeds into the
//! space it was trained for only when it is served the way its card says, so
//! every value in [`SERVING_PROFILES`] is copied from the model's own card and
//! its sentence-transformers configuration (`1_Pooling/config.json`,
//! `config_sentence_transformers.json`, `modules.json`), and the entry names
//! the card it came from. Adding a model is adding one entry.
//!
//! A model that is not in the table is served as its own `config.json` says:
//! its `pooling_mode` when it states one, otherwise the architecture's default
//! pooling, with no prefixes. A local model directory may state its query
//! prefix with a `kin_query_prefix` key in `config.json`.
//!
//! The settings are hashed into the embedding identity Kin stores with its
//! vectors (see [`Serving::descriptor`]), so vectors made under one set of
//! settings are refused and re-embedded under another rather than mixed.

use sha2::{Digest, Sha256};

/// How a model's final hidden states become one vector.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pooling {
    /// The mask-weighted mean of every attended token's hidden state,
    /// special tokens included (`pooling_mode_mean_tokens`).
    Mean,
    /// The hidden state of the first, `[CLS]`, token
    /// (`pooling_mode_cls_token`).
    Cls,
}

impl Pooling {
    /// The `pooling_mode` value kin-infer reads from a model config.
    pub(crate) fn config_value(self) -> &'static str {
        match self {
            Self::Mean => "mean",
            Self::Cls => "cls",
        }
    }
}

/// How a pooled vector is scaled before it is stored or compared.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Normalization {
    /// Divide by the L2 norm, so cosine similarity is a dot product.
    L2,
}

impl Normalization {
    fn descriptor(self) -> &'static str {
        match self {
            Self::L2 => "l2",
        }
    }
}

/// One supported model and the settings its card specifies.
#[derive(Debug, PartialEq, Eq)]
pub struct ServingProfile {
    /// The Hugging Face repository id.
    pub model_id: &'static str,
    /// Other names the same weights are served under by an OpenAI-compatible
    /// server (LM Studio, Ollama), compared case-insensitively.
    pub aliases: &'static [&'static str],
    /// The model card these settings were copied from.
    pub card: &'static str,
    pub pooling: Pooling,
    /// Prepended to every query.
    pub query_prefix: &'static str,
    /// Prepended to every document (entity text).
    pub document_prefix: &'static str,
    pub normalization: Normalization,
    /// The width Kin serves, which is the model's full hidden width.
    pub dimensions: usize,
    /// The narrower Matryoshka widths the card offers, widest first, or empty
    /// for a model trained without them. The card's recipe for a narrower
    /// width is: layer-norm the full pooled vector, truncate, then L2
    /// normalize. At the full width there is no layer norm, only the
    /// normalization above, and the full width is the only one Kin serves.
    pub matryoshka_dimensions: &'static [usize],
    /// SHA-256 of the published `model.safetensors`, which is also the name
    /// the Hugging Face cache stores the blob under. A local model directory
    /// holding these exact weights is served by this entry, whatever the
    /// directory is called.
    pub weights_sha256: &'static str,
}

/// The query instruction CodeRankEmbed and SweRankEmbed were trained with.
const CODE_QUERY_INSTRUCTION: &str = "Represent this query for searching relevant code: ";

/// Every model Kin serves with settings of its own. See the module docs.
pub const SERVING_PROFILES: &[ServingProfile] = &[
    // Kin's default model. The card requires a task prefix on every input,
    // `search_query: ` for queries and `search_document: ` for documents; its
    // `1_Pooling/config.json` sets `pooling_mode_mean_tokens`, and every usage
    // example on the card normalizes the mean-pooled vector with
    // `F.normalize(p=2)`. It is a Matryoshka model; the layer norm in the
    // card's examples applies only when truncating below 768.
    ServingProfile {
        model_id: "nomic-ai/nomic-embed-text-v1.5",
        aliases: &[
            "text-embedding-nomic-embed-text-v1.5",
            "nomic-embed-text-v1.5",
            "nomic-embed-text:v1.5",
            "nomic-embed-text",
        ],
        card: "https://huggingface.co/nomic-ai/nomic-embed-text-v1.5",
        pooling: Pooling::Mean,
        query_prefix: "search_query: ",
        document_prefix: "search_document: ",
        normalization: Normalization::L2,
        dimensions: 768,
        matryoshka_dimensions: &[768, 512, 256, 128, 64],
        weights_sha256: "9e7d262b1fe5ea350782829496efa831901b77486bbde1cea54a4c822d010d5c",
    },
    // `1_Pooling/config.json` sets `pooling_mode_cls_token`, and
    // `config_sentence_transformers.json` gives queries its instruction as the
    // `query` prompt. Documents are encoded raw. The sentence-transformers
    // pipeline has no Normalize module; the model is scored by cosine
    // similarity, which L2 normalization turns into the dot product Kin ranks
    // by without changing any ranking.
    ServingProfile {
        model_id: "nomic-ai/CodeRankEmbed",
        aliases: &[],
        card: "https://huggingface.co/nomic-ai/CodeRankEmbed",
        pooling: Pooling::Cls,
        query_prefix: CODE_QUERY_INSTRUCTION,
        document_prefix: "",
        normalization: Normalization::L2,
        dimensions: 768,
        matryoshka_dimensions: &[],
        weights_sha256: "827529bcd58aef0d9082e66eeff7e7d53a02f62bd005f841a26b3d3e2fb17ebe",
    },
    // `1_Pooling/config.json` sets `pooling_mode_cls_token` and the `query`
    // prompt is the code instruction. The card's Transformers example takes
    // the first token and normalizes with `F.normalize(p=2)`; documents are
    // encoded raw.
    ServingProfile {
        model_id: "Salesforce/SweRankEmbed-Small",
        aliases: &[],
        card: "https://huggingface.co/Salesforce/SweRankEmbed-Small",
        pooling: Pooling::Cls,
        query_prefix: CODE_QUERY_INSTRUCTION,
        document_prefix: "",
        normalization: Normalization::L2,
        dimensions: 768,
        matryoshka_dimensions: &[],
        weights_sha256: "e40687da7f148e684648691facbaa2222503829afb9d21b9b3913034bef14f41",
    },
    // `1_Pooling/config.json` sets `pooling_mode_cls_token` and `modules.json`
    // ends in a Normalize module. The card's model list gives the query
    // instruction for retrieval, and says passages never carry one.
    ServingProfile {
        model_id: "BAAI/bge-small-en-v1.5",
        aliases: &[],
        card: "https://huggingface.co/BAAI/bge-small-en-v1.5",
        pooling: Pooling::Cls,
        query_prefix: "Represent this sentence for searching relevant passages: ",
        document_prefix: "",
        normalization: Normalization::L2,
        dimensions: 384,
        matryoshka_dimensions: &[],
        weights_sha256: "3c9f31665447c8911517620762200d2245a2518d6e7208acc78cd9db317e21ad",
    },
];

/// The profile for a Hugging Face repository id, compared case-insensitively.
pub fn profile_for_model_id(model_id: &str) -> Option<&'static ServingProfile> {
    let wanted = model_id.trim();
    SERVING_PROFILES
        .iter()
        .find(|profile| profile.model_id.eq_ignore_ascii_case(wanted))
}

/// The profile for a model an OpenAI-compatible server names, by repository
/// id or by one of the entry's server aliases. A trailing `@<quantization>`
/// tag (LM Studio's `name@q8_0`) names the same weights and is ignored.
pub fn profile_for_served_name(name: &str) -> Option<&'static ServingProfile> {
    let trimmed = name.trim();
    let base = trimmed.split_once('@').map_or(trimmed, |(base, _)| base);
    profile_for_model_id(base).or_else(|| {
        SERVING_PROFILES.iter().find(|profile| {
            profile
                .aliases
                .iter()
                .any(|alias| alias.eq_ignore_ascii_case(base))
        })
    })
}

/// The profile whose published weights have this SHA-256 digest.
pub fn profile_for_weights(sha256: &[u8; 32]) -> Option<&'static ServingProfile> {
    let digest = hex::encode(sha256);
    SERVING_PROFILES
        .iter()
        .find(|profile| profile.weights_sha256 == digest)
}

/// The settings one loaded model is actually served with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Serving {
    /// `None` leaves pooling to the model's own `config.json`, which is what a
    /// model outside [`SERVING_PROFILES`] gets.
    pub pooling: Option<Pooling>,
    pub query_prefix: String,
    pub document_prefix: String,
    pub normalization: Normalization,
}

impl Serving {
    /// Serve a model as its table entry says.
    pub fn from_profile(profile: &ServingProfile) -> Self {
        Self {
            pooling: Some(profile.pooling),
            query_prefix: profile.query_prefix.to_string(),
            document_prefix: profile.document_prefix.to_string(),
            normalization: profile.normalization,
        }
    }

    /// Serve a model outside the table as its own config says, with the
    /// query prefix a local directory states, if any.
    pub fn unlisted(query_prefix: Option<String>) -> Self {
        Self {
            pooling: None,
            query_prefix: query_prefix.unwrap_or_default(),
            document_prefix: String::new(),
            normalization: Normalization::L2,
        }
    }

    /// Serving for a Hugging Face repository id, decided by the id alone, so
    /// the identity of a store's vectors can be read without loading or
    /// downloading the model.
    pub fn for_hub_model(model_id: &str) -> Self {
        profile_for_model_id(model_id).map_or_else(|| Self::unlisted(None), Self::from_profile)
    }

    /// The canonical text of these settings, hashed into every embedding
    /// identity Kin stores. Any change to it names a different vector space.
    pub fn descriptor(&self) -> String {
        format!(
            "kin-embedding-serving-v1;pooling={};query_prefix={:?};document_prefix={:?};normalization={}",
            self.pooling.map_or("model-config", Pooling::config_value),
            self.query_prefix,
            self.document_prefix,
            self.normalization.descriptor(),
        )
    }

    /// A short digest of [`Self::descriptor`] for identity fields that are
    /// read by people as well as compared.
    pub fn digest(&self) -> String {
        let digest = Sha256::digest(self.descriptor().as_bytes());
        hex::encode(&digest[..6])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What each model's card and sentence-transformers files say, transcribed
    /// from the files themselves. `1_Pooling/config.json` and
    /// `config_sentence_transformers.json` are quoted verbatim; the prefixes
    /// and the normalization that no config file carries are quoted from the
    /// card text.
    struct Card {
        model_id: &'static str,
        pooling_config: &'static str,
        sentence_transformers_config: &'static str,
        /// The card's own words for the query and document task prefixes.
        card_query_prefix: &'static str,
        card_document_prefix: &'static str,
        /// Whether the card normalizes every vector it scores (a Normalize
        /// module, `F.normalize(p=2)`, or `normalize_embeddings=True`), or
        /// scores by cosine similarity, which equals the dot product of L2
        /// normalized vectors.
        card_scores_normalized_vectors: bool,
        /// The Matryoshka widths from the card's dimensionality table.
        card_matryoshka_dimensions: &'static [usize],
        /// The width a card example leaves untruncated, and so un-layer-normed.
        card_full_width: usize,
    }

    const CARDS: &[Card] = &[
        Card {
            model_id: "nomic-ai/nomic-embed-text-v1.5",
            pooling_config: r#"{
                "word_embedding_dimension": 768,
                "pooling_mode_cls_token": false,
                "pooling_mode_mean_tokens": true,
                "pooling_mode_max_tokens": false,
                "pooling_mode_mean_sqrt_len_tokens": false,
                "pooling_mode_weightedmean_tokens": false,
                "pooling_mode_lasttoken": false
            }"#,
            sentence_transformers_config: r#"{
                "__version__": {"sentence_transformers": "2.4.0.dev0", "transformers": "4.37.2", "pytorch": "2.1.0+cu121"}
            }"#,
            // "you embed your documents as `search_document: <text here>` and
            // embed your user queries as `search_query: <text here>`"
            card_query_prefix: "search_query: ",
            card_document_prefix: "search_document: ",
            // "embeddings = mean_pooling(...) ... embeddings = F.normalize(embeddings, p=2, dim=1)"
            card_scores_normalized_vectors: true,
            // "Adjusting Dimensionality": 768, 512, 256, 128, 64.
            card_matryoshka_dimensions: &[768, 512, 256, 128, 64],
            card_full_width: 768,
        },
        Card {
            model_id: "nomic-ai/CodeRankEmbed",
            pooling_config: r#"{
                "word_embedding_dimension": 768,
                "pooling_mode_cls_token": true,
                "pooling_mode_mean_tokens": false,
                "pooling_mode_max_tokens": false,
                "pooling_mode_mean_sqrt_len_tokens": false,
                "pooling_mode_weightedmean_tokens": false,
                "pooling_mode_lasttoken": false,
                "include_prompt": true
            }"#,
            sentence_transformers_config: r#"{
                "__version__": {"sentence_transformers": "2.7.0.dev0", "transformers": "4.39.3", "pytorch": "2.1.0+cu121"},
                "prompts": {"query": "Represent this query for searching relevant code: "},
                "default_prompt_name": null
            }"#,
            // "the query prompt *must* include the following *task instruction
            // prefix*"; the card encodes code snippets raw.
            card_query_prefix: "Represent this query for searching relevant code: ",
            card_document_prefix: "",
            card_scores_normalized_vectors: true,
            card_matryoshka_dimensions: &[],
            card_full_width: 768,
        },
        Card {
            model_id: "Salesforce/SweRankEmbed-Small",
            pooling_config: r#"{
                "word_embedding_dimension": 768,
                "pooling_mode_cls_token": true,
                "pooling_mode_mean_tokens": false,
                "pooling_mode_max_tokens": false,
                "pooling_mode_mean_sqrt_len_tokens": false,
                "pooling_mode_weightedmean_tokens": false,
                "pooling_mode_lasttoken": false,
                "include_prompt": true
            }"#,
            sentence_transformers_config: r#"{
                "prompts": {"query": "Represent this query for searching relevant code: "},
                "default_prompt_name": null
            }"#,
            card_query_prefix: "Represent this query for searching relevant code: ",
            card_document_prefix: "",
            // "query_embeddings = torch.nn.functional.normalize(query_embeddings, p=2, dim=1)"
            card_scores_normalized_vectors: true,
            card_matryoshka_dimensions: &[],
            card_full_width: 768,
        },
        Card {
            model_id: "BAAI/bge-small-en-v1.5",
            pooling_config: r#"{
                "word_embedding_dimension": 384,
                "pooling_mode_cls_token": true,
                "pooling_mode_mean_tokens": false,
                "pooling_mode_max_tokens": false,
                "pooling_mode_mean_sqrt_len_tokens": false
            }"#,
            sentence_transformers_config: r#"{
                "__version__": {"sentence_transformers": "2.2.2", "transformers": "4.28.1", "pytorch": "1.13.0+cu117"}
            }"#,
            // Model list, "query instruction for retrieval"; "In all cases,
            // **no instruction** needs to be added to passages."
            card_query_prefix: "Represent this sentence for searching relevant passages: ",
            card_document_prefix: "",
            // `modules.json` ends in `sentence_transformers.models.Normalize`.
            card_scores_normalized_vectors: true,
            card_matryoshka_dimensions: &[],
            card_full_width: 384,
        },
    ];

    fn card_pooling(config: &str) -> (Pooling, usize) {
        let value: serde_json::Value = serde_json::from_str(config).unwrap();
        let flag = |key: &str| value.get(key).and_then(serde_json::Value::as_bool) == Some(true);
        let modes = [
            "pooling_mode_cls_token",
            "pooling_mode_mean_tokens",
            "pooling_mode_max_tokens",
            "pooling_mode_mean_sqrt_len_tokens",
            "pooling_mode_weightedmean_tokens",
            "pooling_mode_lasttoken",
        ];
        let set: Vec<_> = modes.iter().filter(|mode| flag(mode)).collect();
        assert_eq!(set.len(), 1, "exactly one pooling mode: {set:?}");
        let pooling = match *set[0] {
            "pooling_mode_cls_token" => Pooling::Cls,
            "pooling_mode_mean_tokens" => Pooling::Mean,
            other => panic!("Kin cannot serve {other}"),
        };
        let width = value["word_embedding_dimension"].as_u64().unwrap() as usize;
        (pooling, width)
    }

    fn card_query_prompt(config: &str) -> Option<String> {
        let value: serde_json::Value = serde_json::from_str(config).unwrap();
        value
            .get("prompts")
            .and_then(|prompts| prompts.get("query"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
    }

    /// Every table entry serves exactly what its card says, and every card
    /// transcribed above has an entry.
    #[test]
    fn each_model_is_served_as_its_card_specifies() {
        assert_eq!(CARDS.len(), SERVING_PROFILES.len());
        for card in CARDS {
            let profile = profile_for_model_id(card.model_id)
                .unwrap_or_else(|| panic!("{} has no serving profile", card.model_id));
            let (pooling, width) = card_pooling(card.pooling_config);
            assert_eq!(profile.pooling, pooling, "{} pooling", card.model_id);
            assert_eq!(profile.dimensions, width, "{} width", card.model_id);
            assert_eq!(
                profile.dimensions, card.card_full_width,
                "{} is served at its full, un-layer-normed width",
                card.model_id
            );
            if let Some(prompt) = card_query_prompt(card.sentence_transformers_config) {
                assert_eq!(
                    profile.query_prefix, prompt,
                    "{} query prompt",
                    card.model_id
                );
            }
            assert_eq!(
                profile.query_prefix, card.card_query_prefix,
                "{} query prefix",
                card.model_id
            );
            assert_eq!(
                profile.document_prefix, card.card_document_prefix,
                "{} document prefix",
                card.model_id
            );
            assert!(card.card_scores_normalized_vectors);
            assert_eq!(
                profile.normalization,
                Normalization::L2,
                "{}",
                card.model_id
            );
            assert_eq!(
                profile.matryoshka_dimensions, card.card_matryoshka_dimensions,
                "{} Matryoshka widths",
                card.model_id
            );
            assert!(
                profile.matryoshka_dimensions.is_empty()
                    || profile.matryoshka_dimensions[0] == profile.dimensions,
                "{} serves its widest Matryoshka width",
                card.model_id
            );
            assert!(profile.card.ends_with(card.model_id));
            assert_eq!(profile.weights_sha256.len(), 64);
        }
    }

    /// The defect this table exists to prevent: the default model was served
    /// with CLS pooling and CodeRankEmbed's query instruction because both are
    /// nomic_bert models. The two now resolve to different settings, each
    /// their own.
    #[test]
    fn the_default_model_and_coderankembed_are_served_differently() {
        let text = Serving::for_hub_model("nomic-ai/nomic-embed-text-v1.5");
        assert_eq!(text.pooling, Some(Pooling::Mean));
        assert_eq!(text.query_prefix, "search_query: ");
        assert_eq!(text.document_prefix, "search_document: ");
        let code = Serving::for_hub_model("nomic-ai/CodeRankEmbed");
        assert_eq!(code.pooling, Some(Pooling::Cls));
        assert_eq!(
            code.query_prefix,
            "Represent this query for searching relevant code: "
        );
        assert_eq!(code.document_prefix, "");
        assert_ne!(text.descriptor(), code.descriptor());
        assert_ne!(text.digest(), code.digest());
    }

    #[test]
    fn a_model_outside_the_table_is_served_by_its_own_config() {
        let serving = Serving::for_hub_model("sentence-transformers/all-MiniLM-L6-v2");
        assert_eq!(serving, Serving::unlisted(None));
        assert_eq!(serving.pooling, None);
        assert!(serving.query_prefix.is_empty());
        assert!(serving.document_prefix.is_empty());
        assert!(serving.descriptor().contains("pooling=model-config"));
    }

    #[test]
    fn every_serving_setting_changes_the_descriptor() {
        let base = Serving::for_hub_model("nomic-ai/nomic-embed-text-v1.5");
        let mut cls = base.clone();
        cls.pooling = Some(Pooling::Cls);
        let mut unpooled = base.clone();
        unpooled.pooling = None;
        let mut query = base.clone();
        query.query_prefix = "search_query:".into();
        let mut document = base.clone();
        document.document_prefix = String::new();
        for changed in [cls, unpooled, query, document] {
            assert_ne!(base.descriptor(), changed.descriptor(), "{changed:?}");
            assert_ne!(base.digest(), changed.digest(), "{changed:?}");
        }
    }

    #[test]
    fn server_names_resolve_to_the_same_profile() {
        let default = profile_for_model_id("nomic-ai/nomic-embed-text-v1.5").unwrap();
        for name in [
            "nomic-ai/nomic-embed-text-v1.5",
            "NOMIC-AI/nomic-embed-text-v1.5",
            "text-embedding-nomic-embed-text-v1.5",
            "text-embedding-nomic-embed-text-v1.5@q8_0",
            "nomic-embed-text",
        ] {
            assert_eq!(profile_for_served_name(name), Some(default), "{name}");
        }
        // A different model of the same family is not this one.
        assert_eq!(profile_for_served_name("nomic-embed-text-v2-moe"), None);
        assert_eq!(profile_for_served_name("text-embedding-3-small"), None);
    }

    #[test]
    fn published_weights_select_their_profile_by_content() {
        for profile in SERVING_PROFILES {
            let mut digest = [0u8; 32];
            hex::decode_to_slice(profile.weights_sha256, &mut digest).unwrap();
            assert_eq!(profile_for_weights(&digest), Some(profile));
        }
        assert_eq!(profile_for_weights(&[0u8; 32]), None);
    }
}
