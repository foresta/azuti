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
| d | Connect to Snowflake (key-pair / JWT first, SSO later) | ✅ |

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
| `←` / `→`, `h` / `l` | Scroll columns left / right |
| `PageUp` / `PageDown` | Move by a page |
| `g` / `G` | Jump to top / bottom |
| `Enter` | Open the full-value detail view for the selected row (`Esc` to close) |
| `q`, `Esc`, `Ctrl-C` | Quit |

Supported input formats: **CSV** (header row inferred) and **Parquet**. Errors
(missing file, bad format) are shown in the results area instead of crashing.

Cells wider than their column are clipped with a `…`, and column widths are
measured in terminal cells so full-width (e.g. CJK) text aligns correctly. Press
`Enter` to open a detail view with the selected row's full, untruncated values.
Very wide tables scroll horizontally with `←` / `→` (or `h` / `l`); `‹` / `›` in
the title mark columns hidden off the left / right edge.

## Snowflake

Run a query against Snowflake with key-pair (JWT) auth and browse the result.

Using `~/.snowflake/connections.toml` (the same file the `snow` CLI uses):

```sh
# Uses the [default] connection
cargo run -- --sql "SELECT * FROM my_db.my_schema.my_table LIMIT 10000"

# Or a named connection
cargo run -- --sql "SELECT ..." --connection my_conn
```

Any explicit flag overrides the value from connections.toml. To skip the file
entirely and pass everything on the command line:

```sh
cargo run -- \
  --sql "SELECT * FROM my_db.my_schema.my_table LIMIT 10000" \
  --account ab12345.ap-northeast-1.aws \
  --user ME \
  --private-key ~/.snowflake/rsa_key.p8 \
  --warehouse WH --database MY_DB --schema MY_SCHEMA --role MY_READONLY_ROLE
```

The connections file is found via `$SNOWFLAKE_HOME` or `~/.snowflake`, or set it
with `--connections-file`.

The query runs once at startup; connection or SQL errors are printed to the
terminal (not inside the TUI). Results come back as Arrow and flow into the same
viewport-rendered table as local files.

Notes:

- Prefer a **read-only role** for viewing — Snowflake has no app-enforced
  read-only mode, so access is controlled by the role you connect with.
- The private key must be an **unencrypted PEM** (PKCS#8) for now.
- Connectivity uses [`snowflake-api`](https://crates.io/crates/snowflake-api),
  which talks to Snowflake's HTTPS API; `arrow` is pinned to the version it uses.

## Stack

- [ratatui](https://ratatui.rs/) — TUI framework
- [arrow (arrow-rs)](https://github.com/apache/arrow-rs) — columnar data storage
  and cell formatting (from step c)
- [snowflake-api](https://crates.io/crates/snowflake-api) — Snowflake
  connectivity (Arrow-native, pure Rust over HTTPS; step d)

## License

MIT
