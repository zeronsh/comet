//! Synthetic ACP peer. Exercises real native stdio/spawn without an agent account.
use serde_json::{Value, json};
use std::io::{BufRead, Write};

fn emit(message: Value) {
    let mut out = std::io::stdout().lock();
    serde_json::to_writer(&mut out, &message).unwrap();
    writeln!(out).unwrap();
    out.flush().unwrap();
}

fn update(session: &str, text: String) {
    emit(json!({"jsonrpc":"2.0","method":"session/update","params":{
        "sessionId":session,"update":{"sessionUpdate":"agent_message_chunk",
        "content":{"type":"text","text":text}}
    }}));
}

fn main() {
    // A broken test must not leave even a direct fixture process behind.
    std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_secs(20));
        std::process::exit(99);
    });
    let mut session = "native-session".to_string();
    let mut pending = None;
    for line in std::io::stdin().lock().lines() {
        let message: Value = serde_json::from_str(&line.unwrap()).unwrap();
        let id = message.get("id").cloned().unwrap_or(Value::Null);
        match message["method"].as_str().unwrap_or_default() {
            "initialize" => emit(json!({"jsonrpc":"2.0","id":id,"result":{
                "protocolVersion":1,"agentCapabilities":{"loadSession":true}
            }})),
            "session/new" => emit(json!({"jsonrpc":"2.0","id":id,"result":{"sessionId":session}})),
            "session/load" => {
                session = message["params"]["sessionId"].as_str().unwrap().to_string();
                emit(json!({"jsonrpc":"2.0","id":id,"result":{}}));
            }
            "session/prompt" => {
                let prompt = message["params"]["prompt"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter_map(|part| part["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("");
                update(
                    &session,
                    json!({
                        "pid":std::process::id(), "prompt":prompt,
                        "cwd":std::env::current_dir().unwrap(),
                        "argv":std::env::args().skip(1).collect::<Vec<_>>()
                    })
                    .to_string(),
                );
                if prompt == "wait-for-cancel" {
                    pending = Some(id);
                } else {
                    emit(json!({"jsonrpc":"2.0","id":id,"result":{"stopReason":"end_turn"}}));
                    break;
                }
            }
            "session/cancel" => {
                emit(
                    json!({"jsonrpc":"2.0","id":pending.take().expect("active prompt"),
                    "result":{"stopReason":"cancelled"}}),
                );
                break;
            }
            "session/set_model" | "session/set_config_option" => {
                emit(json!({"jsonrpc":"2.0","id":id,"result":{}}))
            }
            other => panic!("unexpected method: {other}"),
        }
    }
}
