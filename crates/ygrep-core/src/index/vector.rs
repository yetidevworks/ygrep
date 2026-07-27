//! HNSW vector index for semantic search

use hnsw_rs::hnswio::HnswIo;
use hnsw_rs::prelude::*;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::error::{Result, YgrepError};

/// HNSW dump file basename
const HNSW_BASENAME: &str = "hnsw";

/// Compact doc_id index (fast to load)
#[derive(Debug, Serialize, Deserialize)]
struct DocIdIndex {
    dimension: usize,
    doc_ids: Vec<String>,
}

/// Stored vector with its document ID (legacy format)
#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredVector {
    doc_id: String,
    vector: Vec<f32>,
}

/// Persistent data for vector index (legacy format - slow to load)
#[derive(Debug, Serialize, Deserialize)]
struct VectorData {
    dimension: usize,
    vectors: Vec<StoredVector>,
}

/// HNSW vector index for storing and searching embeddings
pub struct VectorIndex {
    path: PathBuf,
    hnsw: RwLock<Hnsw<'static, f32, DistCosine>>,
    dimension: usize,
    /// Document IDs (index matches HNSW point ID)
    doc_ids: RwLock<Vec<String>>,
    /// Where each document ID sits in `doc_ids`.
    ///
    /// Deleting used to scan the whole vector: a branch switch that removed five
    /// thousand files from a two hundred thousand vector index compared a billion
    /// strings to do it.
    slots: RwLock<HashMap<String, usize>>,
    /// The reloader `hnsw` was read through.
    ///
    /// `load_hnsw` hands back a graph borrowed from its reloader, so the reloader has to
    /// outlive it. It used to be leaked to arrange that, and the dashboard re-opens every
    /// watched workspace after each sleep, so a long session leaked one reloader per
    /// wake. Owning it here frees it with the index instead: `hnsw` is declared before
    /// this field, and fields drop in declaration order, so the graph goes first.
    _reloader: Option<Box<HnswIo>>,
}

/// Read a JSON file, treating any failure as "nothing there"
fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    let file = std::fs::File::open(path).ok()?;
    serde_json::from_reader(std::io::BufReader::new(file)).ok()
}

/// Map document IDs to their position, skipping the blanks left by soft deletes
fn slot_map(doc_ids: &[String]) -> HashMap<String, usize> {
    let mut slots = HashMap::with_capacity(doc_ids.len());
    for (slot, doc_id) in doc_ids.iter().enumerate() {
        if doc_id.is_empty() {
            continue;
        }
        // The first slot wins, matching the scan this replaced: duplicate content
        // hashes to one document ID, so the same ID can land in several slots.
        slots.entry(doc_id.clone()).or_insert(slot);
    }
    slots
}

impl VectorIndex {
    /// Create a new vector index
    pub fn new(path: PathBuf, dimension: usize) -> Result<Self> {
        std::fs::create_dir_all(&path)?;
        Ok(Self::empty(path, dimension))
    }

    /// Create an empty in-memory vector index without touching the filesystem.
    /// Used for read-only opens when no semantic index exists on disk.
    pub fn empty(path: PathBuf, dimension: usize) -> Self {
        // HNSW parameters:
        // - max_nb_connection (M): 16 is a good default
        // - max_elements: Initial capacity, will grow
        // - max_layer: log2(max_elements) is optimal
        // - ef_construction: Higher = better quality, slower build
        let hnsw = Hnsw::new(
            16,     // max_nb_connection (M)
            10_000, // initial capacity
            16,     // max_layer
            200,    // ef_construction
            DistCosine {},
        );

        Self {
            path,
            hnsw: RwLock::new(hnsw),
            dimension,
            doc_ids: RwLock::new(Vec::new()),
            slots: RwLock::new(HashMap::new()),
            _reloader: None,
        }
    }

