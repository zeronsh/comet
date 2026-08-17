use std::time::Instant;

use zeron_syntax::{HighlightKind, HighlightRequest, highlight};

fn snapshot(source: &str, path: &str) -> Vec<String> {
    let document = highlight(HighlightRequest {
        source,
        path: Some(path),
        fence_tag: None,
    })
    .unwrap();
    source
        .lines()
        .zip(document.lines)
        .flat_map(|(line, spans)| {
            spans
                .into_iter()
                .map(move |span| format!("{:?}:{}", span.kind, &line[span.range]))
        })
        .collect()
}

#[test]
fn rust_reference_span_snapshot() {
    let source = r#"use std::path::Path;
// quiet comment
struct Widget { field: usize }
fn build(value: usize) -> Widget {
    let label = format!("item-{value}");
    Widget { field: 42 }
}"#;
    let spans = snapshot(source, "src/lib.rs");
    for expected in [
        "Keyword:use",
        "Constructor:Path",
        "Comment:// quiet comment",
        "Type:Widget",
        "Property:field",
        "TypeBuiltin:usize",
        "Function:build",
        "Parameter:value",
        "Macro:format!",
        "String:\"item-{value}\"",
        "Number:42",
    ] {
        assert!(
            spans.iter().any(|span| span == expected),
            "missing {expected}: {spans:#?}"
        );
    }
}

#[test]
fn every_span_is_valid_and_non_overlapping_for_incomplete_unicode() {
    let source = "fn café(value: &str) {\n let text = r#\"héllo\nworld\"#;\n if value {";
    let document = highlight(HighlightRequest {
        source,
        path: Some("broken.rs"),
        fence_tag: None,
    })
    .unwrap();
    for (line, spans) in source.lines().zip(document.lines) {
        assert!(
            spans
                .windows(2)
                .all(|pair| pair[0].range.end <= pair[1].range.start)
        );
        for span in spans {
            assert!(line.is_char_boundary(span.range.start));
            assert!(line.is_char_boundary(span.range.end));
        }
    }
}

#[test]
#[ignore = "diagnostic benchmark; run explicitly when changing parsers or queries"]
fn benchmark_small_medium_large_and_incomplete_documents() {
    for (name, source) in [
        ("small", "fn main() { println!(\"hi\"); }".to_string()),
        ("medium", "fn item() -> usize { 42 }\n".repeat(2_000)),
        ("large", "struct Item { value: usize }\n".repeat(20_000)),
        (
            "incomplete",
            "fn broken( { let value = \"open".repeat(2_000),
        ),
    ] {
        let started = Instant::now();
        let document = highlight(HighlightRequest {
            source: &source,
            path: Some("bench.rs"),
            fence_tag: None,
        })
        .unwrap();
        eprintln!(
            "{name}: bytes={} spans={} elapsed_us={}",
            source.len(),
            document.lines.iter().map(Vec::len).sum::<usize>(),
            started.elapsed().as_micros()
        );
    }
}

#[test]
fn reference_contains_expected_visual_roles() {
    let spans = snapshot("fn call() { let value = 42; }", "main.rs");
    for kind in [
        HighlightKind::Keyword,
        HighlightKind::Function,
        HighlightKind::Number,
    ] {
        assert!(
            spans
                .iter()
                .any(|span| span.starts_with(&format!("{kind:?}:")))
        );
    }
}
