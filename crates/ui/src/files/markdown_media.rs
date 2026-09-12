//! Collect workspace images and diagrams referenced by Markdown.
use crate::markdown::parser::{Block, BlockTree, InlineRun};

pub(super) fn image_sources(tree: &BlockTree) -> Vec<String> {
    fn runs(runs: &[InlineRun], out: &mut Vec<String>) {
        for run in runs {
            if let Some(image) = &run.style.image {
                if !out.contains(&image.source) {
                    out.push(image.source.clone());
                }
            }
        }
    }
    fn block(b: &Block, out: &mut Vec<String>) {
        match b {
            Block::Paragraph { runs: r } | Block::Heading { runs: r, .. } => runs(r, out),
            Block::BlockQuote { children } => children.iter().for_each(|b| block(b, out)),
            Block::List { items, .. } => items.iter().flatten().for_each(|b| block(b, out)),
            Block::Table { header, rows, .. } => header
                .iter()
                .chain(rows.iter().flatten())
                .for_each(|r| runs(r, out)),
            _ => {}
        }
    }
    let mut out = Vec::new();
    for top in &tree.blocks {
        block(&top.block, &mut out);
    }
    out
}

pub(super) fn diagram_sources(tree: &BlockTree) -> Vec<String> {
    fn visit(block: &Block, out: &mut Vec<String>) {
        match block {
            Block::CodeBlock { language, code }
                if language
                    .as_deref()
                    .is_some_and(|l| l.eq_ignore_ascii_case("mermaid")) =>
            {
                if !out.contains(code) {
                    out.push(code.clone());
                }
            }
            Block::BlockQuote { children } => children.iter().for_each(|b| visit(b, out)),
            Block::List { items, .. } => items.iter().flatten().for_each(|b| visit(b, out)),
            _ => {}
        }
    }
    let mut out = Vec::new();
    for top in &tree.blocks {
        visit(&top.block, &mut out);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn image_collection_preserves_nested_and_repeated_media() {
        let tree = crate::markdown::parser::parse_full(
            "before ![a](a.png) after\n\n> ![b](b.svg)\n\n- ![a](a.png)",
        );
        assert_eq!(image_sources(&tree), ["a.png", "b.svg"]);
    }
}
