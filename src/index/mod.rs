//! Derived retrieval indexes: `memory_vec` (KNN) and `memory_fts` (BM25). Both are
//! rebuildable from `memory` alone (SPEC §5).

pub mod fts;
pub mod vec;
