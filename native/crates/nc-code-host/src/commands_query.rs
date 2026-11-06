//! `v4/commands/query`：命令幂等查询面。**S0-S3 共存期本文件是 TS schema 的镜像**：
//! 字段与约束以 `packages/shared/src/zcode-protocol-v4/command.ts` 为真源，任何加严都是协议变更（先改 TS）。
//! 本文件对齐 `commandKeySchema` / `commandsQueryParamsSchema` /
//! `commandsQueryResultSchema`，以及 `packages/shared/src/localTtft.ts` 的
//! `localTtftClockSchema`。
//!
//! 为什么是这个方法：它是 S3 协议面里**唯一一个纯只读、只回源已落定 ack**
//! 的查询面。host 在 `admitted_command` 里已经为每条命令写过终态 ack
//! （session 内落盘 + 进程内账本），只差一个把账本读出来的 RPC 出口。
//! 接线它不新增状态、不新增 owner、不触碰 turn 热路径。
//!
//! 与 TS 的差异（fail-closed，逐条记在 crate README）：
//! - TS 的 `clock` 校准走 `readyFlights` 等待后再查账本；这里 `clock` 是纯时钟
//!   探测，**不**触碰账本、**不**触发 admission gate（与 TS `queryCommands` 的
//!   `if (params.clock)` 早退分支语义一致）。
//! - 账本只在进程内：进程重启后未恢复的 key 一律 `"unknown"`（fail-closed，
//!   不猜、不落盘反查）。

use serde_json::{json, Value};

use crate::rpc::{RpcError, CODE_INVALID_PARAMS};

/// `commandsQueryParamsSchema`: `z.array(commandKeySchema).min(1).max(64)`。
pub const MAX_QUERY_COMMANDS: usize = 64;

/// 一条 `commandKeySchema`：`{ sessionId: string | null, commandId: string }`，strict。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandKey {
    pub session_id: Option<String>,
    pub command_id: String,
}

