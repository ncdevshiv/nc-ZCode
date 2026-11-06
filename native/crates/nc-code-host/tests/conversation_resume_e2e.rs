//! `base.seq` resumption e2e: a client reconnecting after a drop must get back **the deltas it missed**, rather than being
//! hit with the whole snapshot every time (specs/rust-native-crates.md S3 "conversation resume").
//!
//! The pinned behaviours (each line matching a criterion in TS's `conversation-topic-publisher.ts`):
//! 1. same epoch and the watermark lands inside the retention window → ack `mode:"resume"` + a `kind:"deltas"` frame,
//!    `fromSeq == base.seq`, with content covering every row produced during the offline period;
//! 2. the golden invariant: the base snapshot's row set + the resumed deltas == the full snapshot row set of a re-subscription at this moment
//!    (rowId and kind identical row by row) — resumption is not a "second-class recovery";
//! 3. epoch mismatch / watermark ahead / `forceSnapshot` → answer honestly with `mode:"snapshot"`;
//! 4. the watermark is already aligned (`base.seq == current watermark`) → `mode:"resume"` and **no frame is sent** (same as TS):
//!    pinned by "no extra protocol lines before the next request's response", not by sleeping on a timeout;
//! 5. a first subscription with no `base` behaves exactly as before (snapshot).
//!
//! The scenario is built in the shape of a mobile client: get the snapshot → drop out (unsubscribe) → run two turns **while no subscriber exists** →
//! reconnect with base. The host keeps projecting and keeping accounts even with no subscribers (having nowhere to send frames is no reason to refuse the ack),
//! and that is the only way to recover the content missed during the offline period.

mod common;

use common::{
    apply_to_rows, command_envelope, deltas_from_frame, Client, HostProcess, ScriptedProvider,
};
use serde_json::{json, Value};
use std::time::Duration;

