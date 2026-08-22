//! Live probe: real cursor model discovery through the harness path.
use zeron_harness::{CursorHarness, Harness};

#[tokio::main]
async fn main() {
    let models = CursorHarness::new().models().await.expect("models");
    println!("count={}", models.len());
    for m in models.iter().take(6) {
        let opts: Vec<String> = m
            .options
            .iter()
            .map(|o| format!("{}[{}→{}]", o.id, o.choices.len(), o.default_choice))
            .collect();
        println!("{} | {} | {}", m.id, m.label, opts.join(" "));
    }
}
