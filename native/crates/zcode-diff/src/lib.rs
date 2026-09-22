//! zcode-diff sidecar：镜像 npm `diff@9`（jsdiff）的行级差分与补丁应用，
//! 替代 TS 热路径 `structuredPatch` / `applyPatch`。方法：
//! - `diff_unified {old,new,oldLabel?,newLabel?,context?=4,timeoutMs?=5000}`
//!   → `{patch: string|null}`（超时 → `null`，对应 jsdiff 的 `undefined`）；
//! - `structured_patch {同上 + oldHeader?,newHeader?}` → 补丁对象本身或 `null`；
//! - `diff_lines {old,new}` → `{chunks:[{count,added,removed,value}]}`（无超时，
//!   对应 jsdiff `diffLines` 的 Infinity 默认）；
//! - `apply_patch {source,hunks,fuzzFactor?}` → `{applied:true,result}` / `{applied:false}`。
//!
//! 参数问题一律 400；parity 基线由 `tools/gen-jsdiff-fixtures.mjs` 用真实
//! npm diff@9 生成（见 README 的差异清单）。

mod apply;
mod diff;
mod patch;
mod tokenize;

use std::io;

use serde_json::{json, Map, Value};

use zcode_sidecar_proto::{
    bad_request, optional_u64, require_object, serve, Emitter, HandlerError, Registry,
    SIDECAR_PROTOCOL_VERSION,
};

/// sidecar 标识（ready 事件回显）。
pub const SIDECAR_NAME: &str = "zcode-diff";

/// `context` 默认值（jsdiff `structuredPatch` 同源默认）。
const DEFAULT_CONTEXT: u64 = 4;
/// `timeoutMs` 默认值（对齐 TS 调用方的 5000ms）。
const DEFAULT_TIMEOUT_MS: u64 = 5_000;

/// 运行 sidecar：`ready` → serve 循环，直到 shutdown / EOF / 版本不匹配。
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
    for method in [
        "diff_unified",
        "structured_patch",
        "diff_lines",
        "apply_patch",
    ] {
        registry.register(method, |params, _emitter| handle(method, params));
    }

    let shutdown = {
        let stdin = io::stdin();
        serve(stdin.lock(), &emitter, &registry)
    };
    let code = match shutdown {
        Ok(shutdown) => shutdown.exit_code(),
        Err(err) => {
            eprintln!("{SIDECAR_NAME}: serve failed: {err}");
            1
        }
    };
    drop(registry);
    code
}

/// 方法分发入口：registry 与 parity 测试共用同一条实现路径。
pub fn handle(method: &str, params: &Value) -> Result<Value, HandlerError> {
    match method {
        "diff_unified" => diff_unified(params),
        "structured_patch" => structured_patch(params),
        "diff_lines" => diff_lines(params),
        "apply_patch" => apply_patch(params),
        other => Err(HandlerError::not_found(format!("unknown method: {other}"))),
    }
}

/// 必填字符串（允许空串：空文本是合法 diff 输入，交给引擎处理）。
fn require_text(obj: &Map<String, Value>, key: &str) -> Result<String, HandlerError> {
    obj.get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| bad_request(format!("params.{key} must be a string")))
}

/// 可选字符串：缺失/`null` → `None`（对应 jsdiff 的 `undefined`，
/// 输出 JSON 时整体省略该键）；类型不符 → 400。
fn optional_text(obj: &Map<String, Value>, key: &str) -> Result<Option<String>, HandlerError> {
    match obj.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_str()
            .map(|text| Some(text.to_string()))
            .ok_or_else(|| bad_request(format!("params.{key} must be a string"))),
    }
}

/// 解析 `context`：非负整数，默认 4；jsdiff 侧接受任意 number，
/// 本 crate 收紧为非负整数（见 README 差异清单）。
fn parse_context(obj: &Map<String, Value>) -> Result<usize, HandlerError> {
    let raw = optional_u64(obj, "context", DEFAULT_CONTEXT)?;
    usize::try_from(raw).map_err(|_| bad_request("params.context must be a non-negative integer"))
}

