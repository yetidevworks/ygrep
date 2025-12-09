//! HNSW vector index for semantic search

use std::path::{Path, PathBuf};
use parking_lot::RwLock;
use hnsw_rs::prelude::*;
use serde::{Deserialize, Serialize};

use crate::error::{Result, YgrepError};

/// Stored vector with its document ID
#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredVector {
    doc_id: String,
    vector: Vec<f32>,
}

/// Persistent data for vector index
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
    /// Stored vectors (for persistence)
    vectors: RwLock<Vec<StoredVector>>,
}

impl VectorIndex {
    /// Create a new vector index
    pub fn new(path: PathBuf, dimension: usize) -> Result<Self> {
        std::fs::create_dir_all(&path)?;

        // HNSW parameters:
        // - max_nb_connection (M): 16 is a good default
        // - max_elements: Initial capacity, will grow
        // - max_layer: log2(max_elements) is optimal
        // - ef_construction: Higher = better quality, slower build
        let hnsw = Hnsw::new(
            16,         // max_nb_connection (M)
            10_000,     // initial capacity
            16,         // max_layer
            200,        // ef_construction
            DistCosine {},
        );

        Ok(Self {
            path,
            hnsw: RwLock::new(hnsw),
            dimension,
            vectors: RwLock::new(Vec::new()),
        })
    }

    /// Load an existing vector index
    pub fn load(path: PathBuf) -> Result<Self> {
        let data_path = path.join("vectors.json");

        if !data_path.exists() {
            return Err(YgrepError::WorkspaceNotIndexed(path));
        }

        // Load vector data
        let data: VectorData = serde_json::from_reader(
            std::fs::File::open(&data_path)?
        ).map_err(|e| YgrepError::Config(format!("Failed to load vector data: {}", e)))?;

        // Create HNSW index
        let hnsw = Hnsw::new(16, data.vectors.len().max(10_000), 16, 200, DistCosine {});

        // Rebuild index from stored vectors
        for (id, sv) in data.vectors.iter().enumerate() {
            hnsw.insert((&sv.vector, id));
        }

        Ok(Self {
            path,
            hnsw: RwLock::new(hnsw),
            dimension: data.dimension,
            vectors: RwLock::new(data.vectors),
        })
    }

    /// Check if a vector index exists at the path
    pub fn exists(path: &Path) -> bool {
        path.join("vectors.json").exists()
    }

    /// Insert an embedding and return its ID
    pub fn insert(&self, doc_id: &str, embedding: &[f32]) -> Result<u64> {
        if embedding.len() != self.dimension {
            return Err(YgrepError::Config(format!(
                "Embedding dimension mismatch: expected {}, got {}",
                self.dimension, embedding.len()
            )));
        }

        let mut vectors = self.vectors.write();
        let id = vectors.len();

        // Store the vector
        vectors.push(StoredVector {
            doc_id: doc_id.to_string(),
            vector: embedding.to_vec(),
        });

        // Insert into HNSW
        let hnsw = self.hnsw.write();
        hnsw.insert((&embedding.to_vec(), id));

        Ok(id as u64)
    }

    /// Search for similar vectors
    ///
    /// Returns (vector_id, distance, doc_id) tuples, sorted by distance (ascending)
    pub fn search(&self, query: &[f32], k: usize) -> Result<Vec<(u64, f32, String)>> {
        if query.len() != self.dimension {
            return Err(YgrepError::Config(format!(
                "Query dimension mismatch: expected {}, got {}",
                self.dimension, query.len()
            )));
        }

        let hnsw = self.hnsw.read();
        let vectors = self.vectors.read();

        if vectors.is_empty() {
            return Ok(vec![]);
        }

        // ef_search should be >= k, higher = better recall
        let ef_search = k.max(30);
        let neighbors = hnsw.search(query, k, ef_search);

        Ok(neighbors
            .into_iter()
            .filter_map(|n| {
                vectors.get(n.d_id).map(|sv| {
                    (n.d_id as u64, n.distance, sv.doc_id.clone())
                })
            })
            .collect())
    }

    /// Save the index to disk
    pub fn save(&self) -> Result<()> {
        let data_path = self.path.join("vectors.json");

        let vectors = self.vectors.read();
        let data = VectorData {
            dimension: self.dimension,
            vectors: vectors.clone(),
        };

        serde_json::to_writer(
            std::fs::File::create(&data_path)?,
            &data,
        ).map_err(|e| YgrepError::Config(format!("Failed to save vector data: {}", e)))?;

        Ok(())
    }

    /// Get the number of vectors in the index
    pub fn len(&self) -> usize {
        self.vectors.read().len()
    }

    /// Check if the index is empty
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Get the embedding dimension
    pub fn dimension(&self) -> usize {
        self.dimension
    }

    /// Clear the index
    pub fn clear(&self) {
        let mut hnsw = self.hnsw.write();
        *hnsw = Hnsw::new(16, 10_000, 16, 200, DistCosine {});
        self.vectors.write().clear();
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
