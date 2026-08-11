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
- Interactive SQL REPL (`archon-sql` binary) over `tpt-archon-relational`'s
  `Database`: the same parser/planner/executor the engine's Rust tests use, with
  a terminal-friendly result renderer.
- Interactive mode (`;` terminates a statement) and a single-statement `-e`
  flag plus `--help`/`--version`.
- Dot-commands: `.mode table|csv|json` to switch output rendering, `.help`,
  and `.quit`.
- Optional `sqlite-import` feature — `rusqlite` (bundled) backed import of an
  existing SQLite database for experimentation.
