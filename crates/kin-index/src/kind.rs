// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! What an unparsed entity is, in a word or two, and the cheap facts its
//! first bytes state.
//!
//! Each format Kin recognizes is one [`KindDetector`], and the formats are
//! asked in the order of one registry: [`BYTES`] for an entity held as bytes,
//! [`TEXT`] for one held as text. A format joins by adding its detector
//! there; nothing else in Kin knows the format. Content that names its own
//! format comes first, so a PNG is a PNG image whatever it is called, and a
//! name is asked only when no content does.
//!
//! Everything is computed from an entity's name and its leading bytes. The
//! classifier stores one thing from here: the media type of content that is
//! not text ([`media_type`]), which the retrieval record of such an entity
//! carries, so a better detector re-derives those records and nothing else.

use kin_model::ArtifactKind;

use crate::{FileClassification, FileClassifier};

/// The media type of bytes no detector recognizes.
pub const UNKNOWN_BYTES: &str = "application/octet-stream";

/// What an entity is and what its first bytes say about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Described {
    /// The kind a row and a sentence name it by: `PNG image`, `licence`.
    pub kind: String,
    /// A fact read from the content's header, as a clause: `16 by 8 pixels`.
    pub detail: Option<String>,
    /// Its media type, where the format has one: `image/png`.
    pub media_type: Option<&'static str>,
}

impl Described {
    fn of(kind: &str) -> Self {
        Self {
            kind: kind.to_string(),
            detail: None,
            media_type: None,
        }
    }

    fn sized(kind: &str, size: Option<(u32, u32)>) -> Self {
        Self {
            kind: kind.to_string(),
            detail: size
                .filter(|(width, height)| *width > 0 && *height > 0)
                .map(|(width, height)| format!("{width} by {height} pixels")),
            media_type: None,
        }
    }

    fn typed(mut self, media_type: Option<&'static str>) -> Self {
        self.media_type = media_type;
        self
    }
}

/// What a detector is shown of one entity: where it is projected, its own
/// name and its leading bytes.
pub struct Probe<'a> {
    pub place: &'a str,
    pub name: &'a str,
    pub head: &'a [u8],
}

impl Probe<'_> {
    /// The name without its last extension, lower case, and that extension.
    fn split(&self) -> (String, String) {
        let lower = self.name.to_ascii_lowercase();
        match lower.rsplit_once('.') {
            Some((stem, extension)) if !stem.is_empty() => {
                (stem.to_string(), extension.to_string())
            }
            _ => (lower, String::new()),
        }
    }

    fn at(&self, offset: usize, signature: &[u8]) -> bool {
        self.head.get(offset..offset + signature.len()) == Some(signature)
    }

    fn be16(&self, at: usize) -> Option<u32> {
        let field = self.head.get(at..at + 2)?;
        Some(u32::from(u16::from_be_bytes([field[0], field[1]])))
    }

    fn le16(&self, at: usize) -> Option<u32> {
        let field = self.head.get(at..at + 2)?;
        Some(u32::from(u16::from_le_bytes([field[0], field[1]])))
    }

    fn be32(&self, at: usize) -> Option<u32> {
        let field = self.head.get(at..at + 4)?;
        Some(u32::from_be_bytes([field[0], field[1], field[2], field[3]]))
    }
}

