//! E2e for attachments entering the model context: an attachment the user hangs on must really appear in **the request body sent to the model**.
//!
//! The real path (not the display layer): `v4/attachment/{begin,chunk,commit}` lands the bytes → `sendText`
//! carries an `attachmentRefSchema` reference → the host resolves it into content parts → assert the request body the provider received.
//! Pins one item for each of the three outcomes: image → image part; text-like → inlined body; PDF → the reference text the declaration emits.
//! The reference vectors are computed independently by Node `crypto` (sha256/base64), not produced by the code under test.

mod common;

use common::{command_envelope, text_sse, Client, HostProcess, ScriptedResponder};
use serde_json::{json, Value};
use std::time::Duration;

const ABC_SHA256: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
const ABC_BASE64: &str = "YWJj";
const TEXT_BODY: &str = "ATTACHED-TEXT-BODY-91c3";
const TEXT_SHA256: &str = "fab99da07b0766937a40d1c2a9555ec7ae975180d61af8cca61b9a24d74a4f97";
const TEXT_BASE64: &str = "QVRUQUNIRUQtVEVYVC1CT0RZLTkxYzM=";
const PDF_BODY: &str = "%PDF-1.4-fake-body";
const PDF_SHA256: &str = "6061cc1989f6bd933a9c26ffd9a11a326119e220e62827ae7d649e4d841fca5a";
const PDF_BASE64: &str = "JVBERi0xLjQtZmFrZS1ib2R5";

fn temp_base(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "zcode-host-attach-ctx-{name}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn write_provider_config(port: u16, path: &std::path::Path) {
    std::fs::write(
        path,
        json!({
            "kind": "anthropic",
            "baseUrl": format!("http://127.0.0.1:{port}"),
            "apiKey": "test-key-not-a-real-credential",
            "model": "test-model",
            "maxTokens": 1024,
        })
        .to_string(),
    )
    .expect("provider config");
}

/// The full description of one upload (helpers take a struct argument, avoiding call sites with eight positional parameters).
struct UploadSpec<'a> {
    upload_id: &'a str,
    file_name: &'a str,
    mime: &'a str,
    data_base64: &'a str,
    bytes: u64,
    sha256: &'a str,
}

/// Goes through the real upload transaction to obtain the ref (not a shortcut of stuffing fields into the store).
fn upload(client: &mut Client, session_id: &str, spec: &UploadSpec<'_>) -> String {
    let id = client.send(
        "v4/attachment/begin",
        json!({
            "connectionId": "conn-1",
            "sessionId": session_id,
            "uploadId": spec.upload_id,
            "fileName": spec.file_name,
            "mime": spec.mime,
            "totalBytes": spec.bytes,
            "totalChunks": 1,
            "checksum": format!("sha256:{}", spec.sha256),
        }),
    );
    let begun = client.response(id);
    assert_eq!(
        begun["result"]["state"].as_str(),
        Some("staging"),
        "begin must stage the upload: {begun}"
    );

    let id = client.send(
        "v4/attachment/chunk",
        json!({
            "connectionId": "conn-1",
            "sessionId": session_id,
            "uploadId": spec.upload_id,
            "chunkIndex": 0,
            "dataBase64": spec.data_base64,
        }),
    );
    let chunked = client.response(id);
    assert_eq!(
        chunked["result"]["nextChunkIndex"].as_u64(),
        Some(1),
        "the chunk must be accepted: {chunked}"
    );

    let id = client.send(
        "v4/attachment/commit",
        json!({"connectionId": "conn-1", "sessionId": session_id, "uploadId": spec.upload_id}),
    );
    let committed = client.response(id);
    committed["result"]["ref"]
        .as_str()
        .unwrap_or_else(|| panic!("commit must return a ref: {committed}"))
        .to_string()
}

fn send_with_attachment(
    client: &mut Client,
    session_id: &str,
    command_id: &str,
    text: &str,
    attachment: Value,
) {
    let id = client.send(
        "v4/command",
        command_envelope(
            "sendText",
            command_id,
            Some(session_id),
            json!({"text": text, "attachments": [attachment]}),
        ),
    );
    let response = client.response(id);
    assert_eq!(
        response["result"]["status"],
        json!("accepted"),
        "{command_id} must be admitted: {response}"
    );
}