fn temp_base(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "zcode-host-resume-e2e-{name}-{}",
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

fn subscription_id(session_id: &str, connection_id: &str) -> String {
    format!("{connection_id}:conversation/{session_id}")
}

/// Whether `base: { logEpoch, seq }` is carried is up to the caller (a first subscription has no base).
fn subscribe(
    client: &mut Client,
    session_id: &str,
    connection_id: &str,
    base: Option<(&str, u64)>,
) -> Value {
    let mut params = json!({
        "sessionId": session_id,
        "connectionId": connection_id,
        "clientMode": "desktop-continuous",
    });
    if let Some((epoch, seq)) = base {
        params["base"] = json!({"logEpoch": epoch, "seq": seq});
    }
    let id = client.send("v4/conversation/subscribe", params);
    let response = client.response(id);
    assert!(
        response["result"]["ack"]["mode"].is_string(),
        "subscribe must ack with its delivery mode: {response}"
    );
    response
}

fn resync(
    client: &mut Client,
    session_id: &str,
    connection_id: &str,
    base: Option<(&str, u64)>,
    force_snapshot: bool,
) -> Value {
    let mut params = json!({ "sessionId": session_id, "connectionId": connection_id });
    if let Some((epoch, seq)) = base {
        params["base"] = json!({"logEpoch": epoch, "seq": seq});
    }
    if force_snapshot {
        params["forceSnapshot"] = json!(true);
    }
    let id = client.send("v4/conversation/resync", params);
    client.response(id)
}

/// Read the next frame belonging to a given subscriber; lines belonging to others go back into pending in original order (never swallow a frame).
/// The protocol gives every request a response, and frames are ordered after the response, so "the target frame is long in coming" is a defect:
/// fail outright after 200 bounded reads instead of hanging the suite.
fn await_frame(client: &mut Client, sid: &str, payload_kind: &str) -> Value {
    let mut skipped = std::collections::VecDeque::new();
    let found = loop {
        let line = client.next_line();
        if line["params"]["frame"]["subscriptionId"].as_str() == Some(sid)
            && line["params"]["frame"]["payload"]["kind"].as_str() == Some(payload_kind)
        {
            break line;
        }
        skipped.push_back(line);
        if skipped.len() > 200 {
            panic!("no {payload_kind} frame for {sid} within 200 lines");
        }
    };
    for line in skipped.into_iter().rev() {
        client.pending.push_front(line);
    }
    found
}

/// Whether the subscriber still receives a frame after getting the ack: the session is quiet right now (no other subscriber running a turn),
/// so "an extra protocol line appears before the next request's response" is equivalent to "a frame was sent".
fn ack_is_followed_by_no_frame(client: &mut Client, session_id: &str, connection_id: &str) {
    let probe_id = client.send(
        "v4/commands/query",
        json!({"commands": [{"sessionId": session_id, "commandId": "cmd-probe"}]}),
    );
    let probe = client.response(probe_id);
    assert!(
        probe["result"]["results"].is_array(),
        "probe query must answer: {probe}"
    );
    let stray: Vec<Value> = client
        .pending
        .iter()
        .filter(|line| {
            line["params"]["frame"]["subscriptionId"].as_str()
                == Some(subscription_id(session_id, connection_id).as_str())
        })
        .cloned()
        .collect();
    client.pending.clear();
    assert!(
        stray.is_empty(),
        "an already-aligned client must get no frame at all: {stray:?}"
    );
}

fn wait_turn_settled(client: &mut Client, session_id: &str, command_id: &str) {
    for _ in 0..100 {
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
    panic!("command {command_id} never settled into the command ledger");
}

fn submit_turn(client: &mut Client, session_id: &str, command_id: &str, text: &str) {
    let id = client.send(
        "v4/command",
        command_envelope(
            "sendText",
            command_id,
            Some(session_id),
            json!({"text": text}),
        ),
    );
    let response = client.response(id);
    assert_eq!(
        response["result"]["status"],
        json!("accepted"),
        "{command_id} must be admitted even with no subscriber: {response}"
    );
    wait_turn_settled(client, session_id, command_id);
}

fn payload_of(frame: &Value) -> &Value {
    &frame["params"]["frame"]["payload"]
}

fn rows_of_snapshot(snapshot: &Value) -> Vec<Value> {
    snapshot["rows"]["window"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

/// The comparable identity of a row set: rowId + kind (streaming body text is carried incrementally by `row.delta`; byte-for-byte
/// comparison across paths is the established assertion surface for snapshot consistency; what we compare here is "not one row missing, not one row extra").
fn row_identity(rows: &[Value]) -> Vec<(u64, String)> {
    let mut ids: Vec<(u64, String)> = rows
        .iter()
        .map(|row| {
            (
                row["rowId"].as_u64().unwrap_or_default(),
                row["kind"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect();
    ids.sort();
    ids.dedup();
    ids
}

#[test]
fn resume_hands_back_exactly_the_rows_the_client_missed() {
    let (provider, port) = ScriptedProvider::spawn_with_delay(Duration::from_millis(1));
    let base_dir = temp_base("replay");
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
    let live_sid = subscription_id(&session_id, "conn-first");

    // 1) The online subscription takes the snapshot, then drops out: the snapshot is the client's last known state.
    let ack = subscribe(&mut client, &session_id, "conn-first", None);
    assert_eq!(
        ack["result"]["ack"]["mode"],
        json!("snapshot"),
        "subscribe without base stays a snapshot"
    );
    let initial_frame = await_frame(&mut client, &live_sid, "snapshot");
    let initial = payload_of(&initial_frame)["snapshot"].clone();
    let epoch = initial["logEpoch"].as_str().expect("logEpoch").to_string();
    let base_rows = rows_of_snapshot(&initial);
    // The snapshot frame's (fromSeq, toSeq]: TS counts from 0, and toSeq is the snapshot's own watermark.
    assert_eq!(
        initial_frame["params"]["frame"]["fromSeq"].as_u64(),
        Some(0),
        "a snapshot frame is self-contained: fromSeq 0"
    );
    let watermark = initial["seq"].as_u64().expect("snapshot seq");
    assert_eq!(
        initial_frame["params"]["frame"]["toSeq"].as_u64(),
        Some(watermark),
        "the snapshot frame's toSeq must equal the snapshot's own seq"
    );

    let id = client.send(
        "v4/conversation/unsubscribe",
        json!({"sessionId": session_id, "connectionId": "conn-first"}),
    );
    client.response(id);

    // 2) Run two turns while there is no subscriber: projection happens as usual, only nobody receives the frames.
    submit_turn(
        &mut client,
        &session_id,
        "cmd-offline-1",
        "first offline turn",
    );
    submit_turn(
        &mut client,
        &session_id,
        "cmd-offline-2",
        "second offline turn",
    );
    client.pending.clear();

    // 3) Reconnect carrying base → resume, and the resumed frame covers the two offline turns starting from the client's watermark.
    let ack = subscribe(
        &mut client,
        &session_id,
        "conn-back",
        Some((epoch.as_str(), watermark)),
    );
    assert_eq!(
        ack["result"]["ack"]["mode"],
        json!("resume"),
        "same epoch + watermark inside the retention window must resume, not snapshot"
    );
    let back_sid = subscription_id(&session_id, "conn-back");
    let replay = await_frame(&mut client, &back_sid, "deltas");
    assert_eq!(
        replay["params"]["frame"]["fromSeq"].as_u64(),
        Some(watermark),
        "the continuation frame must start where the client stopped"
    );
    let replayed = deltas_from_frame(&replay);
    assert!(
        !replayed.is_empty(),
        "two settled turns cannot replay to zero deltas"
    );
    let replay_texts: Vec<&str> = replayed
        .iter()
        .filter_map(|delta| delta["row"]["text"].as_str())
        .collect();
    for expected in ["first offline turn", "second offline turn"] {
        assert!(
            replay_texts.iter().any(|text| text.contains(expected)),
            "the missed turn's own row must be in the replay; got {replay_texts:?}"
        );
    }

    // 4) The golden invariant: the base row set + the resumed deltas == the full snapshot row set.
    let mut resumed_rows = base_rows.clone();
    apply_to_rows(&mut resumed_rows, &replayed);
    let replay_watermark = replay["params"]["frame"]["toSeq"]
        .as_u64()
        .expect("continuation toSeq");

    let fresh = subscribe(&mut client, &session_id, "conn-fresh", None);
    assert_eq!(fresh["result"]["ack"]["mode"], json!("snapshot"));
    let fresh_frame = await_frame(
        &mut client,
        &subscription_id(&session_id, "conn-fresh"),
        "snapshot",
    );
    let fresh_snapshot = payload_of(&fresh_frame)["snapshot"].clone();
    assert_eq!(
        fresh_snapshot["seq"].as_u64(),
        Some(replay_watermark),
        "the replay watermark must be the authoritative seq"
    );
    assert_eq!(
        row_identity(&resumed_rows),
        row_identity(&rows_of_snapshot(&fresh_snapshot)),
        "resume must reconstruct the same rows a full snapshot carries"
    );

    // 5) The watermark is already aligned → resume with no frame sent (TS's base.seq === currentSeq early exit).
    client.pending.clear();
    let aligned = subscribe(
        &mut client,
        &session_id,
        "conn-aligned",
        Some((epoch.as_str(), replay_watermark)),
    );
    assert_eq!(aligned["result"]["ack"]["mode"], json!("resume"));
    ack_is_followed_by_no_frame(&mut client, &session_id, "conn-aligned");

    // 6) resync uses the same criteria: a valid base → resume; forceSnapshot must override a resumable base.
    client.pending.clear();
    let resynced = resync(
        &mut client,
        &session_id,
        "conn-back",
        Some((epoch.as_str(), watermark)),
        false,
    );
    assert_eq!(
        resynced["result"]["ack"]["mode"],
        json!("resume"),
        "resync with a valid base resumes: {resynced}"
    );
    let recovery = await_frame(&mut client, &back_sid, "deltas");
    assert_eq!(
        recovery["params"]["deliveryKind"].as_str(),
        Some("recovery"),
        "resync frames are recovery, not initial"
    );
    assert_eq!(
        recovery["params"]["frame"]["fromSeq"].as_u64(),
        Some(watermark),
        "recovery must also start at the client's claimed seq"
    );

    let forced_sid = subscription_id(&session_id, "conn-back");
    let forced = resync(
        &mut client,
        &session_id,
        "conn-back",
        Some((epoch.as_str(), watermark)),
        true,
    );
    assert_eq!(
        forced["result"]["ack"]["mode"],
        json!("snapshot"),
        "forceSnapshot must win over a resumable base"
    );
    let forced_frame = await_frame(&mut client, &forced_sid, "snapshot");
    assert_eq!(
        forced_frame["params"]["frame"]["fromSeq"].as_u64(),
        Some(0),
        "a recovery snapshot resets the watermark like TS `sentSeq = 0`"
    );

    // 6b) resync of a subscription the host does not recognize must fail closed (TS `resyncReserved` checks
    //     ownership first). It must never answer "ack present, no frame" — the client would keep waiting for that initial frame.
    let unknown = resync(
        &mut client,
        &session_id,
        "conn-never-subscribed",
        Some((epoch.as_str(), watermark)),
        false,
    );
    assert_eq!(
        unknown["error"]["message"].as_str(),
        Some("fault.subscription.notOwned"),
        "an unknown subscription must be refused by name: {unknown}"
    );
    assert_eq!(
        unknown["error"]["data"]["reasonCode"].as_str(),
        Some("fault.subscription.notOwned")
    );
    assert!(
        unknown.get("result").is_none(),
        "refused resync must not also ack: {unknown}"
    );

    // 7) epoch mismatch → an honest snapshot (never pretend to resume using another logEpoch's watermark).
    let foreign = subscribe(
        &mut client,
        &session_id,
        "conn-foreign",
        Some((format!("{epoch}-foreign").as_str(), replay_watermark)),
    );
    assert_eq!(foreign["result"]["ack"]["mode"], json!("snapshot"));
    await_frame(
        &mut client,
        &subscription_id(&session_id, "conn-foreign"),
        "snapshot",
    );

    // 8) Watermark ahead (the client lies about a position that has not happened yet) → snapshot.
    let ahead = subscribe(
        &mut client,
        &session_id,
        "conn-ahead",
        Some((epoch.as_str(), replay_watermark + 1000)),
    );
    assert_eq!(ahead["result"]["ack"]["mode"], json!("snapshot"));
    await_frame(
        &mut client,
        &subscription_id(&session_id, "conn-ahead"),
        "snapshot",
    );

    // 9) The retention-window lower bound (base.seq already evicted) cannot be exercised in e2e by filling 2000 entries,
    //    so eviction and the lower-bound check are pinned by the journal unit tests in `session.rs`.
    assert!(
        provider.request_bodies().len() >= 2,
        "the offline turns really reached the provider"
    );
}
