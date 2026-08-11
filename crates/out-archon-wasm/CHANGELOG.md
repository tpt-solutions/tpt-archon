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
- Browser playground (`out-archon-wasm`): `wasm-bindgen` glue (`ArchonDb` type)
  wrapping `tpt-archon-relational`'s `Database::empty()` + `parse_statement` +
  `execute`, plus a static page (`www/`) that runs SQL entirely client-side with
  no server.
- `wasm-pack` build (`--target web`) emitting `pkg/` consumed by `www/index.js`
  via a relative import (no bundler step).
- Proven `cargo check -p out-archon-wasm --target wasm32-unknown-unknown`: the
  crate and its `tpt-archon-relational` dependency compile for the target this
  playground ships to. Covered by the repo's `wasm` CI job and deployable to
  GitHub Pages via `wasm-demo.yml`.
