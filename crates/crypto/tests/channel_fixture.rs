//! Reproducible Snow transcript consumed by the Swift channel conformance tests.
use serde_json::json;

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[test]
fn swift_channel_fixture() {
    let prologue = [b"zeron/device-channel/v1\0".as_slice(), &[1; 16], &[2; 16]].concat();
    let build = || snow::Builder::new("Noise_XX_25519_AESGCM_SHA256".parse().unwrap());
    let mut initiator = build()
        .local_private_key(&[3; 32])
        .unwrap()
        .fixed_ephemeral_key_for_testing_only(&[5; 32])
        .prologue(&prologue)
        .unwrap()
        .build_initiator()
        .unwrap();
    let mut responder = build()
        .local_private_key(&[4; 32])
        .unwrap()
        .fixed_ephemeral_key_for_testing_only(&[6; 32])
        .prologue(&prologue)
        .unwrap()
        .build_responder()
        .unwrap();
    let mut buffer = vec![0; 65535];
    let mut plaintext = vec![0; 65535];
    let n = initiator.write_message(&[], &mut buffer).unwrap();
    let first = buffer[..n].to_vec();
    responder.read_message(&first, &mut plaintext).unwrap();
    let n = responder.write_message(&[8; 16], &mut buffer).unwrap();
    let second = buffer[..n].to_vec();
    initiator.read_message(&second, &mut plaintext).unwrap();
    let peer_key = initiator.get_remote_static().unwrap().to_vec();
    let n = initiator.write_message(&[7; 16], &mut buffer).unwrap();
    let third = buffer[..n].to_vec();
    responder.read_message(&third, &mut plaintext).unwrap();
    let mut initiator = initiator.into_transport_mode().unwrap();
    let mut responder = responder.into_transport_mode().unwrap();
    let messages = [
        Vec::new(),
        b"hello from the phone".to_vec(),
        (0..200_000).map(|i| (i % 251) as u8).collect(),
    ];
    let mut seal = |message: &[u8], from_phone: bool| {
        let (writer, reader) = if from_phone {
            (&mut initiator, &mut responder)
        } else {
            (&mut responder, &mut initiator)
        };
        let chunks: Vec<_> = if message.is_empty() {
            vec![&[][..]]
        } else {
            message.chunks(65518).collect()
        };
        let mut sealed = Vec::new();
        for (i, chunk) in chunks.iter().enumerate() {
            let body = [&[u8::from(i + 1 < chunks.len())][..], chunk].concat();
            let n = writer.write_message(&body, &mut buffer).unwrap();
            sealed.extend_from_slice(&(n as u16).to_be_bytes());
            sealed.extend_from_slice(&buffer[..n]);
            let count = reader.read_message(&buffer[..n], &mut plaintext).unwrap();
            assert_eq!(&plaintext[..count], &body);
        }
        hex(&sealed)
    };
    let frames: Vec<_> = messages
        .iter()
        .map(
            |m| json!({"plaintext": hex(m), "outgoing": seal(m, true), "incoming": seal(m, false)}),
        )
        .collect();
    let fixture = json!({"first": hex(&first), "second": hex(&second), "third": hex(&third), "peerKey": hex(&peer_key), "frames": frames});
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/channel.json");
    if std::env::var_os("UPDATE_CHANNEL_FIXTURE").is_some() {
        std::fs::write(&path, serde_json::to_string(&fixture).unwrap() + "\n").unwrap();
    }
    let existing: serde_json::Value =
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    assert_eq!(existing, fixture);
}
