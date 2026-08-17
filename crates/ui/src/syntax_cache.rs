//! Bounded cache for neutral syntax documents.
//!
//! Colors and GPUI runs deliberately stay outside this cache so appearance
//! changes recolor existing spans without parsing again.

use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
};

use sha2::{Digest, Sha256};
use zeron_syntax::{HighlightedDocument, LanguageId};

pub const QUERY_GENERATION: u32 = 1;
const MAX_DOCUMENTS: usize = 96;
const MAX_RETAINED_BYTES: usize = 24 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DocumentHighlightKey {
    pub language: LanguageId,
    pub content_hash: [u8; 32],
    pub query_generation: u32,
}

impl DocumentHighlightKey {
    pub fn new(language: LanguageId, source: &str) -> Self {
        Self {
            language,
            content_hash: Sha256::digest(source.as_bytes()).into(),
            query_generation: QUERY_GENERATION,
        }
    }
}

struct CachedDocument {
    retained_bytes: usize,
    document: Arc<HighlightedDocument>,
}

#[derive(Default)]
pub struct SyntaxHighlightCache {
    documents: HashMap<DocumentHighlightKey, CachedDocument>,
    recency: VecDeque<DocumentHighlightKey>,
    retained_bytes: usize,
    hits: u64,
    misses: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyntaxCacheStats {
    pub hits: u64,
    pub misses: u64,
    pub documents: usize,
    pub retained_bytes: usize,
}

impl SyntaxHighlightCache {
    pub fn get(&mut self, key: &DocumentHighlightKey) -> Option<Arc<HighlightedDocument>> {
        let Some(document) = self.documents.get(key).map(|entry| entry.document.clone()) else {
            self.misses += 1;
            return None;
        };
        self.hits += 1;
        self.touch(*key);
        Some(document)
    }

    pub fn insert(
        &mut self,
        key: DocumentHighlightKey,
        document: Arc<HighlightedDocument>,
    ) -> bool {
        if let Some(previous) = self.documents.remove(&key) {
            self.retained_bytes = self.retained_bytes.saturating_sub(previous.retained_bytes);
        }
        let retained_bytes = estimated_document_bytes(&document);
        if retained_bytes > MAX_RETAINED_BYTES {
            self.recency.retain(|candidate| *candidate != key);
            return false;
        }
        self.retained_bytes = self.retained_bytes.saturating_add(retained_bytes);
        self.documents.insert(
            key,
            CachedDocument {
                retained_bytes,
                document,
            },
        );
        self.touch(key);
        while self.documents.len() > MAX_DOCUMENTS || self.retained_bytes > MAX_RETAINED_BYTES {
            let Some(oldest) = self.recency.pop_front() else {
                break;
            };
            if let Some(removed) = self.documents.remove(&oldest) {
                self.retained_bytes = self.retained_bytes.saturating_sub(removed.retained_bytes);
            }
        }
        self.documents.contains_key(&key)
    }

    fn touch(&mut self, key: DocumentHighlightKey) {
        self.recency.retain(|candidate| *candidate != key);
        self.recency.push_back(key);
    }

    pub fn stats(&self) -> SyntaxCacheStats {
        SyntaxCacheStats {
            hits: self.hits,
            misses: self.misses,
            documents: self.documents.len(),
            retained_bytes: self.retained_bytes,
        }
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.documents.len()
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.documents.is_empty()
    }
}

fn estimated_document_bytes(document: &HighlightedDocument) -> usize {
    std::mem::size_of::<HighlightedDocument>()
        .saturating_add(
            document
                .lines
                .capacity()
                .saturating_mul(std::mem::size_of::<Vec<zeron_syntax::HighlightSpan>>()),
        )
        .saturating_add(document.lines.iter().fold(0usize, |total, line| {
            total.saturating_add(
                line.capacity()
                    .saturating_mul(std::mem::size_of::<zeron_syntax::HighlightSpan>()),
            )
        }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_depends_on_content_language_and_query_not_theme() {
        let rust = DocumentHighlightKey::new(LanguageId::Rust, "fn main() {}");
        assert_eq!(
            rust,
            DocumentHighlightKey::new(LanguageId::Rust, "fn main() {}")
        );
        assert_ne!(
            rust,
            DocumentHighlightKey::new(LanguageId::Rust, "fn other() {}")
        );
        assert_ne!(
            rust,
            DocumentHighlightKey::new(LanguageId::TypeScript, "fn main() {}")
        );
    }

    #[test]
    fn cache_reuses_neutral_documents() {
        let source = "fn main() {}";
        let key = DocumentHighlightKey::new(LanguageId::Rust, source);
        let document = Arc::new(
            zeron_syntax::highlight(zeron_syntax::HighlightRequest {
                source,
                path: None,
                fence_tag: Some("rust"),
            })
            .unwrap(),
        );
        let mut cache = SyntaxHighlightCache::default();
        assert!(cache.insert(key, document.clone()));
        assert!(Arc::ptr_eq(&cache.get(&key).unwrap(), &document));
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.stats().hits, 1);
        assert_eq!(cache.stats().misses, 0);
    }

    #[test]
    fn retained_bytes_measure_materialized_spans_not_source_text() {
        let source = "let x = 1;";
        let key = DocumentHighlightKey::new(LanguageId::Rust, source);
        let document = Arc::new(
            zeron_syntax::highlight(zeron_syntax::HighlightRequest {
                source,
                path: None,
                fence_tag: Some("rust"),
            })
            .unwrap(),
        );
        let mut cache = SyntaxHighlightCache::default();
        assert!(cache.insert(key, document));
        assert!(cache.stats().retained_bytes > source.len());
    }
}
