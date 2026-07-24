//! `tpt-archon-relational`: AI-native relational query engine.
//!
//! Phase 3 of the `tpt-archon` stack. A PostgreSQL-dialect SQL [`parser`],
//! cost-based [`planner`], vectorized [`executor`], and [`mvcc`] concurrency
//! control, all running on the lower `tpt-archon` layers. GPU acceleration is
//! opt-in behind the `gpu` feature; the executor has a CPU-only fallback.
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

pub mod database;
pub mod executor;
pub mod explain;
pub mod mvcc;
pub mod parser;
pub mod planner;
pub mod vector_index;

#[cfg(feature = "gpu")]
pub mod gpu;