/// One format Kin recognizes an unparsed entity as.
pub trait KindDetector: Sync {
    /// What `probe` is when it is this format, or `None`.
    fn detect(&self, probe: &Probe<'_>) -> Option<Described>;
}

/// A PNG image, with the size its header states.
struct Png;

impl KindDetector for Png {
    fn detect(&self, probe: &Probe<'_>) -> Option<Described> {
        probe.at(0, b"\x89PNG\r\n\x1a\n").then(|| {
            let size = probe.be32(16).zip(probe.be32(20));
            Described::sized("PNG image", size).typed(Some("image/png"))
        })
    }
}

/// A GIF image, with the size its screen descriptor states.
struct Gif;

impl KindDetector for Gif {
    fn detect(&self, probe: &Probe<'_>) -> Option<Described> {
        (probe.at(0, b"GIF87a") || probe.at(0, b"GIF89a")).then(|| {
            Described::sized("GIF image", probe.le16(6).zip(probe.le16(8))).typed(Some("image/gif"))
        })
    }
}

/// A JPEG image, with the size its first frame header states.
struct Jpeg;

impl Jpeg {
    fn size(probe: &Probe<'_>) -> Option<(u32, u32)> {
        let bytes = probe.head;
        let mut at = 2;
        while at + 9 < bytes.len() {
            if bytes[at] != 0xFF {
                return None;
            }
            let marker = bytes[at + 1];
            // Padding, and markers that carry no length.
            if marker == 0xFF {
                at += 1;
                continue;
            }
            if (0xD0..=0xD9).contains(&marker) || marker == 0x01 {
                at += 2;
                continue;
            }
            let length = probe.be16(at + 2)? as usize;
            let frame = matches!(marker, 0xC0..=0xCF) && !matches!(marker, 0xC4 | 0xC8 | 0xCC);
            if frame {
                return Some((probe.be16(at + 7)?, probe.be16(at + 5)?));
            }
            at += 2 + length.max(2);
        }
        None
    }
}

impl KindDetector for Jpeg {
    fn detect(&self, probe: &Probe<'_>) -> Option<Described> {
        probe
            .at(0, &[0xFF, 0xD8, 0xFF])
            .then(|| Described::sized("JPEG image", Self::size(probe)).typed(Some("image/jpeg")))
    }
}

/// A WebP image: a RIFF container of WEBP.
struct WebP;

impl KindDetector for WebP {
    fn detect(&self, probe: &Probe<'_>) -> Option<Described> {
        (probe.at(0, b"RIFF") && probe.at(8, b"WEBP"))
            .then(|| Described::of("WebP image").typed(Some("image/webp")))
    }
}

/// A BMP image: its signature and a header long enough to be one.
struct Bmp;

impl KindDetector for Bmp {
    fn detect(&self, probe: &Probe<'_>) -> Option<Described> {
        (probe.at(0, b"BM") && probe.head.len() > 26)
            .then(|| Described::of("BMP image").typed(Some("image/bmp")))
    }
}

/// A format known by the signature it holds at one offset.
struct Magic {
    kind: &'static str,
    at: usize,
    signature: &'static [u8],
    media_type: Option<&'static str>,
}

impl KindDetector for Magic {
    fn detect(&self, probe: &Probe<'_>) -> Option<Described> {
        probe
            .at(self.at, self.signature)
            .then(|| Described::of(self.kind).typed(self.media_type))
    }
}

/// A format known by the last extension of its name.
struct Extension {
    kind: &'static str,
    extensions: &'static [&'static str],
    media_type: Option<&'static str>,
}

impl KindDetector for Extension {
    fn detect(&self, probe: &Probe<'_>) -> Option<Described> {
        let (_, extension) = probe.split();
        self.extensions
            .contains(&extension.as_str())
            .then(|| Described::of(self.kind).typed(self.media_type))
    }
}

/// A format known by its whole name, case aside.
struct Named {
    kind: &'static str,
    names: &'static [&'static str],
}

impl KindDetector for Named {
    fn detect(&self, probe: &Probe<'_>) -> Option<Described> {
        let name = probe.name.to_ascii_lowercase();
        self.names
            .contains(&name.as_str())
            .then(|| Described::of(self.kind))
    }
}

/// A licence, known by its name whatever it is written in.
struct Licence;

impl KindDetector for Licence {
    fn detect(&self, probe: &Probe<'_>) -> Option<Described> {
        let (stem, extension) = probe.split();
        let titled = extension.is_empty() || matches!(extension.as_str(), "txt" | "md" | "rst");
        let licence = matches!(
            stem.as_str(),
            "license"
                | "licence"
                | "copying"
                | "unlicense"
                | "notice"
                | "license-mit"
                | "license-apache"
        ) || stem.starts_with("license.")
            || stem.starts_with("licence.");
        (titled && licence).then(|| Described::of("licence"))
    }
}

