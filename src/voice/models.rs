//! Embedded catalog of canonical whisper.cpp model release artifacts.
//!
//! Sourced from <https://huggingface.co/ggerganov/whisper.cpp>. SHA256
//! checksums are extracted from the upstream Git LFS pointer files.
//!
//! Consumed by:
//!   - the model downloader (Task B2),
//!   - the model browser UI (Task C1).

/// A single whisper.cpp model entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CatalogEntry {
    /// Short identifier, e.g. "base", "base.en", "large-v3-turbo".
    pub id: &'static str,
    /// Upstream filename, e.g. "ggml-base.bin".
    pub filename: &'static str,
    /// Approximate on-disk size in megabytes.
    pub size_mb: u32,
    /// `false` for the English-only `*.en` variants.
    pub multilingual: bool,
    /// Lowercase hex SHA256 of the file (no `sha256:` prefix). Empty if
    /// the upstream pointer could not be fetched at build time.
    pub sha256: &'static str,
}

/// Canonical catalog of whisper.cpp release artifacts.
pub const CATALOG: &[CatalogEntry] = &[
    CatalogEntry {
        id: "tiny",
        filename: "ggml-tiny.bin",
        size_mb: 75,
        multilingual: true,
        sha256: "be07e048e1e599ad46341c8d2a135645097a538221678b7acdd1b1919c6e1b21",
    },
    CatalogEntry {
        id: "tiny.en",
        filename: "ggml-tiny.en.bin",
        size_mb: 75,
        multilingual: false,
        sha256: "921e4cf8686fdd993dcd081a5da5b6c365bfde1162e72b08d75ac75289920b1f",
    },
    CatalogEntry {
        id: "base",
        filename: "ggml-base.bin",
        size_mb: 142,
        multilingual: true,
        sha256: "60ed5bc3dd14eea856493d334349b405782ddcaf0028d4b5df4088345fba2efe",
    },
    CatalogEntry {
        id: "base.en",
        filename: "ggml-base.en.bin",
        size_mb: 142,
        multilingual: false,
        sha256: "a03779c86df3323075f5e796cb2ce5029f00ec8869eee3fdfb897afe36c6d002",
    },
    CatalogEntry {
        id: "small",
        filename: "ggml-small.bin",
        size_mb: 466,
        multilingual: true,
        sha256: "1be3a9b2063867b937e64e2ec7483364a79917e157fa98c5d94b5c1fffea987b",
    },
    CatalogEntry {
        id: "small.en",
        filename: "ggml-small.en.bin",
        size_mb: 466,
        multilingual: false,
        sha256: "c6138d6d58ecc8322097e0f987c32f1be8bb0a18532a3f88f734d1bbf9c41e5d",
    },
    CatalogEntry {
        id: "medium",
        filename: "ggml-medium.bin",
        size_mb: 1500,
        multilingual: true,
        sha256: "6c14d5adee5f86394037b4e4e8b59f1673b6cee10e3cf0b11bbdbee79c156208",
    },
    CatalogEntry {
        id: "medium.en",
        filename: "ggml-medium.en.bin",
        size_mb: 1500,
        multilingual: false,
        sha256: "cc37e93478338ec7700281a7ac30a10128929eb8f427dda2e865faa8f6da4356",
    },
    CatalogEntry {
        id: "large-v3",
        filename: "ggml-large-v3.bin",
        size_mb: 3094,
        multilingual: true,
        sha256: "64d182b440b98d5203c4f9bd541544d84c605196c4f7b845dfa11fb23594d1e2",
    },
    CatalogEntry {
        id: "large-v3-turbo",
        filename: "ggml-large-v3-turbo.bin",
        size_mb: 1624,
        multilingual: true,
        sha256: "1fc70f774d38eb169993ac391eea357ef47c88757ef72ee5943879b7e8e2bc69",
    },
];

/// Look up a catalog entry by its filename (e.g. "ggml-base.bin").
pub fn find_by_filename(filename: &str) -> Option<&'static CatalogEntry> {
    CATALOG.iter().find(|e| e.filename == filename)
}

/// Look up a catalog entry by its short id (e.g. "base.en").
pub fn find_by_id(id: &str) -> Option<&'static CatalogEntry> {
    CATALOG.iter().find(|e| e.id == id)
}

/// Iterate over multilingual entries.
pub fn iter_multilingual() -> impl Iterator<Item = &'static CatalogEntry> {
    CATALOG.iter().filter(|e| e.multilingual)
}

/// Iterate over English-only entries.
pub fn iter_english_only() -> impl Iterator<Item = &'static CatalogEntry> {
    CATALOG.iter().filter(|e| !e.multilingual)
}

/// Build the HuggingFace download URL for a given entry.
pub fn download_url(entry: &CatalogEntry) -> String {
    format!(
        "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/{}",
        entry.filename
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_has_expected_ids() {
        let expected = [
            "tiny",
            "tiny.en",
            "base",
            "base.en",
            "small",
            "small.en",
            "medium",
            "medium.en",
            "large-v3",
            "large-v3-turbo",
        ];
        assert_eq!(CATALOG.len(), expected.len());
        for id in expected {
            assert!(find_by_id(id).is_some(), "missing catalog id: {id}");
        }
    }

    #[test]
    fn find_by_id_works() {
        let entry = find_by_id("base.en").expect("base.en present");
        assert_eq!(entry.filename, "ggml-base.en.bin");
        assert!(!entry.multilingual);
    }

    #[test]
    fn find_by_id_unknown_returns_none() {
        assert!(find_by_id("nonexistent-model").is_none());
    }

    #[test]
    fn find_by_filename_roundtrips() {
        for entry in CATALOG {
            let found = find_by_filename(entry.filename)
                .unwrap_or_else(|| panic!("missing filename: {}", entry.filename));
            assert_eq!(found.id, entry.id);
        }
    }

    #[test]
    fn english_only_entries_are_not_multilingual() {
        for entry in CATALOG {
            if entry.id.ends_with(".en") {
                assert!(
                    !entry.multilingual,
                    "{} is .en but flagged multilingual",
                    entry.id
                );
            }
        }
    }

    #[test]
    fn download_url_format() {
        let entry = find_by_id("base").unwrap();
        assert_eq!(
            download_url(entry),
            "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base.bin"
        );
    }

    #[test]
    fn multilingual_iterator_excludes_en_variants() {
        for entry in iter_multilingual() {
            assert!(
                !entry.id.ends_with(".en"),
                "{} appeared in multilingual iter",
                entry.id
            );
        }
    }
}
