# Changelog

All notable changes to this crate are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
### Changed
### Fixed

## [0.1.0] - 2026-08-05

### Added
- Crate scaffold (Phase 2a of tpt-archon): zero-copy IPC and unified memory
  management gluing `tpt-archon-core` to the kernel.
- `capability` module — strongly-typed, unforgeable, revocable `Capability`
  tokens minted by a `CapabilityIssuer`, granting a `Right` (`Read`/`Write`/
  `ReadWrite`) over a `Resource` (page or channel).
- `page_cache` module — the `UnifiedPageCache` trait that lets the kernel map
  storage pages directly into the database's address space with no
  double-buffering, plus `CorePageCache`, which adapts the core buffer pool to
  it (verified by a zero-copy integration test).
- `grant` module — `CapabilityGrant`, a thin safe layer over `UnifiedPageCache`
  that bundles the capability check and the page borrow into one call, returning
  a `MemoryView`/`MemoryViewMut` (no new `unsafe`; backed by the same safe borrows
  `map_read`/`map_write` already return).