/// A structure the indexer recognizes by its place: a package manifest, a
/// CI configuration, a Dockerfile.
struct Structured;

impl KindDetector for Structured {
    fn detect(&self, probe: &Probe<'_>) -> Option<Described> {
        let FileClassification::StructuredArtifact(kind) =
            FileClassifier::classify(std::path::Path::new(probe.place))
        else {
            return None;
        };
        Some(Described::of(match kind {
            ArtifactKind::PackageManifest => "package manifest",
            ArtifactKind::SqlMigration => "SQL migration",
            ArtifactKind::CiConfig => "CI configuration",
            ArtifactKind::Dockerfile => "Dockerfile",
            ArtifactKind::ComposeFile => "Compose configuration",
            ArtifactKind::Makefile => "Makefile",
        }))
    }
}

const fn magic(kind: &'static str, signature: &'static [u8], media_type: &'static str) -> Magic {
    Magic {
        kind,
        at: 0,
        signature,
        media_type: Some(media_type),
    }
}

const fn extension(kind: &'static str, extensions: &'static [&'static str]) -> Extension {
    Extension {
        kind,
        extensions,
        media_type: None,
    }
}

const fn typed(
    kind: &'static str,
    extensions: &'static [&'static str],
    media_type: &'static str,
) -> Extension {
    Extension {
        kind,
        extensions,
        media_type: Some(media_type),
    }
}

/// The formats of an entity held as bytes, in the order they are asked:
/// those its content names, then those its name names.
pub static BYTES: &[&dyn KindDetector] = &[
    &Png,
    &Gif,
    &Jpeg,
    &WebP,
    &Bmp,
    &magic("icon", &[0, 0, 1, 0], "image/vnd.microsoft.icon"),
    &magic("PDF document", b"%PDF-", "application/pdf"),
    &magic("ZIP archive", b"PK\x03\x04", "application/zip"),
    &magic("gzip archive", &[0x1F, 0x8B], "application/gzip"),
    &magic("bzip2 archive", b"BZh", "application/x-bzip2"),
    &magic(
        "XZ archive",
        &[0xFD, b'7', b'z', b'X', b'Z', 0],
        "application/x-xz",
    ),
    &magic(
        "Zstandard archive",
        &[0x28, 0xB5, 0x2F, 0xFD],
        "application/zstd",
    ),
    &magic(
        "7z archive",
        b"7z\xBC\xAF\x27\x1C",
        "application/x-7z-compressed",
    ),
    &magic("WOFF font", b"wOFF", "font/woff"),
    &magic("WOFF2 font", b"wOF2", "font/woff2"),
    &magic("OpenType font", b"OTTO", "font/otf"),
    &magic("TrueType font", &[0, 1, 0, 0, 0], "font/ttf"),
    &magic("WebAssembly module", b"\0asm", "application/wasm"),
    &magic(
        "SQLite database",
        b"SQLite format 3\0",
        "application/vnd.sqlite3",
    ),
    &magic("ELF binary", b"\x7FELF", "application/x-elf"),
    &magic(
        "Mach-O binary",
        &[0xCF, 0xFA, 0xED, 0xFE],
        "application/x-mach-binary",
    ),
    &magic(
        "Java class or universal binary",
        &[0xCA, 0xFE, 0xBA, 0xBE],
        "application/octet-stream",
    ),
    &magic("MP3 recording", b"ID3", "audio/mpeg"),
    &magic("Ogg recording", b"OggS", "audio/ogg"),
    &magic("FLAC recording", b"fLaC", "audio/flac"),
    &Magic {
        kind: "MP4 video",
        at: 4,
        signature: b"ftyp",
        media_type: Some("video/mp4"),
    },
    &Magic {
        kind: "tar archive",
        at: 257,
        signature: b"ustar",
        media_type: Some("application/x-tar"),
    },
    &typed("PNG image", &["png"], "image/png"),
    &typed("JPEG image", &["jpg", "jpeg"], "image/jpeg"),
    &typed("GIF image", &["gif"], "image/gif"),
    &typed("WebP image", &["webp"], "image/webp"),
    &typed("BMP image", &["bmp"], "image/bmp"),
    &typed("icon", &["ico"], "image/vnd.microsoft.icon"),
    &typed("TIFF image", &["tif", "tiff"], "image/tiff"),
    &typed("PDF document", &["pdf"], "application/pdf"),
    &typed("ZIP archive", &["zip"], "application/zip"),
    &typed("gzip archive", &["gz", "tgz"], "application/gzip"),
    &typed("tar archive", &["tar"], "application/x-tar"),
    &typed("Python wheel", &["whl"], "application/zip"),
    &typed("Java archive", &["jar"], "application/java-archive"),
    &extension("font", &["woff", "woff2", "ttf", "otf", "eot"]),
    &extension("sound recording", &["mp3", "wav", "flac", "ogg"]),
    &extension("video", &["mp4", "mov", "webm", "mkv"]),
    &typed("WebAssembly module", &["wasm"], "application/wasm"),
    &extension("compiled library", &["so", "dylib", "dll", "a", "o"]),
    &extension("executable", &["exe"]),
    &extension("Python bytecode", &["pyc"]),
    &extension("Java class", &["class"]),
    &extension("database", &["db", "sqlite", "sqlite3"]),
    &extension("Python pickle", &["pkl", "pickle"]),
    &extension("NumPy array", &["npy", "npz"]),
    &extension("Parquet table", &["parquet"]),
];

