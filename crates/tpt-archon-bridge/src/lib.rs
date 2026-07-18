//! `tpt-archon-bridge`: zero-copy IPC and unified page-cache traits.
//!
//! Phase 2a of the `tpt-archon` stack. This crate defines the capability types
//! and the [`page_cache`] trait that let the kernel and the storage engine
//! share pages with no double-buffering. It depends only on
//! `tpt-archon-core`.
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

pub mod capability;
pub mod page_cache;
