//! One-off recovery tool (untracked): inspect and repair chat/workspace docs
//! in a docs.sqlite3 store. Modes:
//!   inspect-workspace <data_dir> <chat_id>
//!   inspect-chat      <data_dir> <chat_id>
//!   cut-chat          <data_dir> <chat_id> <from_index>
use comet_doc::SessionDoc;
use comet_sync::DocsStore;
use loro::{LoroDoc, ToJson};

fn load_doc(store: &DocsStore, doc_id: &str) -> LoroDoc {
    let bytes = store
        .load_snapshot(doc_id)
        .expect("load snapshot")
        .unwrap_or_else(|| panic!("no snapshot for doc {doc_id}"));
    let doc = LoroDoc::new();
    doc.import(&bytes).expect("import snapshot");
    doc
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mode = args.get(1).expect("mode").as_str();
    let data_dir = args.get(2).expect("data dir");
    let store = DocsStore::open(data_dir).expect("open store");

    match mode {
        "inspect-workspace" => {
            let chat_id = args.get(3).expect("chat id");
            let doc = load_doc(&store, "workspace2");
            let value = doc.get_deep_value().to_json_value();
            let row = value
                .get("chats")
                .and_then(|c| c.get(chat_id.as_str()))
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            println!("{}", serde_json::to_string_pretty(&row).unwrap());
        }
        "inspect-chat" => {
            let chat_id = args.get(3).expect("chat id");
            let doc = load_doc(&store, chat_id);
            let value = doc.get_deep_value().to_json_value();
            let msgs = value
                .get("messages")
                .and_then(|m| m.as_array())
                .cloned()
                .unwrap_or_default();
            println!("total entries: {}", msgs.len());
            for (i, m) in msgs.iter().enumerate() {
                let role = m.get("role").and_then(|v| v.as_str()).unwrap_or("?");
                let id = m.get("id").and_then(|v| v.as_str()).unwrap_or("?");
                let status = m.get("status").and_then(|v| v.as_str()).unwrap_or("-");
                let at = m.get("createdAt").and_then(|v| v.as_i64()).unwrap_or(0);
                let mut preview = String::new();
                if let Some(parts) = m.get("parts").and_then(|p| p.as_array()) {
                    for p in parts {
                        let kind = p.get("kind").and_then(|v| v.as_str()).unwrap_or("text");
                        match kind {
                            "error" => {
                                preview.push_str(&format!(
                                    "[ERROR: {}] ",
                                    p.get("message").and_then(|v| v.as_str()).unwrap_or("")
                                ));
                            }
                            "tool" => preview.push_str("[tool] "),
                            _ => {
                                if let Some(t) = p.get("text").and_then(|v| v.as_str()) {
                                    preview.push_str(t);
                                    preview.push(' ');
                                }
                            }
                        }
                        if preview.len() > 140 {
                            break;
                        }
                    }
                }
                preview.truncate(140);
                let preview = preview.replace('\n', " ");
                println!("[{i:3}] {role:9} {status:9} {at} {id} :: {preview}");
            }
        }
        "cut-chat" => {
            let chat_id = args.get(3).expect("chat id");
            let from: usize = args.get(4).expect("from index").parse().expect("index");
            let doc = load_doc(&store, chat_id);
            let list = doc.get_list("messages");
            let len = list.len();
            assert!(from < len, "from {from} out of range (len {len})");
            list.delete(from, len - from).expect("delete entries");
            doc.commit();
            let session = SessionDoc::from_doc(doc);
            let bytes = session.export_snapshot().expect("export");
            store.save_snapshot(chat_id, &bytes).expect("save");
            println!(
                "cut chat {chat_id}: removed entries [{from}..{len}), {} remain; snapshot saved ({} bytes)",
                from,
                bytes.len()
            );
        }
        "set-preview" => {
            let chat_id = args.get(3).expect("chat id");
            let preview = args.get(4).expect("preview");
            let at: i64 = args.get(5).expect("at ms").parse().expect("ms");
            let doc = load_doc(&store, "workspace2");
            let chats = doc.get_map("chats");
            let row = match chats.get(chat_id) {
                Some(loro::ValueOrContainer::Container(loro::Container::Map(m))) => m,
                _ => panic!("no chat row {chat_id}"),
            };
            row.insert("lastMessagePreview", preview.as_str())
                .expect("preview");
            row.insert("lastMessageAt", at).expect("at");
            doc.commit();
            let bytes = doc
                .export(loro::ExportMode::Snapshot)
                .expect("export workspace");
            store.save_snapshot("workspace2", &bytes).expect("save");
            println!(
                "workspace2 preview updated for {chat_id} ({} bytes)",
                bytes.len()
            );
        }
        other => panic!("unknown mode {other}"),
    }
}