/// The formats of an entity held as text, in the order they are asked: a
/// structure the indexer recognizes, then a whole name, then an extension.
pub static TEXT: &[&dyn KindDetector] = &[
    &Structured,
    &Licence,
    &Named {
        kind: "ignore rules",
        names: &[
            ".gitignore",
            ".dockerignore",
            ".npmignore",
            ".kinignore",
            ".eslintignore",
            ".prettierignore",
        ],
    },
    &Named {
        kind: "Git attributes",
        names: &[".gitattributes"],
    },
    &Named {
        kind: "Git submodule list",
        names: &[".gitmodules"],
    },
    &Named {
        kind: "editor settings",
        names: &[".editorconfig"],
    },
    &Named {
        kind: "code owners",
        names: &["codeowners"],
    },
    &Named {
        kind: "contributor list",
        names: &["authors", "contributors", "maintainers"],
    },
    &extension("Markdown", &["md", "markdown", "mdx"]),
    &extension("reStructuredText", &["rst"]),
    &extension("AsciiDoc", &["adoc", "asciidoc"]),
    &extension("plain text", &["txt", "text"]),
    &extension("TOML", &["toml"]),
    &extension("JSON", &["json", "jsonc", "json5"]),
    &extension("YAML", &["yaml", "yml"]),
    &extension("configuration", &["ini", "cfg", "conf", "properties"]),
    &extension("environment settings", &["env"]),
    &extension("HTML", &["html", "htm", "xhtml"]),
    &extension(
        "template",
        &[
            "jinja", "jinja2", "j2", "mustache", "hbs", "ejs", "erb", "in", "tmpl", "template",
        ],
    ),
    &extension("CSS", &["css"]),
    &extension("stylesheet", &["scss", "sass", "less"]),
    &extension("SVG image", &["svg"]),
    &extension("XML", &["xml", "xsd", "xsl", "plist"]),
    &extension("SQL", &["sql"]),
    &extension("shell script", &["sh", "bash", "zsh", "fish"]),
    &extension("Windows script", &["ps1", "bat", "cmd"]),
    &extension("CSV", &["csv"]),
    &extension("TSV", &["tsv"]),
    &extension("dependency lock", &["lock"]),
    &extension("Protocol Buffers schema", &["proto"]),
    &extension("GraphQL schema", &["graphql", "gql"]),
    &extension("Jupyter notebook", &["ipynb"]),
    &extension("LaTeX", &["tex", "bib"]),
    &extension("translation catalogue", &["po", "pot"]),
    &extension("patch", &["patch", "diff"]),
    &extension("type marker", &["typed"]),
];

