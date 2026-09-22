//! zcode-proc sidecar：进程树枚举/强杀 + Job 作用域 spawn，替代
//! TS `taskkill /T /F` 与抓取式进程列表（`adapters/src/mcp/{process-tree,windows-job-object}.ts`、
//! `services/src/process/*`）。协议传输由 `zcode-sidecar-proto` 提供。

mod job;
mod process;

pub use process::{kill_tree, list_tree, KillFailure, KillTreeResult, ProcessInfo};

use std::collections::HashMap;
use std::io;

use serde_json::{json, Map, Value};

use zcode_sidecar_proto::{
    bad_request, optional_u64, require_object, require_string, serve, Emitter, HandlerError,
    Registry, SIDECAR_PROTOCOL_VERSION,
};

/// sidecar 标识（ready 事件回显）。
pub const SIDECAR_NAME: &str = "zcode-proc";

/// 运行 sidecar：`ready` → serve 循环，直到 shutdown / EOF / 版本不匹配。
/// 返回前显式 drop 注册表并关闭全部 Job 句柄（KILL_ON_JOB_CLOSE 清理残留子树），
/// 避免 `process::exit` 跳过 TLS 析构导致句柄泄漏语义不确定。
pub fn run() -> i32 {
    let emitter: Emitter<io::Stdout> = Emitter::new(io::stdout());
    if let Err(err) = emitter.emit(
        "ready",
        json!({"sidecar": SIDECAR_NAME, "proto": SIDECAR_PROTOCOL_VERSION}),
    ) {
        eprintln!("{SIDECAR_NAME}: emit ready failed: {err}");
        return 1;
    }

    let mut registry: Registry<io::Stdout> = Registry::new();
    register_handlers(&mut registry);

    let shutdown = {
        let stdin = io::stdin();
        serve(stdin.lock(), &emitter, &registry)
    };
    let code = match shutdown {
        Ok(shutdown) => shutdown.exit_code(),
        Err(err) => {
            // stdin 读失败属不可恢复 IO 错误 → 非零退出并保留诊断。
            eprintln!("{SIDECAR_NAME}: serve failed: {err}");
            1
        }
    };
    drop(registry);
    job::clear_all();
    code
}

fn register_handlers(registry: &mut Registry<io::Stdout>) {
    registry.register("list_tree", |params, _emitter| {
        let obj = require_object(params)?;
        let pid = require_pid(obj)?;
        let processes: Vec<Value> = process::list_tree(pid)
            .iter()
            .map(|info| json!({"pid": info.pid, "ppid": info.ppid, "name": info.name}))
            .collect();
        Ok(json!({ "processes": processes }))
    });

    registry.register("kill_tree", |params, _emitter| {
        let obj = require_object(params)?;
        let pid = require_pid(obj)?;
        let timeout_ms = optional_u64(obj, "timeoutMs", 2000)?;
        let result = process::kill_tree(pid, timeout_ms);
        let failed: Vec<Value> = result
            .failed
            .iter()
            .map(|entry| json!({"pid": entry.pid, "error": entry.error}))
            .collect();
        Ok(json!({ "killed": result.killed, "failed": failed }))
    });

    registry.register("spawn_in_job", |params, _emitter| {
        let obj = require_object(params)?;
        let cmd = require_string(obj, "cmd")?;
        let args = optional_string_array(obj, "args")?;
        let cwd = match obj.get("cwd") {
            None | Some(Value::Null) => None,
            Some(value) => Some(
                value
                    .as_str()
                    .filter(|text| !text.is_empty())
                    .map(str::to_string)
                    .ok_or_else(|| bad_request("params.cwd must be a non-empty string"))?,
            ),
        };
        let env = optional_env(obj)?;
        let (pid, handle) =
            job::spawn(&cmd, &args, cwd.as_deref(), env.as_ref()).map_err(|err| match err {
                job::SpawnError::Setup(message) => bad_request(message),
                // 入 Job 失败：子进程仍在运行，409 并提示 kill_tree 回退（不隐式杀）。
                job::SpawnError::Assign { pid, error } => HandlerError::conflict(format!(
                    "assign pid {pid} to job failed: {error}; child still running, \
                     fallback: kill_tree {{\"pid\":{pid}}}"
                )),
            })?;
        let job_key = job::next_key(pid);
        job::store(job_key.clone(), handle);
        Ok(json!({ "pid": pid, "jobKey": job_key }))
    });

    registry.register("kill_job", |params, _emitter| {
        let obj = require_object(params)?;
        let job_key = require_string(obj, "jobKey")?;
        let terminated = job::terminate(&job_key);
        Ok(json!({ "jobKey": job_key, "terminated": terminated }))
    });
}

