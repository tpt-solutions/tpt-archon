//! `tpt-archon-kernel`: capability-based microkernel (user-space first).
//!
//! Phase 2b of the `tpt-archon` stack. "Microkernel" here means a user-space
//! process model on top of a host OS first; bare-metal comes later. Provides an
//! async task [`scheduler`], capability-bearing [`ipc`], and page-cache-aware
//! [`memory`] management. Depends on `tpt-archon-core` + `tpt-archon-bridge`.
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

pub mod ipc;
pub mod memory;
pub mod scheduler;

#[cfg(all(feature = "io-uring-backend", target_os = "linux"))]
pub mod io_uring_backend;