    /// Load an existing vector index
    pub fn load(path: PathBuf) -> Result<Self> {
        // Try fast path: load from doc_ids.json + HNSW dump
        let doc_ids_path = path.join("doc_ids.json");
        let hnsw_graph = path.join(format!("{}.hnsw.graph", HNSW_BASENAME));

        if doc_ids_path.exists() && hnsw_graph.exists() {
            // Fast path: load compact doc_id index + HNSW dump
            let doc_index: DocIdIndex =
                serde_json::from_reader(std::fs::File::open(&doc_ids_path)?).map_err(|e| {
                    YgrepError::Config(format!("Failed to load doc_id index: {}", e))
                })?;

            let mut reloader = Box::new(HnswIo::new(&path, HNSW_BASENAME));
            let reloader_ptr: *mut HnswIo = &mut *reloader;
            // SAFETY: the reloader is boxed, so its address stays put, and it is stored
            // in the struct built below and dropped after the graph that borrows it.
            let hnsw = unsafe { (*reloader_ptr).load_hnsw::<f32, DistCosine>() }
                .map_err(|e| YgrepError::Config(format!("Failed to load HNSW index: {}", e)))?;

            return Ok(Self {
                path,
                hnsw: RwLock::new(hnsw),
                dimension: doc_index.dimension,
                slots: RwLock::new(slot_map(&doc_index.doc_ids)),
                doc_ids: RwLock::new(doc_index.doc_ids),
                _reloader: Some(reloader),
            });
        }

        // Fallback: load from legacy vectors.json
        let data_path = path.join("vectors.json");
        if !data_path.exists() {
            return Err(YgrepError::WorkspaceNotIndexed(path.clone()));
        }

        // Load legacy vector data (slow but backwards compatible)
        let data: VectorData = serde_json::from_reader(std::fs::File::open(&data_path)?)
            .map_err(|e| YgrepError::Config(format!("Failed to load vector data: {}", e)))?;

        // Extract doc_ids from vectors
        let doc_ids: Vec<String> = data.vectors.iter().map(|sv| sv.doc_id.clone()).collect();

        // Rebuild HNSW from vectors
        let hnsw = Hnsw::new(16, data.vectors.len().max(10_000), 16, 200, DistCosine {});
        for (id, sv) in data.vectors.iter().enumerate() {
            hnsw.insert((&sv.vector, id));
        }

        Ok(Self {
            path,
            hnsw: RwLock::new(hnsw),
            dimension: data.dimension,
            slots: RwLock::new(slot_map(&doc_ids)),
            doc_ids: RwLock::new(doc_ids),
            _reloader: None,
        })
    }

    /// How many vectors a saved index holds, read without building the graph.
    ///
    /// Answering "is semantic search available?" used to mean loading the whole HNSW
    /// graph, on every workspace open, including runs that never search a vector.
    pub fn stored_len(path: &Path) -> usize {
        #[derive(Deserialize)]
        struct DocIdCount {
            doc_ids: Vec<serde::de::IgnoredAny>,
        }

        #[derive(Deserialize)]
        struct VectorCount {
            vectors: Vec<serde::de::IgnoredAny>,
        }

        let doc_ids_path = path.join("doc_ids.json");
        if doc_ids_path.exists() && path.join(format!("{}.hnsw.graph", HNSW_BASENAME)).exists() {
            return read_json(&doc_ids_path)
                .map(|counted: DocIdCount| counted.doc_ids.len())
                .unwrap_or(0);
        }

        read_json(&path.join("vectors.json"))
            .map(|counted: VectorCount| counted.vectors.len())
            .unwrap_or(0)
    }

    /// Check if a vector index exists at the path
    pub fn exists(path: &Path) -> bool {
        // Check for new format (doc_ids.json + HNSW dump) or legacy format (vectors.json)
        let new_format = path.join("doc_ids.json").exists()
            && path.join(format!("{}.hnsw.graph", HNSW_BASENAME)).exists();
        let legacy_format = path.join("vectors.json").exists();
        new_format || legacy_format
    }

    /// Insert an embedding and return its ID
    pub fn insert(&self, doc_id: &str, embedding: &[f32]) -> Result<u64> {
        if embedding.len() != self.dimension {
            return Err(YgrepError::Config(format!(
                "Embedding dimension mismatch: expected {}, got {}",
                self.dimension,
                embedding.len()
            )));
        }

        let mut doc_ids = self.doc_ids.write();
        let id = doc_ids.len();

        // Store the doc_id
        doc_ids.push(doc_id.to_string());
        self.slots.write().entry(doc_id.to_string()).or_insert(id);

        // Insert into HNSW
        let hnsw = self.hnsw.write();
        hnsw.insert((embedding, id));

        Ok(id as u64)
    }

    /// Search for similar vectors
    ///
    /// Returns (vector_id, distance, doc_id) tuples, sorted by distance (ascending)
    pub fn search(&self, query: &[f32], k: usize) -> Result<Vec<(u64, f32, String)>> {
        if query.len() != self.dimension {
            return Err(YgrepError::Config(format!(
                "Query dimension mismatch: expected {}, got {}",
                self.dimension,
                query.len()
            )));
        }

        let hnsw = self.hnsw.read();
        let doc_ids = self.doc_ids.read();

        if doc_ids.is_empty() {
            return Ok(vec![]);
        }

        // ef_search should be >= k, higher = better recall
        let ef_search = k.max(30);
        let neighbors = hnsw.search(query, k, ef_search);

        Ok(neighbors
            .into_iter()
            .filter_map(|n| {
                doc_ids.get(n.d_id).and_then(|doc_id| {
                    if doc_id.is_empty() {
                        None // soft-deleted
                    } else {
                        Some((n.d_id as u64, n.distance, doc_id.clone()))
                    }
                })
            })
            .collect())
    }