/// 解析 `timeoutMs`：非负整数，默认 5000；0 表示尽快超时。
fn parse_timeout(obj: &Map<String, Value>) -> Result<u128, HandlerError> {
    Ok(u128::from(optional_u64(
        obj,
        "timeoutMs",
        DEFAULT_TIMEOUT_MS,
    )?))
}

/// 共享的差分参数解析（文件名/标签与 header 语义见各方法文档）。
struct DiffParams {
    old: String,
    new: String,
    old_file_name: Option<String>,
    new_file_name: Option<String>,
    old_header: Option<String>,
    new_header: Option<String>,
    context: usize,
    timeout_ms: u128,
}

fn parse_diff_params(
    obj: &Map<String, Value>,
    with_headers: bool,
) -> Result<DiffParams, HandlerError> {
    let old_header = if with_headers {
        optional_text(obj, "oldHeader")?
    } else {
        // diff_unified 不暴露 header 参数（TS 调用方不使用 `\t` 头）。
        None
    };
    let new_header = if with_headers {
        optional_text(obj, "newHeader")?
    } else {
        None
    };
    Ok(DiffParams {
        old: require_text(obj, "old")?,
        new: require_text(obj, "new")?,
        old_file_name: optional_text(obj, "oldLabel")?,
        new_file_name: optional_text(obj, "newLabel")?,
        old_header,
        new_header,
        context: parse_context(obj)?,
        timeout_ms: parse_timeout(obj)?,
    })
}

fn diff_unified(params: &Value) -> Result<Value, HandlerError> {
    let obj = require_object(params)?;
    let parsed = parse_diff_params(obj, false)?;
    let built = patch::structured_patch(
        parsed.old_file_name.as_deref(),
        parsed.new_file_name.as_deref(),
        &parsed.old,
        &parsed.new,
        None,
        None,
        parsed.context,
        parsed.timeout_ms,
    );
    Ok(match built {
        Some(built) => json!({"patch": patch::format_patch(&built)}),
        None => json!({"patch": null}),
    })
}

fn structured_patch(params: &Value) -> Result<Value, HandlerError> {
    let obj = require_object(params)?;
    let parsed = parse_diff_params(obj, true)?;
    let built = patch::structured_patch(
        parsed.old_file_name.as_deref(),
        parsed.new_file_name.as_deref(),
        &parsed.old,
        &parsed.new,
        parsed.old_header.as_deref(),
        parsed.new_header.as_deref(),
        parsed.context,
        parsed.timeout_ms,
    );
    Ok(match built {
        Some(built) => built.to_json(),
        None => Value::Null,
    })
}

fn diff_lines(params: &Value) -> Result<Value, HandlerError> {
    let obj = require_object(params)?;
    let old = require_text(obj, "old")?;
    let new = require_text(obj, "new")?;
    // jsdiff diffLines 默认无超时（Infinity）；本方法不接受 timeoutMs。
    let pieces = diff::diff_pieces(&old, &new, u128::MAX).expect("无超时必然产出");
    let chunks: Vec<Value> = pieces
        .into_iter()
        .map(|piece| {
            json!({
                "count": piece.count,
                "added": piece.added,
                "removed": piece.removed,
                "value": piece.value,
            })
        })
        .collect();
    Ok(json!({"chunks": chunks}))
}

/// `fuzzFactor`：仅接受缺失/`null`/`0`（全部 TS 调用方固定 0）。
fn parse_fuzz(obj: &Map<String, Value>) -> Result<(), HandlerError> {
    match obj.get("fuzzFactor") {
        None | Some(Value::Null) => Ok(()),
        Some(value) => match value.as_u64() {
            Some(0) => Ok(()),
            Some(_) => Err(bad_request(
                "params.fuzzFactor must be 0: only exact patch application is supported",
            )),
            None => Err(bad_request(
                "params.fuzzFactor must be a non-negative integer",
            )),
        },
    }
}

