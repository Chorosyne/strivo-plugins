<div align="center">

# strivo-plugins

### Historical first-party plugins for StriVo

[![Status: archived](https://img.shields.io/badge/status-archived%20%2F%20read--only-6b7280)](https://github.com/revoydotdev/strivo-plugins)
[![License: MIT](https://img.shields.io/badge/license-MIT-2563eb.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/Rust-2021-ed7b17?logo=rust&logoColor=white)](https://www.rust-lang.org/)

**This repository is preserved for history. Current StriVo development lives in the [main StriVo workspace](https://github.com/revoydotdev/strivo).**

</div>

> [!WARNING]
> **Archived and read-only.** This repository contains the former standalone
> implementation of StriVo's `Crunchr` and `Archiver` plugins. It is not
> maintained, does not accept feature work, and is not a supported dependency
> for current StriVo releases. The plugins were moved into the active StriVo
> workspace at [`crates/strivo-plugins/`](https://github.com/revoydotdev/strivo/tree/main/crates/strivo-plugins).

## What this archive preserves

This crate is a snapshot of two early, first-party Rust plugins that integrated
with StriVo's Ratatui-based host and stored their local state in SQLite.

| Plugin | Historical role | Notable preserved behavior |
| --- | --- | --- |
| `crunchr` | Recording transcription and analysis | Whisper CLI, a self-hosted Voxtral-compatible endpoint, or the Mistral audio API; transcript search; optional OpenRouter summary, topic, and sentiment analysis |
| `archiver` | Back-catalog acquisition | Twitch and YouTube archive scanning and `yt-dlp` downloads, with SQLite-backed progress and archive tracking |

The source remains useful for understanding the original plugin shape and its
evolution. It is not documentation for the current Creator Edition, a
packaging guide, or a compatibility layer.

## Where current work belongs

Use [**revoydotdev/strivo**](https://github.com/revoydotdev/strivo) for a
working installation, current documentation, bugs, security reports, and
contributions. The active repository builds the first-party plugins in-tree,
so host and plugin changes share one workspace and one dependency graph.

```bash
git clone https://github.com/revoydotdev/strivo.git
cd strivo
cargo build -p strivo-plugins
```

For the Creator Edition binary, follow the active host's
[README](https://github.com/revoydotdev/strivo#the-two-editions) and
[contribution guide](https://github.com/revoydotdev/strivo/blob/main/CONTRIBUTING.md).
Those documents, along with the current `Cargo.toml`, are authoritative.

## Compatibility boundary

This archive targets an earlier standalone host contract. It makes **no** API,
ABI, configuration, database-schema, or toolchain compatibility promise with
current StriVo.

- Do not add this crate as a dependency of a current StriVo checkout, and do
  not copy its `strivo-core` dependency setup into new projects.
- Do not load a binary built from this archive into a current StriVo process.
  Rust trait-object plugin loading requires the exact host build, dependency
  closure, feature set, and `rustc` toolchain; mismatches can crash the host or
  corrupt memory.
- Do not treat the historical backends, configuration names, environment
  variables, paths, or keybindings here as current product documentation.

For the current third-party-plugin constraints, see StriVo's
[plugin manifest documentation](https://github.com/revoydotdev/strivo/blob/main/docs/PLUGIN-MANIFEST.md).

## Inspecting the archive

The repository can still be cloned for source review or a historically pinned
reproduction without presenting this snapshot as a supported build target:

```bash
git clone https://github.com/revoydotdev/strivo-plugins.git
cd strivo-plugins
git log -1 --oneline
```

Building this snapshot requires the matching historical StriVo source,
dependency versions, and Rust toolchain. If your goal is to run or extend the
plugins, use the active workspace instead of attempting to bridge this archive
to a modern release.

## Contributing and security

This archive is read-only: please do not open pull requests or feature issues
here. Report current behavior in the
[StriVo issue tracker](https://github.com/revoydotdev/strivo/issues), and use
its [private security-advisory channel](https://github.com/revoydotdev/strivo/security/advisories/new)
for vulnerabilities. Third-party tool issues belong with their respective
maintainers.

## Provenance and license

The code and documentation preserved in this repository remain available under
the [MIT License](LICENSE). Archiving the repository changes its maintenance
status, not the license or provenance of this historical snapshot.