    /// Save the index to disk
    pub fn save(&self) -> Result<()> {
        // Save compact doc_id index (fast to load)
        let doc_ids_path = self.path.join("doc_ids.json");
        let doc_ids = self.doc_ids.read();
        let doc_index = DocIdIndex {
            dimension: self.dimension,
            doc_ids: doc_ids.clone(),
        };
        serde_json::to_writer(std::fs::File::create(&doc_ids_path)?, &doc_index)
            .map_err(|e| YgrepError::Config(format!("Failed to save doc_id index: {}", e)))?;

        // Save HNSW graph for fast loading
        let hnsw = self.hnsw.read();
        hnsw.file_dump(&self.path, HNSW_BASENAME)
            .map_err(|e| YgrepError::Config(format!("Failed to save HNSW index: {}", e)))?;

        Ok(())
    }

    /// Get the number of vectors in the index
    pub fn len(&self) -> usize {
        self.doc_ids.read().len()
    }

    /// Check if the index is empty
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Get the embedding dimension
    pub fn dimension(&self) -> usize {
        self.dimension
    }

    /// Soft-delete an entry by doc_id.
    /// Sets the doc_id to empty string so it's filtered out during search.
    /// The HNSW point remains but is effectively invisible.
    /// Returns true if the doc_id was found and marked deleted.
    pub fn mark_deleted(&self, doc_id: &str) -> bool {
        // Always `doc_ids` before `slots`, the order `insert` takes them in.
        let mut doc_ids = self.doc_ids.write();
        let Some(slot) = self.slots.write().remove(doc_id) else {
            return false;
        };

        match doc_ids.get_mut(slot) {
            Some(entry) => {
                entry.clear();
                true
            }
            None => false,
        }
    }

    /// Clear the index
    pub fn clear(&self) {
        let mut hnsw = self.hnsw.write();
        *hnsw = Hnsw::new(16, 10_000, 16, 200, DistCosine {});
        self.doc_ids.write().clear();
        self.slots.write().clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_vector_index_basic() -> Result<()> {
        let temp_dir = tempdir().unwrap();
        let index = VectorIndex::new(temp_dir.path().to_path_buf(), 4)?;

        // Insert some vectors
        let v1 = vec![1.0, 0.0, 0.0, 0.0];
        let v2 = vec![0.0, 1.0, 0.0, 0.0];
        let v3 = vec![0.9, 0.1, 0.0, 0.0]; // Similar to v1

        index.insert("doc1", &v1)?;
        index.insert("doc2", &v2)?;
        index.insert("doc3", &v3)?;

        assert_eq!(index.len(), 3);

        // Search for vectors similar to v1
        let results = index.search(&v1, 2)?;
        assert_eq!(results.len(), 2);

        // Results should include doc1 and doc3 (most similar to v1)
        let doc_ids: Vec<_> = results.iter().map(|(_, _, id)| id.as_str()).collect();
        assert!(doc_ids.contains(&"doc1"));

        Ok(())
    }

    #[test]
    fn deleted_entries_disappear_from_search_and_survive_a_reload() -> Result<()> {
        let temp_dir = tempdir().unwrap();
        let path = temp_dir.path().to_path_buf();

        let index = VectorIndex::new(path.clone(), 4)?;
        index.insert("doc1", &[1.0, 0.0, 0.0, 0.0])?;
        index.insert("doc2", &[0.0, 1.0, 0.0, 0.0])?;

        assert!(index.mark_deleted("doc1"));
        assert!(!index.mark_deleted("doc1"), "a second delete finds nothing");
        assert!(!index.mark_deleted("missing"));

        let hits = index.search(&[1.0, 0.0, 0.0, 0.0], 2)?;
        assert!(hits.iter().all(|(_, _, doc_id)| doc_id != "doc1"));

        // A reload rebuilds the slot map, so deleting still works afterwards.
        index.save()?;
        let reloaded = VectorIndex::load(path)?;
        assert!(reloaded.mark_deleted("doc2"));
        assert!(reloaded.search(&[0.0, 1.0, 0.0, 0.0], 2)?.is_empty());

        Ok(())
    }

    #[test]
    fn test_vector_index_save_load() -> Result<()> {
        let temp_dir = tempdir().unwrap();
        let path = temp_dir.path().to_path_buf();

        // Create and populate index
        {
            let index = VectorIndex::new(path.clone(), 4)?;
            index.insert("doc1", &[1.0, 0.0, 0.0, 0.0])?;
            index.insert("doc2", &[0.0, 1.0, 0.0, 0.0])?;
            index.save()?;
        }

        // Load and verify
        {
            let index = VectorIndex::load(path)?;
            assert_eq!(index.len(), 2);
            assert_eq!(index.dimension(), 4);

            // Search should work
            let results = index.search(&[1.0, 0.0, 0.0, 0.0], 1)?;
            assert_eq!(results.len(), 1);
            assert_eq!(results[0].2, "doc1");
        }

        Ok(())
    }
}