/// Describe the entity named `name` projected at `place`: `text` says
/// whether its content is text, and `head` is its leading bytes. The first
/// detector of the registry for its level that recognizes it says what it
/// is; one no detector recognizes is `binary` or `text`.
pub fn describe(place: &str, name: &str, text: bool, head: &[u8]) -> Described {
    let probe = Probe { place, name, head };
    let (registry, otherwise) = if text {
        (TEXT, "text")
    } else {
        (BYTES, "binary")
    };
    registry
        .iter()
        .find_map(|detector| detector.detect(&probe))
        .unwrap_or_else(|| Described::of(otherwise))
}

/// The media type of content that is not text, named `name` and projected
/// at `place`, with `head` its leading bytes: the type of the format a
/// detector recognizes it as, or [`UNKNOWN_BYTES`].
pub fn media_type(place: &str, name: &str, head: &[u8]) -> &'static str {
    describe(place, name, false, head)
        .media_type
        .unwrap_or(UNKNOWN_BYTES)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        bytes.extend_from_slice(&width.to_be_bytes());
        bytes.extend_from_slice(&height.to_be_bytes());
        bytes.extend_from_slice(&[8, 2, 0, 0, 0]);
        bytes
    }

    #[test]
    fn content_that_names_its_format_is_described_by_it() {
        assert_eq!(
            describe("docs/logo.png", "logo.png", false, &png(500, 200)),
            Described {
                kind: "PNG image".into(),
                detail: Some("500 by 200 pixels".into()),
                media_type: Some("image/png"),
            }
        );
        // The content decides, not the name.
        assert_eq!(
            describe("docs/logo.dat", "logo.dat", false, &png(16, 8)).kind,
            "PNG image"
        );
        let mut gif = b"GIF89a".to_vec();
        gif.extend_from_slice(&[32, 0, 16, 0, 0, 0, 0]);
        assert_eq!(
            describe("a.gif", "a.gif", false, &gif).detail.as_deref(),
            Some("32 by 16 pixels")
        );
        let jpeg = [
            0xFF, 0xD8, 0xFF, 0xE0, 0, 4, 0, 0, 0xFF, 0xC0, 0, 11, 8, 0, 100, 0, 200, 3, 0, 0, 0,
        ];
        assert_eq!(
            describe("a.jpg", "a.jpg", false, &jpeg),
            Described {
                kind: "JPEG image".into(),
                detail: Some("200 by 100 pixels".into()),
                media_type: Some("image/jpeg"),
            }
        );
        assert_eq!(
            describe("a.bin", "a.bin", false, b"%PDF-1.7\n").kind,
            "PDF document"
        );
    }

    #[test]
    fn bytes_with_no_signature_are_described_by_their_name_or_as_binary() {
        assert_eq!(
            describe("fonts/a.woff2", "a.woff2", false, &[1, 2, 3, 0]).kind,
            "font"
        );
        assert_eq!(
            describe("data/a.blob", "a.blob", false, &[1, 2, 3, 0]).kind,
            "binary"
        );
    }

    #[test]
    fn text_is_described_by_its_structure_or_its_name() {
        for (place, kind) in [
            ("LICENSE", "licence"),
            ("LICENSE.txt", "licence"),
            ("COPYING", "licence"),
            ("README.md", "Markdown"),
            ("CHANGES.rst", "reStructuredText"),
            ("pyproject.toml", "package manifest"),
            ("config/settings.toml", "TOML"),
            ("Dockerfile", "Dockerfile"),
            ("Makefile", "Makefile"),
            (".github/workflows/ci.yml", "CI configuration"),
            ("src/shop/schema.sql", "SQL"),
            ("src/shop/templates/index.html", "HTML"),
            (".gitignore", "ignore rules"),
            ("src/shop/py.typed", "type marker"),
            ("NOTES", "text"),
        ] {
            let name = place.rsplit('/').next().unwrap();
            assert_eq!(describe(place, name, true, b"").kind, kind, "{place}");
        }
    }
}