fn wait_turn_settled(client: &mut Client, session_id: &str, command_id: &str) {
    for _ in 0..200 {
        let id = client.send(
            "v4/commands/query",
            json!({"commands": [{"sessionId": session_id, "commandId": command_id}]}),
        );
        let response = client.response(id);
        let entry = response["result"]["results"]
            .as_array()
            .and_then(|items| items.first())
            .cloned()
            .unwrap_or(Value::Null);
        if entry["result"] != json!("unknown") && entry["result"] != Value::Null {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("command {command_id} never settled");
}

#[test]
fn attached_images_text_and_pdfs_reach_the_provider_request() {
    // Three rounds: one attachment per round, the model only has to reply with one line of text; all assertions are on the request body.
    let (provider, port) = ScriptedResponder::spawn(vec![
        text_sse("saw the image"),
        text_sse("read the text file"),
        text_sse("noted the pdf"),
    ]);
    let base_dir = temp_base("context");
    let workspace = base_dir.join("workspace");
    let data_dir = base_dir.join("data");
    std::fs::create_dir_all(&workspace).expect("workspace");
    std::fs::create_dir_all(&data_dir).expect("data dir");
    let config_path = base_dir.join("provider.json");
    write_provider_config(port, &config_path);

    let mut process = HostProcess::spawn(&workspace, &data_dir, &config_path, false);
    let mut client = Client {
        id_counter: 0,
        stdin: process.stdin(),
        stdout: process.stdout(),
        pending: std::collections::VecDeque::new(),
    };

    let id = client.send(
        "v4/command",
        command_envelope("createSession", "cmd-create", None, json!({})),
    );
    let session_id = client.response(id)["result"]["result"]["sessionId"]
        .as_str()
        .expect("session id")
        .to_string();
    let id = client.send(
        "v4/conversation/subscribe",
        json!({
            "sessionId": session_id,
            "connectionId": "conn-1",
            "clientMode": "desktop-continuous"
        }),
    );
    client.response(id);

    let image_ref = upload(
        &mut client,
        &session_id,
        &UploadSpec {
            upload_id: "up-1",
            file_name: "a.png",
            mime: "image/png",
            data_base64: ABC_BASE64,
            bytes: 3,
            sha256: ABC_SHA256,
        },
    );
    let text_ref = upload(
        &mut client,
        &session_id,
        &UploadSpec {
            upload_id: "up-2",
            file_name: "notes.txt",
            mime: "text/plain",
            data_base64: TEXT_BASE64,
            bytes: 23,
            sha256: TEXT_SHA256,
        },
    );
    let pdf_ref = upload(
        &mut client,
        &session_id,
        &UploadSpec {
            upload_id: "up-3",
            file_name: "spec.pdf",
            mime: "application/pdf",
            data_base64: PDF_BASE64,
            bytes: 18,
            sha256: PDF_SHA256,
        },
    );
    assert!(image_ref.starts_with("zcode-artifact://"), "{image_ref}");

    send_with_attachment(
        &mut client,
        &session_id,
        "cmd-img",
        "describe this picture",
        json!({"ref": image_ref, "fileName": "a.png", "mime": "image/png", "bytes": 3}),
    );
    wait_turn_settled(&mut client, &session_id, "cmd-img");

    send_with_attachment(
        &mut client,
        &session_id,
        "cmd-txt",
        "summarize the attached file",
        json!({"ref": text_ref, "fileName": "notes.txt", "mime": "text/plain", "bytes": 23}),
    );
    wait_turn_settled(&mut client, &session_id, "cmd-txt");

    send_with_attachment(
        &mut client,
        &session_id,
        "cmd-pdf",
        "what does this spec say",
        json!({"ref": pdf_ref, "fileName": "spec.pdf", "mime": "application/pdf", "bytes": 18}),
    );
    wait_turn_settled(&mut client, &session_id, "cmd-pdf");

    let bodies = provider.request_bodies();
    assert_eq!(bodies.len(), 3, "one request per turn: {bodies:?}");

    // 1) image: an anthropic-shaped base64 image block whose data is the independently computed vector.
    assert!(
        bodies[0].contains("\"type\":\"image\"")
            && bodies[0].contains(&format!("\"data\":\"{ABC_BASE64}\"")),
        "the image bytes must be in the first request body:\n{}",
        bodies[0]
    );
    assert!(
        bodies[0].contains("\"media_type\":\"image/png\""),
        "declared mime must survive to the provider:\n{}",
        bodies[0]
    );
    assert!(
        bodies[0].contains("describe this picture"),
        "the user's own text stays in front of the attachment:\n{}",
        bodies[0]
    );

    // 2) text-like: the body is inlined directly (not a path reference).
    assert!(
        bodies[1].contains(TEXT_BODY),
        "a text/plain attachment must be inlined:\n{}",
        bodies[1]
    );

    // 3) PDF: what the model sees is the reference text the declaration emits, rather than it being dropped or decoded as text.
    assert!(
        bodies[2].contains("Attached application/pdf: spec.pdf"),
        "unsupported media must say so in place:\n{}",
        bodies[2]
    );
    assert!(
        !bodies[2].contains(PDF_BODY),
        "PDF bytes must never be smuggled in as text content:\n{}",
        bodies[2]
    );
}