impl CommandKey {
    pub fn to_value(&self) -> Value {
        json!({
            "sessionId": self.session_id,
            "commandId": self.command_id,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueryParams {
    /// `commandsQueryParamsSchema` 的非 clock 分支。
    Ledger { commands: Vec<CommandKey> },
    /// `clock: true`：`refine` 已保证所有 key 的 sessionId 为 null，
    /// 只回时钟，不查账本。
    Clock { commands: Vec<CommandKey> },
}

fn invalid(message: String) -> RpcError {
    RpcError::new(CODE_INVALID_PARAMS, message)
}

/// strict 对象：`keys ⊆ allowed`。zod `.strict()` 对多余键报错，这里同样必须报错
/// 而不是忽略——忽略会让协议形状悄悄漂移。
fn reject_unknown_keys(value: &Value, allowed: &[&str], what: &str) -> Result<(), RpcError> {
    let object = value
        .as_object()
        .ok_or_else(|| invalid(format!("{what}: expected object")))?;
    for key in object.keys() {
        if !allowed.contains(&key.as_str()) {
            return Err(invalid(format!("{what}: unexpected key `{key}`")));
        }
    }
    Ok(())
}

fn parse_command_key(value: &Value) -> Result<CommandKey, RpcError> {
    reject_unknown_keys(value, &["sessionId", "commandId"], "commands[].key")?;
    // sessionId 是 required 且可为 null：缺失必须报错，不能当成 null 桶。
    let session_id = match value.get("sessionId") {
        Some(Value::Null) => None,
        // TS commandKeySchema 是 z.string().nullable()：没有 min(1)，空串合法。
        Some(Value::String(text)) => Some(text.clone()),
        Some(_) => {
            return Err(invalid(
                "commands[].key.sessionId: expected string or null".to_string(),
            ))
        }
        None => {
            return Err(invalid(
                "commands[].key.sessionId: required string|null".to_string(),
            ))
        }
    };
    // TS commandKeySchema 是 z.string().min(1)：没有 max(128)。不要照抄
    // localTtft.ts 的 identifier 上限——那条常量管的是 localTtftClockSchema.instanceId，
    // 另一个 schema。S0-S3 共存期 Rust 侧是**镜像**：加严就是一次未登记的协议变更，
    // 必须先改 TS schema。（main M2 裁决）
    let command_id = match value.get("commandId") {
        Some(Value::String(text)) if !text.is_empty() => text.clone(),
        _ => {
            return Err(invalid(
                "commands[].key.commandId: string min(1)".to_string(),
            ))
        }
    };
    Ok(CommandKey {
        session_id,
        command_id,
    })
}

/// 解析 `commandsQueryParamsSchema`，包括它的 `.strict()` 与 `.refine()`。
pub fn parse_query_params(raw: &Value) -> Result<QueryParams, RpcError> {
    reject_unknown_keys(raw, &["commands", "clock"], "params")?;
    let commands_value = raw
        .get("commands")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid("params.commands: required array".to_string()))?;
    if commands_value.is_empty() {
        return Err(invalid(format!(
            "params.commands: min(1).max({MAX_QUERY_COMMANDS})"
        )));
    }
    if commands_value.len() > MAX_QUERY_COMMANDS {
        return Err(invalid(format!(
            "params.commands: min(1).max({MAX_QUERY_COMMANDS})"
        )));
    }
    let commands = commands_value
        .iter()
        .map(parse_command_key)
        .collect::<Result<Vec<_>, _>>()?;

    match raw.get("clock") {
        None => Ok(QueryParams::Ledger { commands }),
        // `clock: z.literal(true).optional()`：只接受字面量 true。
        Some(Value::Bool(true)) => {
            // `.refine`: clock probes cannot query session commands.
            if let Some(key) = commands.iter().find(|key| key.session_id.is_some()) {
                return Err(invalid(format!(
                    "params.commands: clock probes cannot query session commands (sessionId={})",
                    key.session_id.as_deref().unwrap_or_default()
                )));
            }
            Ok(QueryParams::Clock { commands })
        }
        Some(_) => Err(invalid("params.clock: expected literal true".to_string())),
    }
}

/// `commandQueryItemSchema`: `{ key, result: commandAck | "unknown" }`，strict。
fn query_item(key: &CommandKey, result: Value) -> Value {
    json!({ "key": key.to_value(), "result": result })
}

/// `commandsQueryResultSchema`（非 clock 分支）。
pub fn ledger_result(keys: &[CommandKey], lookup: impl Fn(&CommandKey) -> Option<Value>) -> Value {
    json!({
        "results": keys.iter().map(|key| query_item(key, lookup(key).unwrap_or_else(|| json!("unknown")))).collect::<Vec<_>>(),
    })
}

/// `clock: true` 的结果：全部 `"unknown"` + 一条 `localTtftClockSchema`。
/// TS `queryCommands` 的 clock 分支同样把所有 key 答成 `"unknown"`——
/// 校准是纯时钟探测，不该顺带泄露账本内容。
pub fn clock_result(
    keys: &[CommandKey],
    instance_id: &str,
    received_at: f64,
    sent_at: f64,
) -> Value {
    json!({
        "results": keys.iter().map(|key| query_item(key, json!("unknown"))).collect::<Vec<_>>(),
        "clock": {
            "instanceId": instance_id,
            "receivedAt": received_at,
            "sentAt": sent_at,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(session_id: Option<&str>, command_id: &str) -> Value {
        json!({"sessionId": session_id, "commandId": command_id})
    }

    fn parse(raw: Value) -> Result<QueryParams, RpcError> {
        parse_query_params(&raw)
    }

    #[test]
    fn accepts_one_to_sixty_four_keys() {
        let one = parse(json!({"commands": [key(Some("s1"), "c1")]})).expect("one key");
        assert_eq!(
            one,
            QueryParams::Ledger {
                commands: vec![CommandKey {
                    session_id: Some("s1".to_string()),
                    command_id: "c1".to_string()
                }]
            }
        );
        let many = json!({"commands": (0..MAX_QUERY_COMMANDS)
            .map(|index| key(Some("s1"), &format!("c{index}")))
            .collect::<Vec<_>>()});
        assert!(parse(many).is_ok());
        let too_many = json!({"commands": (0..=MAX_QUERY_COMMANDS)
            .map(|index| key(Some("s1"), &format!("c{index}")))
            .collect::<Vec<_>>()});
        assert!(parse(too_many).is_err(), "max(64) must be enforced");
        assert!(
            parse(json!({"commands": []})).is_err(),
            "min(1) must be enforced"
        );
    }

    #[test]
    fn null_session_id_is_the_global_bucket() {
        let parsed = parse(json!({"commands": [key(None, "c-create")]})).expect("null session");
        assert!(parse(json!({"commands": [key(Some(""), "c-empty-session")]})).is_ok());
        match parsed {
            QueryParams::Ledger { commands } => {
                assert_eq!(commands[0].session_id, None);
                assert_eq!(commands[0].command_id, "c-create");
            }
            QueryParams::Clock { .. } => panic!("must be the ledger branch"),
        }
    }

    #[test]
    fn strict_rejects_unknown_and_missing_fields() {
        // 多余键（zod .strict()）
        assert!(parse(json!({"commands": [key(Some("s"), "c")], "extra": 1})).is_err());
        // key 上多余键
        assert!(
            parse(json!({"commands": [{"sessionId": null, "commandId": "c", "x": 1}]})).is_err()
        );
        // sessionId 必填：缺失不能当成 null 桶
        assert!(parse(json!({"commands": [{"commandId": "c"}]})).is_err());
        // commandId min(1)：空串非法
        assert!(parse(json!({"commands": [key(Some("s"), "")]})).is_err());
        // 不设上限：TS commandKeySchema 只有 min(1)，加严就是未登记的协议变更（M2）。
        assert!(parse(json!({"commands": [key(Some("s"), &"c".repeat(4096))]})).is_ok());
        // sessionId 允许空串：TS 是 z.string().nullable()，没有 min(1)（M2）。
        assert!(parse(json!({"commands": [key(Some(""), "c")]})).is_ok());
        // commands 必填
        assert!(parse(json!({})).is_err());
    }

    #[test]
    fn clock_is_literal_true_and_session_scoped_keys_are_forbidden() {
        let ok = parse(json!({"commands": [key(None, "c1")], "clock": true})).expect("clock");
        assert!(matches!(ok, QueryParams::Clock { .. }));
        // refine: clock 不能带 session 命令
        assert!(parse(json!({"commands": [key(Some("s"), "c")], "clock": true})).is_err());
        // z.literal(true)：false / "true" / 1 都拒绝
        assert!(parse(json!({"commands": [key(None, "c")], "clock": false})).is_err());
        assert!(parse(json!({"commands": [key(None, "c")], "clock": "true"})).is_err());
        assert!(parse(json!({"commands": [key(None, "c")], "clock": 1})).is_err());
    }

    #[test]
    fn missing_keys_answer_unknown_not_an_error() {
        let keys = vec![CommandKey {
            session_id: Some("s1".to_string()),
            command_id: "missing".to_string(),
        }];
        let result = ledger_result(&keys, |_| None);
        assert_eq!(result["results"][0]["result"], json!("unknown"));
        assert_eq!(result["results"][0]["key"]["commandId"], json!("missing"));
    }

    #[test]
    fn found_keys_answer_the_ack_verbatim() {
        let keys = vec![CommandKey {
            session_id: Some("s1".to_string()),
            command_id: "c1".to_string(),
        }];
        let ack = json!({"commandId": "c1", "status": "failed", "revisionAtDecision": 7});
        let result = ledger_result(&keys, |_| Some(ack.clone()));
        assert_eq!(result["results"][0]["result"], ack);
    }

    #[test]
    fn clock_result_never_leaks_the_ledger() {
        let keys = vec![CommandKey {
            session_id: None,
            command_id: "c1".to_string(),
        }];
        // 即使账本里有记录，clock 分支也必须全答 unknown。
        let result = clock_result(&keys, "instance-1", 10.0, 12.0);
        assert_eq!(result["results"][0]["result"], json!("unknown"));
        assert_eq!(result["clock"]["instanceId"], json!("instance-1"));
        assert_eq!(result["clock"]["receivedAt"], json!(10.0));
        assert_eq!(result["clock"]["sentAt"], json!(12.0));
    }
}
