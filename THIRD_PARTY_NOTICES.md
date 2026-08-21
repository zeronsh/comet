# Third-party notices

Zeron bundles the following syntax-highlighting components. Their parsers and
queries are consumed from the pinned Rust crates listed in `Cargo.lock`.

| Component | Version | License | Source |
| --- | --- | --- | --- |
| Tree-sitter | 0.26.11 | MIT | https://github.com/tree-sitter/tree-sitter |
| Tree-sitter highlight | 0.26.11 | MIT | https://github.com/tree-sitter/tree-sitter |
| Tree-sitter Rust grammar and queries | 0.24.2 | MIT | https://github.com/tree-sitter/tree-sitter-rust |
| Tree-sitter JavaScript grammar and queries | 0.25.0 | MIT | https://github.com/tree-sitter/tree-sitter-javascript |
| Tree-sitter TypeScript grammar and queries | 0.23.2 | MIT | https://github.com/tree-sitter/tree-sitter-typescript |
| Tree-sitter Python, Go, JSON, Bash, HTML, CSS, C, C++, C#, Java, Ruby and PHP grammars and queries | pinned in `Cargo.lock` | MIT | https://github.com/tree-sitter |
| Tree-sitter TOML, Markdown, YAML, Kotlin, Swift, SQL, Lua, Nix, Make and Containerfile grammars and queries | pinned in `Cargo.lock` | MIT-compatible; see each crate | Crate repositories recorded in `Cargo.lock` |

Zeron also uses the following editor foundations from the pinned `jsgrrchg/gpui-component` fork. The fork aligns these crates with the same GPUI revision used by Comet.

| Component | Version | License | Source |
| --- | --- | --- | --- |
| gpui-base | 0.5.2 (`8194877`) | Apache-2.0 | https://github.com/jsgrrchg/gpui-component |
| Ropey | 2.0.0-beta.1 | MIT | https://github.com/cessen/ropey |

The full Zeron distribution remains licensed under the terms in `LICENSE`.
