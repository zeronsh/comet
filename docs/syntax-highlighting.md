# Syntax highlighting

Desktop syntax highlighting lives in the pure `zeron-syntax` crate. It detects
languages, runs pinned Tree-sitter grammars and queries, and returns sorted,
non-overlapping UTF-8 byte spans relative to each source line. The UI resolves
those neutral `HighlightKind` values through `Theme::syntax`; parser code never
depends on GPUI or colors.

Markdown fences and tool diffs parse complete documents on GPUI's background
executor. Changes first parses separate old/new hunk excerpts, then lazily asks
the checkout host for checksum-bound complete sources. Deleted lines use the old
document; added and context lines prefer the new document. A stale checksum or
any visible-line mismatch discards the full result atomically.

## Adding a grammar

1. Review the parser, generated sources, and queries' licenses. Pin an exact
   crate version in `crates/syntax/Cargo.toml` and add it to
   `THIRD_PARTY_NOTICES.md`.
2. Add aliases, extensions, exact filenames, and any unambiguous shebang to the
   central registry in `crates/syntax/src/lib.rs`.
3. Add its `HighlightConfiguration` using official compatible queries. Map new
   capture vocabulary to an existing `HighlightKind`; never expose capture names
   to the theme.
4. Add a minimal distinctive fixture to the query-load table. If the language
   supports injections, register only known child parsers and keep unknown
   injected languages plain.
5. Run `cargo test -p zeron-syntax`, UI Markdown/Changes tests, the ignored
   diagnostic benchmark when parser cost changes, and the workspace checks.

Do not add language-specific parsing to a renderer. Unknown languages, binaries,
oversized sources, incompatible queries, and parse failures must remain plain.
Highlighting changes foreground color only—never font, weight, style, wrapping,
height, or scroll geometry.
