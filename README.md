# azuti

A fast terminal (TUI) data viewer.

The name comes from kyudo (Japanese archery): the **azuchi** is the target mound
where arrows fly, land, and line up in view. Since the arrows here are Apache
Arrow, the metaphor doubles as the engine. Spelled `azuti` in Kunrei-shiki
romanization.

## Design goals

- Keep large result sets as **columnar Apache Arrow buffers** instead of turning
  every cell into a language object, and render only the rows currently on
  screen (**viewport rendering**). This is what keeps it fast on millions of rows.
- Snowflake first, but because the internal engine is Arrow, the name and
  structure are chosen to grow into a multi-database tool later.

## Roadmap

| Step | Scope | Status |
|---|---|---|
| a | CLI scaffold + TUI skeleton | ✅ |
| b | Show local data (CSV / Parquet) | ✅ |
| c | Fast table over synthetic data (Arrow + viewport rendering) | ✅ |
| d | Connect to Snowflake (key-pair / JWT first, SSO later) | — |

## Usage (work in progress)

```sh
# First time only: install the Rust toolchain
# curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# Open a local file (CSV or Parquet)
cargo run -- testdata/sample.csv

# Stress-test with N synthetic rows (no file needed)
cargo run -- --demo 2000000

# Or start empty
cargo run
```

Rendering is viewport-based: only the rows currently on screen are formatted
from the Arrow columns, so scrolling cost does not grow with the row count. A
2,000,000-row demo table paints a frame in well under a millisecond, including
after jumping to the last row.

On start you get a three-row layout (header / results table / key hints).

| Key | Action |
|---|---|
| `↑` / `↓`, `j` / `k` | Move selection |
| `PageUp` / `PageDown` | Move by a page |
| `g` / `G` | Jump to top / bottom |
| `Enter` | Open the full-value detail view for the selected row (`Esc` to close) |
| `q`, `Esc`, `Ctrl-C` | Quit |

Supported input formats: **CSV** (header row inferred) and **Parquet**. Errors
(missing file, bad format) are shown in the results area instead of crashing.

Cells wider than their column are clipped with a `…`, and column widths are
measured in terminal cells so full-width (e.g. CJK) text aligns correctly. Press
`Enter` to open a detail view with the selected row's full, untruncated values.
Very wide tables do not scroll horizontally yet (tracked as a follow-up).

## Stack

- [ratatui](https://ratatui.rs/) — TUI framework
- [arrow (arrow-rs)](https://github.com/apache/arrow-rs) — columnar data storage
  and cell formatting (from step c)
- [snowflake-api](https://crates.io/crates/snowflake-api) — Snowflake
  connectivity (Arrow-native, pure Rust over HTTPS; step d)

## License

MIT
