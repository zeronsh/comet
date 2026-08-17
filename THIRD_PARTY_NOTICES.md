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

The full Zeron distribution remains licensed under the terms in `LICENSE`.
