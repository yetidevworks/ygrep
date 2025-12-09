//! LRU cache for embeddings to avoid re-computation

use std::num::NonZeroUsize;
use lru::LruCache;
use parking_lot::Mutex;
use xxhash_rust::xxh3::xxh3_64;

/// LRU cache for computed embeddings
pub struct EmbeddingCache {
    cache: Mutex<LruCache<u64, Vec<f32>>>,
    hits: std::sync::atomic::AtomicU64,
    misses: std::sync::atomic::AtomicU64,
}

impl EmbeddingCache {
    /// Create a new embedding cache
    ///
    /// # Arguments
    /// * `capacity_mb` - Maximum cache size in megabytes
    /// * `dimension` - Embedding dimension (to calculate entry size)
    pub fn new(capacity_mb: usize, dimension: usize) -> Self {
        // Calculate number of embeddings that fit in cache
        // Each embedding is dimension * 4 bytes (f32)
        let embedding_size = dimension * std::mem::size_of::<f32>();
        let capacity = (capacity_mb * 1024 * 1024) / embedding_size;
        let capacity = NonZeroUsize::new(capacity.max(100)).unwrap();

        Self {
            cache: Mutex::new(LruCache::new(capacity)),
            hits: std::sync::atomic::AtomicU64::new(0),
            misses: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Get an embedding from cache
    pub fn get(&self, text: &str) -> Option<Vec<f32>> {
        let key = xxh3_64(text.as_bytes());
        let mut cache = self.cache.lock();

        if let Some(embedding) = cache.get(&key) {
            self.hits.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Some(embedding.clone())
        } else {
            self.misses.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            None
        }
    }

    /// Insert an embedding into cache
    pub fn insert(&self, text: &str, embedding: Vec<f32>) {
        let key = xxh3_64(text.as_bytes());
        let mut cache = self.cache.lock();
        cache.put(key, embedding);
    }

    /// Get or compute an embedding
    ///
    /// Returns cached embedding if available, otherwise computes using the provided function
    pub fn get_or_insert<F>(&self, text: &str, compute: F) -> Vec<f32>
    where
        F: FnOnce() -> Vec<f32>,
    {
        if let Some(embedding) = self.get(text) {
            return embedding;
        }

        let embedding = compute();
        self.insert(text, embedding.clone());
        embedding
    }

    /// Get cache statistics
    pub fn stats(&self) -> CacheStats {
        let hits = self.hits.load(std::sync::atomic::Ordering::Relaxed);
        let misses = self.misses.load(std::sync::atomic::Ordering::Relaxed);
        let total = hits + misses;

        CacheStats {
            hits,
            misses,
            hit_rate: if total > 0 {
                hits as f64 / total as f64
            } else {
                0.0
            },
            size: self.cache.lock().len(),
        }
    }

    /// Clear the cache
    pub fn clear(&self) {
        self.cache.lock().clear();
    }
}

/// Cache statistics
#[derive(Debug, Clone)]
pub struct CacheStats {
    pub hits: u64,
    pub misses: u64,
    pub hit_rate: f64,
    pub size: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cache_operations() {
        let cache = EmbeddingCache::new(1, 384); // 1MB cache

        // Insert
        let embedding = vec![0.1f32; 384];
        cache.insert("hello", embedding.clone());

        // Get
        let retrieved = cache.get("hello");
        assert!(retrieved.is_some());
        assert_eq!(retrieved.unwrap(), embedding);

        // Miss
        let missed = cache.get("world");
        assert!(missed.is_none());

        // Stats
        let stats = cache.stats();
        assert_eq!(stats.hits, 1);
        assert_eq!(stats.misses, 1);
        assert_eq!(stats.size, 1);
    }

    #[test]
    fn test_get_or_insert() {
        let cache = EmbeddingCache::new(1, 384);

        let mut computed = false;
        let embedding = cache.get_or_insert("test", || {
            computed = true;
            vec![0.5f32; 384]
        });

        assert!(computed);
        assert_eq!(embedding.len(), 384);

        // Second call should use cache
        computed = false;
        let embedding2 = cache.get_or_insert("test", || {
            computed = true;
            vec![0.0f32; 384]
        });

        assert!(!computed);
        assert_eq!(embedding2, embedding);
    }
}