/// pid 必填：非负整数且在 u32 范围内，否则 400。
fn require_pid(obj: &Map<String, Value>) -> Result<u32, HandlerError> {
    let raw = obj
        .get("pid")
        .ok_or_else(|| bad_request("params.pid is required"))?
        .as_u64()
        .ok_or_else(|| bad_request("params.pid must be a non-negative integer"))?;
    u32::try_from(raw).map_err(|_| bad_request("params.pid must be a non-negative integer"))
}

/// 可选字符串数组：缺失/null/空数组均合法（spawn 可无参数），
/// 元素必须是字符串且允许空串（镜像 TS args 直传语义）。
fn optional_string_array(obj: &Map<String, Value>, key: &str) -> Result<Vec<String>, HandlerError> {
    match obj.get(key) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(value) => {
            let items = value
                .as_array()
                .ok_or_else(|| bad_request(format!("params.{key} must be an array")))?;
            items
                .iter()
                .map(|item| {
                    item.as_str()
                        .map(str::to_string)
                        .ok_or_else(|| bad_request(format!("params.{key} must contain strings")))
                })
                .collect()
        }
    }
}

/// 可选 env 对象（string→string）：允许空值（Windows 环境变量允许空串）。
fn optional_env(obj: &Map<String, Value>) -> Result<Option<HashMap<String, String>>, HandlerError> {
    match obj.get("env") {
        None | Some(Value::Null) => Ok(None),
        Some(value) => {
            let map = value
                .as_object()
                .ok_or_else(|| bad_request("params.env must be an object"))?;
            map.iter()
                .map(|(key, entry)| {
                    entry
                        .as_str()
                        .map(|text| (key.clone(), text.to_string()))
                        .ok_or_else(|| bad_request("params.env values must be strings"))
                })
                .collect::<Result<HashMap<String, String>, HandlerError>>()
                .map(Some)
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn require_pid_covers_missing_negative_float_and_overflow() {
        let empty = json!({});
        assert!(
            require_pid(empty.as_object().unwrap()).is_err(),
            "缺失 → 400"
        );
        for bad in [json!(-1), json!(1.5), json!("7"), json!(null)] {
            let mut map = Map::new();
            map.insert("pid".into(), bad);
            assert!(require_pid(&map).is_err(), "非法 pid 值 → 400");
        }
        let ok = json!({"pid": 4242});
        assert_eq!(require_pid(ok.as_object().unwrap()).unwrap(), 4242);
        let too_big = json!({"pid": 5_000_000_000u64}); // > u32::MAX
        assert!(
            require_pid(too_big.as_object().unwrap()).is_err(),
            "超出 u32 → 400"
        );
    }

    #[test]
    fn optional_string_array_allows_empty_but_rejects_bad_shapes() {
        let empty = json!({});
        assert!(optional_string_array(empty.as_object().unwrap(), "args")
            .unwrap()
            .is_empty());
        let empty_arr = json!({"args": []});
        assert!(
            optional_string_array(empty_arr.as_object().unwrap(), "args")
                .unwrap()
                .is_empty()
        );
        let with_empty_str = json!({"args": [""]});
        assert_eq!(
            optional_string_array(with_empty_str.as_object().unwrap(), "args").unwrap(),
            vec!["".to_string()],
            "空串参数合法（镜像 TS）"
        );
        let bad = json!({"args": "x"});
        assert!(optional_string_array(bad.as_object().unwrap(), "args").is_err());
        let bad_item = json!({"args": [1]});
        assert!(optional_string_array(bad_item.as_object().unwrap(), "args").is_err());
    }

    #[test]
    fn optional_env_accepts_empty_values_and_rejects_non_strings() {
        let none = json!({});
        assert!(optional_env(none.as_object().unwrap()).unwrap().is_none());
        let ok = json!({"env": {"K": ""}});
        let env = optional_env(ok.as_object().unwrap()).unwrap().expect("env");
        assert_eq!(env.get("K"), Some(&String::new()), "空值合法");
        let non_str = json!({"env": {"K": 1}});
        assert!(optional_env(non_str.as_object().unwrap()).is_err());
        let bad_shape = json!({"env": "x"});
        assert!(optional_env(bad_shape.as_object().unwrap()).is_err());
    }
}