/// 解析单个契约 DiffHunk（五个字段必须齐备，行内容允许空串）。
fn parse_hunk(value: &Value) -> Result<patch::Hunk, HandlerError> {
    let obj = value
        .as_object()
        .ok_or_else(|| bad_request("params.hunks must contain objects"))?;
    let number = |key: &str| -> Result<i64, HandlerError> {
        obj.get(key)
            .and_then(Value::as_i64)
            .ok_or_else(|| bad_request(format!("params.hunks[].{key} must be an integer")))
    };
    let lines = obj
        .get("lines")
        .and_then(Value::as_array)
        .ok_or_else(|| bad_request("params.hunks[].lines must be an array"))?;
    let lines = lines
        .iter()
        .map(|line| {
            line.as_str()
                .map(str::to_string)
                .ok_or_else(|| bad_request("params.hunks[].lines must contain strings"))
        })
        .collect::<Result<Vec<String>, HandlerError>>()?;
    Ok(patch::Hunk {
        old_start: number("oldStart")?,
        old_lines: number("oldLines")?,
        new_start: number("newStart")?,
        new_lines: number("newLines")?,
        lines,
    })
}

fn apply_patch(params: &Value) -> Result<Value, HandlerError> {
    let obj = require_object(params)?;
    let source = require_text(obj, "source")?;
    parse_fuzz(obj)?;
    let hunks = obj
        .get("hunks")
        .and_then(Value::as_array)
        .ok_or_else(|| bad_request("params.hunks must be an array"))?;
    let hunks = hunks
        .iter()
        .map(parse_hunk)
        .collect::<Result<Vec<patch::Hunk>, HandlerError>>()?;
    Ok(match apply::apply_patch(&source, &hunks) {
        Some(result) => json!({"applied": true, "result": result}),
        None => json!({"applied": false}),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn require_text_accepts_empty_but_rejects_missing_and_non_string() {
        let obj = json!({"old": ""});
        assert_eq!(require_text(obj.as_object().unwrap(), "old").unwrap(), "");
        assert!(require_text(json!({}).as_object().unwrap(), "old").is_err());
        assert!(require_text(json!({"old": 7}).as_object().unwrap(), "old").is_err());
        assert!(require_text(json!({"old": null}).as_object().unwrap(), "old").is_err());
    }

    #[test]
    fn optional_text_maps_null_and_missing_to_none_but_type_mismatch_to_400() {
        let obj = json!({"oldLabel": null});
        assert_eq!(
            optional_text(obj.as_object().unwrap(), "oldLabel").unwrap(),
            None
        );
        let obj = json!({});
        assert_eq!(
            optional_text(obj.as_object().unwrap(), "oldLabel").unwrap(),
            None
        );
        let obj = json!({"oldLabel": "a.txt"});
        assert_eq!(
            optional_text(obj.as_object().unwrap(), "oldLabel").unwrap(),
            Some("a.txt".to_string())
        );
        let obj = json!({"oldLabel": 3});
        assert_eq!(
            optional_text(obj.as_object().unwrap(), "oldLabel")
                .unwrap_err()
                .body
                .code,
            400
        );
    }

    #[test]
    fn context_and_timeout_defaults_and_validation() {
        let obj = json!({});
        let m = obj.as_object().unwrap();
        assert_eq!(parse_context(m).unwrap(), 4, "默认 context=4");
        assert_eq!(parse_timeout(m).unwrap(), 5000, "默认 timeoutMs=5000");

        let obj = json!({"context": 3, "timeoutMs": 0});
        let m = obj.as_object().unwrap();
        assert_eq!(parse_context(m).unwrap(), 3);
        assert_eq!(parse_timeout(m).unwrap(), 0, "timeoutMs 0 合法");

        for bad in [
            json!({"context": -1}),
            json!({"context": 1.5}),
            json!({"timeoutMs": -5}),
        ] {
            let m = bad.as_object().unwrap();
            assert!(
                parse_context(m).is_err() || parse_timeout(m).is_err(),
                "{bad} → 400"
            );
        }
    }

    #[test]
    fn fuzz_factor_only_zero_is_supported() {
        assert!(
            parse_fuzz(json!({}).as_object().unwrap()).is_ok(),
            "缺失 = 0"
        );
        assert!(parse_fuzz(json!({"fuzzFactor": null}).as_object().unwrap()).is_ok());
        assert!(parse_fuzz(json!({"fuzzFactor": 0}).as_object().unwrap()).is_ok());
        let err = parse_fuzz(json!({"fuzzFactor": 1}).as_object().unwrap()).unwrap_err();
        assert_eq!(err.body.code, 400);
        assert!(
            err.body.message.contains("must be 0"),
            "{}",
            err.body.message
        );
        assert!(parse_fuzz(json!({"fuzzFactor": -1}).as_object().unwrap()).is_err());
        assert!(parse_fuzz(json!({"fuzzFactor": 0.5}).as_object().unwrap()).is_err());
    }

    #[test]
    fn parse_hunk_requires_contract_shape() {
        let good = json!({"oldStart":1,"oldLines":2,"newStart":1,"newLines":2,"lines":[" a"]});
        let hunk = parse_hunk(&good).expect("契约 hunk");
        assert_eq!(hunk.old_start, 1);
        assert_eq!(hunk.lines, vec![" a"]);
        // 允许空行与 0/负起点（jsdiff 的 oldStart 可 ≤0）。
        let zero = json!({"oldStart":0,"oldLines":0,"newStart":-2,"newLines":3,"lines":[""]});
        assert!(parse_hunk(&zero).is_ok());
        // 缺字段 / 类型不符 → 400。
        assert!(parse_hunk(&json!({"oldStart":1})).is_err());
        assert!(parse_hunk(&json!({
            "oldStart":1,"oldLines":2,"newStart":1,"newLines":2,"lines":"x"
        }))
        .is_err());
        assert!(parse_hunk(&json!({
            "oldStart":1,"oldLines":2,"newStart":1,"newLines":2,"lines":[7]
        }))
        .is_err());
        assert!(parse_hunk(&json!("hunk")).is_err());
    }

    #[test]
    fn handle_dispatches_methods_and_rejects_unknown() {
        let result =
            handle("diff_lines", &json!({"old": "a\n", "new": "a\nb\n"})).expect("diff_lines ok");
        assert_eq!(result["chunks"].as_array().expect("chunks").len(), 2);

        let err = handle("no_such", &json!({})).unwrap_err();
        assert_eq!(err.body.code, 404);

        // 缺 old → 400。
        let err = handle("diff_unified", &json!({"new": "x"})).unwrap_err();
        assert_eq!(err.body.code, 400);

        // fuzzFactor 1 → 400。
        let err = handle(
            "apply_patch",
            &json!({"source": "a", "hunks": [], "fuzzFactor": 1}),
        )
        .unwrap_err();
        assert_eq!(err.body.code, 400);
    }

    #[test]
    fn unified_and_structured_timeout_return_null() {
        let old: String = (0..5000).map(|i| format!("old {i}\n")).collect();
        let new: String = (0..5000).map(|i| format!("new {i}\n")).collect();
        let params = json!({"old": old, "new": new, "timeoutMs": 0});
        let unified = handle("diff_unified", &params).expect("响应正常");
        assert_eq!(unified, json!({"patch": null}), "超时 → patch:null");
        let structured = handle("structured_patch", &params).expect("响应正常");
        assert_eq!(structured, Value::Null, "超时 → 裸 null");
    }

    #[test]
    fn apply_patch_result_shapes() {
        let ok = handle(
            "apply_patch",
            &json!({
                "source": "a\nb\nc\n",
                "hunks": [{"oldStart":1,"oldLines":3,"newStart":1,"newLines":3,
                           "lines":[" a","-b","+B"," c"]}],
            }),
        )
        .expect("可应用");
        assert_eq!(ok, json!({"applied": true, "result": "a\nB\nc\n"}));

        let fail = handle(
            "apply_patch",
            &json!({
                "source": "zzz",
                "hunks": [{"oldStart":1,"oldLines":3,"newStart":1,"newLines":3,
                           "lines":[" a","-b","+B"," c"]}],
            }),
        )
        .expect("响应正常");
        assert_eq!(fail, json!({"applied": false}));
    }
}
