//! Postmortem inspector: print a chat doc's command entries + processed-ledger
//! state straight from a data dir's docs store (no engine, no lock contention
//! beyond sqlite's own).
//!
//!   cargo run -p zeron-engine --example doc_inspect -- <store-root> <chat-id>

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let root = args.next().expect("store root");
    let chat_id = args.next().expect("chat id");
    let store = zeron_sync::DocsStore::open(std::path::PathBuf::from(root))?;
    let Some(bytes) = store.load_snapshot(&chat_id)? else {
        println!("NO SNAPSHOT for {chat_id}");
        return Ok(());
    };
    let raw = loro::LoroDoc::new();
    raw.import(&bytes)?;
    let doc = zeron_doc::SessionDoc::from_doc(raw);
    let entries = doc.read_entries()?;
    println!("== messages: {} ==", entries.len());
    for e in &entries {
        println!("  {} {:?} {:?}", e.id, e.role, e.status);
    }
    let commands = doc.read_commands()?;
    println!("== commands: {} ==", commands.len());
    for c in &commands {
        let processed = store.is_processed(&c.id).unwrap_or(false);
        println!(
            "  id={} kind={:?} status={:?} processed={} issued_at={} expires_at={:?} resolution={:?}",
            c.id,
            c.kind(),
            c.status,
            processed,
            c.issued_at,
            c.expires_at,
            c.resolution,
        );
    }
    Ok(())
}
