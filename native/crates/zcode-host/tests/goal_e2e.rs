//! goal 子系统 e2e：真实二进制 + 脚本化 provider，验证产品事实而不是"命令被接受"。
//!
//! 覆盖：
//! 1. `/goal` → 续跑轮（model-only，turnHeader.origin=goalContinuation）→ 裁判
//!    （tools:[]、notSatisfied + nextAction）→ 第二轮 → 裁判 pass → goal verified；
//! 2. pauseGoal / resumeGoal 的状态机往返与 availability 投影；
//! 3. stop 打断在途裁判 → goal 强制 paused（`stopPausesActiveGoalTarget`），
//!    且裁判在途时 inputRouting = enqueue/goalVerifierAcceptsFutureInput。
//!
//! 读取全部带超时：路径哪天阻塞，测试必须失败而不是挂住套件。

mod common;

use common::{command_envelope, text_sse, HostProcess, ScriptedResponder};
use serde_json::{json, Value};
use std::io::{BufRead, Write};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

fn provider_config(port: u16, path: &std::path::Path) {
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

/// 订阅后的客户端状态：行集 + goal/availability/inputRouting 投影 + 水位。
struct Harness {
    stdin: std::process::ChildStdin,
    rx: mpsc::Receiver<Value>,
    next_id: u64,
    session_id: String,
    rows: Vec<Value>,
    goal: Value,
    availability: Value,
    input_routing: Value,
    revision: i64,
    log_epoch: String,
}

impl Harness {
    fn send(&mut self, method: &str, params: Value) -> u64 {
        self.next_id += 1;
        let id = self.next_id;
        let line = serde_json::to_string(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))
        .expect("serialize");
        self.stdin
            .write_all(line.as_bytes())
            .and_then(|_| self.stdin.write_all(b"\n"))
            .and_then(|_| self.stdin.flush())
            .expect("write request");
        id
    }

    fn recv(&self, what: &str) -> Value {
        match self.rx.recv_timeout(Duration::from_secs(10)) {
            Ok(line) => line,
            Err(error) => panic!("{what} never arrived within 10s: {error}"),
        }
    }

    fn response(&mut self, id: u64, what: &str) -> Value {
        loop {
            let line = self.recv(what);
            if line.get("id").and_then(Value::as_u64) == Some(id) {
                return line;
            }
            self.absorb(&line);
        }
    }

    fn absorb(&mut self, line: &Value) {
        let payload = &line["params"]["frame"]["payload"];
        match payload["kind"].as_str() {
            Some("snapshot") => {
                let snapshot = &payload["snapshot"];
                if snapshot["sessionId"].as_str() != Some(self.session_id.as_str()) {
                    return;
                }
                self.rows = snapshot["rows"]["window"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                self.goal = snapshot["goal"].clone();
                self.availability = snapshot["availability"].clone();
                self.input_routing = snapshot["inputRouting"].clone();
                self.revision = self
                    .revision
                    .max(snapshot["revision"].as_i64().unwrap_or(0));
                if let Some(log_epoch) = snapshot["logEpoch"].as_str() {
                    self.log_epoch = log_epoch.to_string();
                }
            }
            Some("deltas") => {
                let deltas = payload["deltas"].as_array().cloned().unwrap_or_default();
                for delta in &deltas {
                    match delta["op"].as_str() {
                        Some("row.appended") => self.rows.push(delta["row"].clone()),
                        Some("row.upserted") => {
                            let row_id = delta["row"]["rowId"].as_u64();
                            if let Some(slot) = self
                                .rows
                                .iter_mut()
                                .find(|row| row_id.is_some() && row["rowId"].as_u64() == row_id)
                            {
                                *slot = delta["row"].clone();
                            }
                        }
                        Some("row.removed") => {
                            let from = delta["fromRowId"].as_u64();
                            self.rows.retain(|row| {
                                from.is_some_and(|from| row["rowId"].as_u64().unwrap_or(0) < from)
                            });
                        }
                        Some("state.updated") => {
                            let patch = &delta["patch"];
                            if let Some(revision) = patch["revision"].as_i64() {
                                self.revision = self.revision.max(revision);
                            }
                            if !patch["goal"].is_null() {
                                self.goal = patch["goal"].clone();
                            }
                            if !patch["availability"].is_null() {
                                self.availability = patch["availability"].clone();
                            }
                            if !patch["inputRouting"].is_null() {
                                self.input_routing = patch["inputRouting"].clone();
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    fn goal_status(&self) -> Option<&str> {
        self.goal["status"].as_str()
    }

    /// 读帧直到谓词成立。谓词**先求值一次**：脚本化 provider 是 0 延迟的，
    /// 整个 goal 循环可能在 admission ack 写出之前就跑完，帧全被 `response` 吸收。
    fn pump_until(&mut self, what: &str, mut done: impl FnMut(&Harness) -> bool) {
        if done(self) {
            return;
        }
        for _ in 0..600 {
            let line = self.recv(what);
            self.absorb(&line);
            if done(self) {
                return;
            }
        }
        panic!(
            "{what} never became true; goal={}, routing={}",
            self.goal, self.input_routing
        );
    }

    fn wait_snapshot(&mut self) {
        for _ in 0..40 {
            let line = self.recv("initial snapshot");
            self.absorb(&line);
            if line["params"]["frame"]["payload"]["kind"] == json!("snapshot") {
                return;
            }
        }
        panic!("no snapshot frame after subscribe");
    }

    fn turn_headers(&self) -> Vec<&Value> {
        self.rows
            .iter()
            .filter(|row| row["kind"] == json!("turnHeader"))
            .collect()
    }
}

fn spawn_host(port: u16, tag: &str) -> (HostProcess, Harness, std::path::PathBuf) {
    let base = std::env::temp_dir().join(format!("zcode-host-goal-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let workspace = base.join("workspace");
    let data_dir = base.join("data");
    std::fs::create_dir_all(&workspace).expect("workspace");
    std::fs::create_dir_all(&data_dir).expect("data dir");
    let config_path = base.join("provider.json");
    provider_config(port, &config_path);
    let (process, harness) = boot(&workspace, &data_dir, &config_path);
    (process, harness, base)
}

/// 用同一份 workspace/data-dir 启动 host（重启场景复用）。
fn boot(
    workspace: &std::path::Path,
    data_dir: &std::path::Path,
    config_path: &std::path::Path,
) -> (HostProcess, Harness) {
    let mut process = HostProcess::spawn(workspace, data_dir, config_path, false);
    let stdin = process.stdin();
    let stdout = process.stdout();
    // stdout 交给读线程（管道读做不到带超时）。
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        for line in stdout.lines().map_while(Result::ok) {
            if line.trim().is_empty() {
                continue;
            }
            let parsed: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
            if tx.send(parsed).is_err() {
                return;
            }
        }
    });
    let harness = Harness {
        stdin,
        rx,
        next_id: 0,
        session_id: String::new(),
        rows: Vec::new(),
        goal: Value::Null,
        availability: Value::Null,
        input_routing: Value::Null,
        revision: 0,
        log_epoch: String::new(),
    };
    (process, harness)
}

fn create_and_subscribe(harness: &mut Harness) {
    let id = harness.send(
        "v4/command",
        command_envelope("createSession", "cmd-create", None, json!({})),
    );
    let session_id = harness.response(id, "createSession ack")["result"]["result"]["sessionId"]
        .as_str()
        .expect("session id")
        .to_string();
    harness.session_id = session_id.clone();
    let id = harness.send(
        "v4/conversation/subscribe",
        json!({
            "sessionId": session_id,
            "connectionId": "conn-goal",
            "clientMode": "desktop-continuous",
        }),
    );
    let _ = harness.response(id, "subscribe ack");
    harness.wait_snapshot();
}

#[test]
fn goal_continuation_and_verifier_drive_the_goal_to_verified() {
    let (provider, port) = ScriptedResponder::spawn(vec![
        text_sse("worked on the first slice"),
        text_sse(
            r#"{"passed": false, "reason": "tests missing", "nextAction": "write the tests"}"#,
        ),
        text_sse("wrote the tests"),
        text_sse(r#"{"passed": true, "reason": "all requirements shipped"}"#),
    ]);
    let (_process, mut harness, base) = spawn_host(port, "verified");
    create_and_subscribe(&mut harness);

    let id = harness.send(
        "v4/command",
        command_envelope(
            "sendGoalCommand",
            "cmd-goal",
            Some(&harness.session_id),
            json!({"text": "ship the feature"}),
        ),
    );
    let ack = harness.response(id, "sendGoalCommand admission ack");
    assert_eq!(ack["result"]["status"], json!("accepted"), "{ack}");

    harness.pump_until("goal verified", |harness| {
        harness.goal_status() == Some("verified")
    });

    // 1) 目标投影：两轮裁判、pass 后 durable complete。
    assert_eq!(harness.goal["objective"], json!("ship the feature"));
    assert_eq!(harness.goal["iteration"], json!(2));
    let verifications = harness.goal["verifications"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert_eq!(verifications.len(), 2, "{verifications:?}");
    assert_eq!(verifications[0]["outcome"], json!("notSatisfied"));
    assert_eq!(verifications[0]["nextAction"], json!("write the tests"));
    assert_eq!(verifications[0]["iteration"], json!(1));
    assert_eq!(verifications[1]["outcome"], json!("pass"));
    assert!(
        verifications[0]["anchorRowId"].as_u64().is_some(),
        "verification entries anchor to the goalVerify timeline marker"
    );

    // 2) 两次续跑轮：origin 必须是 goalContinuation，且不产生 userInput 行。
    let headers = harness.turn_headers();
    let continuation_headers: Vec<_> = headers
        .iter()
        .filter(|row| row["origin"] == json!("goalContinuation"))
        .collect();
    assert_eq!(
        continuation_headers.len(),
        2,
        "two continuation turns: {headers:?}"
    );
    for header in &continuation_headers {
        assert_eq!(header["state"], json!("completedSuccess"));
    }
    assert_eq!(
        harness
            .rows
            .iter()
            .filter(|row| row["kind"] == json!("userInput"))
            .count(),
        0,
        "model-only continuation inputs must not render as user bubbles"
    );
    // 3) 裁判时间线：两行 goalVerify marker（running → 终态 upsert 同一行）。
    let markers: Vec<_> = harness
        .rows
        .iter()
        .filter(|row| {
            row["kind"] == json!("timelineMarker") && row["marker"]["type"] == json!("goalVerify")
        })
        .collect();
    assert_eq!(markers.len(), 2, "{markers:?}");
    assert_eq!(markers[0]["marker"]["outcome"], json!("notSatisfied"));
    assert_eq!(markers[1]["marker"]["outcome"], json!("pass"));
    assert_eq!(markers[0]["lane"], json!("turnTailBoundary"));

    // 4) 模型请求：续跑轮 + 裁判交替；裁判请求带验证提示词且 tools 为空。
    let bodies = provider.request_bodies();
    assert_eq!(bodies.len(), 4, "2 turns + 2 verifications: {bodies:?}");
    assert!(bodies[0].contains("<system-reminder>"));
    assert!(
        bodies[0].contains("ship the feature"),
        "continuation prompt quotes the objective"
    );
    assert!(bodies[1].contains("Verify whether the active session goal is actually complete."));
    assert!(
        !bodies[1].contains("\"tools\""),
        "the verifier request must carry no tools at all: {}",
        bodies[1]
    );

    // 5) availability：completed 目标不能 pause/resume。
    assert_eq!(harness.availability["pauseGoal"]["allowed"], json!(false));
    assert_eq!(
        harness.availability["pauseGoal"]["reasonCode"],
        json!("guard.goal.goalNotActive")
    );
    assert_eq!(harness.availability["resumeGoal"]["allowed"], json!(false));

    // 6) 命令账本：终态 accepted 且带上循环结束原因（不是只有"被接受"）。
    let mut settled = Value::Null;
    for _ in 0..100 {
        let id = harness.send(
            "v4/commands/query",
            json!({"commands": [{"sessionId": harness.session_id, "commandId": "cmd-goal"}]}),
        );
        let response = harness.response(id, "commands query");
        settled = response["result"]["results"]
            .as_array()
            .and_then(|items| items.first())
            .cloned()
            .unwrap_or(Value::Null);
        if settled["result"]["status"].as_str().is_some() {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(settled["result"]["status"], json!("accepted"), "{settled}");
    let result = &settled["result"]["result"];
    assert_eq!(result["type"], json!("sendGoalCommand"));
    assert_eq!(result["loopEnd"], json!("verified"), "{result}");

    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn pause_and_resume_round_trip_a_goal() {
    let (provider, port) = ScriptedResponder::spawn(vec![
        text_sse("first slice done"),
        // 没有 nextAction 的 notSatisfied：有效结论，但循环必须停下等用户。
        text_sse(r#"{"passed": false, "reason": "needs review"}"#),
        text_sse("addressed the review"),
        text_sse(r#"{"passed": true, "reason": "review addressed"}"#),
    ]);
    let (_process, mut harness, base) = spawn_host(port, "pause");
    create_and_subscribe(&mut harness);

    let id = harness.send(
        "v4/command",
        command_envelope(
            "sendGoalCommand",
            "cmd-goal",
            Some(&harness.session_id),
            json!({"text": "finish the audit"}),
        ),
    );
    let _ = harness.response(id, "sendGoalCommand ack");
    harness.pump_until("goal awaiting user", |harness| {
        harness.goal_status() == Some("notSatisfied")
    });
    assert_eq!(
        harness.availability["pauseGoal"]["allowed"],
        json!(true),
        "notSatisfied 的 active 目标仍可暂停"
    );
    assert_eq!(harness.availability["resumeGoal"]["allowed"], json!(false));

    // pauseGoal 是 CAS 命令：baseRevision 必须等于当前权威水位。
    let id = harness.send(
        "v4/command",
        json!({
            "type": "pauseGoal",
            "commandId": "cmd-pause",
            "clientId": "e2e",
            "sessionId": harness.session_id,
            "baseRevision": harness.revision,
            "payload": {},
            "issuedAt": 1_700_000_000_000.0,
        }),
    );
    let ack = harness.response(id, "pauseGoal ack");
    assert_eq!(ack["result"]["status"], json!("accepted"), "{ack}");
    harness.pump_until("goal paused", |harness| {
        harness.goal_status() == Some("paused")
    });
    assert_eq!(harness.availability["resumeGoal"]["allowed"], json!(true));
    assert_eq!(harness.availability["pauseGoal"]["allowed"], json!(false));

    // resumeGoal：paused → active 并立即续跑；第二轮裁判 pass。
    let id = harness.send(
        "v4/command",
        json!({
            "type": "resumeGoal",
            "commandId": "cmd-resume",
            "clientId": "e2e",
            "sessionId": harness.session_id,
            "baseRevision": harness.revision,
            "payload": {},
            "issuedAt": 1_700_000_000_000.0,
        }),
    );
    let ack = harness.response(id, "resumeGoal ack");
    assert_eq!(ack["result"]["status"], json!("accepted"), "{ack}");
    harness.pump_until("goal verified after resume", |harness| {
        harness.goal_status() == Some("verified")
    });
    assert_eq!(
        provider.request_bodies().len(),
        4,
        "resume must trigger a real continuation + verification"
    );
    let headers = harness.turn_headers();
    assert_eq!(
        headers
            .iter()
            .filter(|row| row["origin"] == json!("goalContinuation"))
            .count(),
        2
    );

    // 无目标时 resumeGoal 幂等成功（不抛错、不改状态）。
    let id = harness.send(
        "v4/command",
        json!({
            "type": "resumeGoal",
            "commandId": "cmd-resume-again",
            "clientId": "e2e",
            "sessionId": harness.session_id,
            "baseRevision": harness.revision,
            "payload": {},
            "issuedAt": 1_700_000_000_000.0,
        }),
    );
    let ack = harness.response(id, "idempotent resumeGoal ack");
    assert_eq!(ack["result"]["status"], json!("accepted"), "{ack}");

    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn stop_during_the_verifier_pauses_the_goal() {
    let (_provider, port) = ScriptedResponder::spawn_with_delay(
        vec![
            text_sse("did some work"),
            text_sse(r#"{"passed": false, "reason": "not yet", "nextAction": "keep going"}"#),
        ],
        Duration::from_millis(400),
    );
    let (_process, mut harness, base) = spawn_host(port, "stop");
    create_and_subscribe(&mut harness);

    let id = harness.send(
        "v4/command",
        command_envelope(
            "sendGoalCommand",
            "cmd-goal",
            Some(&harness.session_id),
            json!({"text": "long running goal"}),
        ),
    );
    let _ = harness.response(id, "sendGoalCommand ack");
    harness.pump_until("verifier in flight", |harness| {
        harness.goal_status() == Some("verifying")
    });
    assert_eq!(
        harness.input_routing["mode"],
        json!("enqueue"),
        "input during a goal verifier must be queued, not steered"
    );
    assert_eq!(
        harness.input_routing["reasonCode"],
        json!("goalVerifierAcceptsFutureInput")
    );
    assert_eq!(
        harness.availability["pauseGoal"]["allowed"],
        json!(true),
        "verifying 的目标仍然可暂停"
    );

    let id = harness.send(
        "v4/command",
        command_envelope("stop", "cmd-stop", Some(&harness.session_id), json!({})),
    );
    let ack = harness.response(id, "stop ack");
    assert_eq!(ack["result"]["status"], json!("accepted"), "{ack}");

    harness.pump_until("goal paused by stop", |harness| {
        harness.goal_status() == Some("paused")
    });
    // 取消不是结论：verifications 里不能出现第三轮结论。
    assert_eq!(
        harness.goal["verifications"]
            .as_array()
            .map(Vec::len)
            .unwrap_or(0),
        0,
        "a cancelled verification must not be recorded as a conclusion"
    );
    assert_eq!(harness.availability["resumeGoal"]["allowed"], json!(true));

    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn goal_submission_is_validated_before_anything_is_written() {
    let (provider, port) = ScriptedResponder::spawn(vec![text_sse("unused")]);
    let (_process, mut harness, base) = spawn_host(port, "validate");
    create_and_subscribe(&mut harness);

    // 空目标 → rejected emptyObjective，且不产生 goal 投影。
    let id = harness.send(
        "v4/command",
        command_envelope(
            "sendGoalCommand",
            "cmd-empty",
            Some(&harness.session_id),
            json!({"text": "   "}),
        ),
    );
    let ack = harness.response(id, "empty objective ack");
    assert_eq!(ack["result"]["status"], json!("rejected"), "{ack}");
    assert_eq!(ack["result"]["reasonCode"], json!("emptyObjective"));
    assert!(
        harness.goal.is_null(),
        "no goal may be written: {}",
        harness.goal
    );

    // plan 互斥 → rejected guard.planGoalMutuallyExclusive。
    let id = harness.send(
        "v4/command",
        command_envelope(
            "sendGoalCommand",
            "cmd-plan",
            Some(&harness.session_id),
            json!({"text": "ship it", "planEnabled": true}),
        ),
    );
    let ack = harness.response(id, "plan mutual exclusion ack");
    assert_eq!(ack["result"]["status"], json!("rejected"), "{ack}");
    assert_eq!(
        ack["result"]["reasonCode"],
        json!("guard.planGoalMutuallyExclusive")
    );
    assert!(harness.goal.is_null());
    assert_eq!(
        provider.request_bodies().len(),
        0,
        "no model call may happen"
    );

    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn the_goal_survives_a_host_restart() {
    let (provider, port) = ScriptedResponder::spawn(vec![
        text_sse("delivered"),
        text_sse(r#"{"passed": true, "reason": "delivered"}"#),
    ]);
    let (process, mut harness, base) = spawn_host(port, "restart");
    create_and_subscribe(&mut harness);

    let id = harness.send(
        "v4/command",
        command_envelope(
            "sendGoalCommand",
            "cmd-goal",
            Some(&harness.session_id),
            json!({"text": "keep this objective across restarts"}),
        ),
    );
    let _ = harness.response(id, "sendGoalCommand ack");
    harness.pump_until("goal verified", |harness| {
        harness.goal_status() == Some("verified")
    });
    let target_id = harness.goal["targetId"].clone();
    let session_id = harness.session_id.clone();
    assert_eq!(provider.request_bodies().len(), 2);

    // 冷重启：goal 必须从 meta.json 恢复（不是从 transcript 猜）。
    drop(process);
    let (_process, mut restarted) = boot(
        &base.join("workspace"),
        &base.join("data"),
        &base.join("provider.json"),
    );
    restarted.session_id = session_id.clone();
    let subscribe_id = restarted.send(
        "v4/conversation/subscribe",
        json!({
            "sessionId": session_id,
            "connectionId": "conn-restart",
            "clientMode": "desktop-continuous",
        }),
    );
    let _ = restarted.response(subscribe_id, "restart subscribe ack");
    restarted.wait_snapshot();
    assert_eq!(
        restarted.goal["targetId"], target_id,
        "the goal identity survives restart: {}",
        restarted.goal
    );
    assert_eq!(restarted.goal_status(), Some("verified"));
    assert_eq!(
        restarted.goal["objective"],
        json!("keep this objective across restarts")
    );
    assert_eq!(
        restarted.goal["verifications"]
            .as_array()
            .map(Vec::len)
            .unwrap_or(0),
        1,
        "the verification ledger is durable too"
    );

    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn a_user_turn_inside_an_active_goal_is_followed_by_verification() {
    let (provider, port) = ScriptedResponder::spawn(vec![
        text_sse("first pass"),
        text_sse(r#"{"passed": false, "reason": "waiting for the user steer"}"#),
        text_sse("responded to the user"),
        text_sse(r#"{"passed": true, "reason": "complete after the steer"}"#),
    ]);
    let (_process, mut harness, base) = spawn_host(port, "user-turn");
    create_and_subscribe(&mut harness);

    let id = harness.send(
        "v4/command",
        command_envelope(
            "sendGoalCommand",
            "cmd-goal",
            Some(&harness.session_id),
            json!({"text": "finish everything"}),
        ),
    );
    let _ = harness.response(id, "sendGoalCommand ack");
    harness.pump_until("goal awaiting the user", |harness| {
        harness.goal_status() == Some("notSatisfied")
    });
    assert_eq!(provider.request_bodies().len(), 2);

    // 用户轮（`continueActiveTargetAfterTurn`）：轮尾必须先验、再续跑。
    let id = harness.send(
        "v4/command",
        command_envelope(
            "sendText",
            "cmd-user",
            Some(&harness.session_id),
            json!({"text": "also cover the queue subsystem"}),
        ),
    );
    let ack = harness.response(id, "sendText ack");
    assert_eq!(ack["result"]["status"], json!("accepted"), "{ack}");
    harness.pump_until("goal verified after the user turn", |harness| {
        harness.goal_status() == Some("verified")
    });

    assert_eq!(
        provider.request_bodies().len(),
        4,
        "user turn + post-turn verification: {:?}",
        provider.request_bodies()
    );
    // 用户轮是真实用户输入：userInput 行在场（与 model-only 续跑轮相反）。
    let user_rows: Vec<_> = harness
        .rows
        .iter()
        .filter(|row| row["kind"] == json!("userInput"))
        .collect();
    assert_eq!(user_rows.len(), 1, "{:?}", harness.rows);
    assert_eq!(
        user_rows[0]["text"],
        json!("also cover the queue subsystem")
    );
    assert_eq!(user_rows[0]["origin"], json!("realUser"));
    assert_eq!(
        harness.goal["verifications"]
            .as_array()
            .map(Vec::len)
            .unwrap_or(0),
        2
    );
    assert_eq!(harness.goal["iteration"], json!(2));

    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn a_fork_inherits_the_parent_goal_state() {
    let (_provider, port) = ScriptedResponder::spawn(vec![
        text_sse("did some work"),
        text_sse(r#"{"passed": false, "reason": "review pending"}"#),
    ]);
    let (_process, mut harness, base) = spawn_host(port, "fork-goal");
    create_and_subscribe(&mut harness);

    let id = harness.send(
        "v4/command",
        command_envelope(
            "sendGoalCommand",
            "cmd-goal",
            Some(&harness.session_id),
            json!({"text": "audit the whole surface"}),
        ),
    );
    let _ = harness.response(id, "sendGoalCommand ack");
    harness.pump_until("goal awaiting user", |harness| {
        harness.goal_status() == Some("notSatisfied")
    });
    let parent_target = harness.goal["targetId"].clone();

    // fork 目标必须是轮尾 completed 的 assistant 行；CAS 字段来自当前投影水位。
    let target_row = harness
        .rows
        .iter()
        .rev()
        .find(|row| row["kind"] == json!("assistantText") && row["state"] == json!("complete"))
        .cloned()
        .expect("a completed assistant row to fork at");
    let fork_command = json!({
        "type": "forkAssistant",
        "commandId": "cmd-fork",
        "clientId": "e2e",
        "sessionId": harness.session_id,
        "baseRevision": harness.revision,
        "baseLogEpoch": harness.log_epoch,
        "payload": {
            "target": {
                "rowId": target_row["rowId"],
                "entityId": target_row["entityId"],
            }
        },
        "issuedAt": 1_700_000_000_000.0,
    });
    let id = harness.send("v4/command", fork_command);
    let ack = harness.response(id, "forkAssistant ack");
    assert_eq!(ack["result"]["status"], json!("accepted"), "{ack}");

    // 子会话 id 在命令终态里（admission ack 之后才落定）。
    let mut child_id = String::new();
    for _ in 0..100 {
        let id = harness.send(
            "v4/commands/query",
            json!({"commands": [{"sessionId": harness.session_id, "commandId": "cmd-fork"}]}),
        );
        let response = harness.response(id, "fork query");
        let result = response["result"]["results"]
            .as_array()
            .and_then(|items| items.first())
            .map(|item| item["result"].clone())
            .unwrap_or(Value::Null);
        if let Some(id) = result["result"]["sessionId"].as_str() {
            child_id = id.to_string();
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        !child_id.is_empty(),
        "fork must report the child session id"
    );

    // 订阅 child：目标身份与结论都要跟过来（TS cloneTargetForFork）。
    let subscribe_id = harness.send(
        "v4/conversation/subscribe",
        json!({
            "sessionId": child_id,
            "connectionId": "conn-child",
            "clientMode": "desktop-continuous",
        }),
    );
    let _ = harness.response(subscribe_id, "child subscribe ack");
    let mut child_goal = Value::Null;
    for _ in 0..50 {
        let line = harness.recv("child snapshot");
        let payload = &line["params"]["frame"]["payload"];
        if payload["kind"] == json!("snapshot")
            && payload["snapshot"]["sessionId"].as_str() == Some(child_id.as_str())
        {
            child_goal = payload["snapshot"]["goal"].clone();
            break;
        }
    }
    assert_eq!(
        child_goal["targetId"], parent_target,
        "the fork inherits the parent goal identity: {child_goal}"
    );
    assert_eq!(child_goal["objective"], json!("audit the whole surface"));
    assert_eq!(child_goal["status"], json!("notSatisfied"));
    assert_eq!(
        child_goal["verifications"]
            .as_array()
            .map(Vec::len)
            .unwrap_or(0),
        1,
        "the verification ledger follows the fork"
    );

    let _ = std::fs::remove_dir_all(&base);
}
