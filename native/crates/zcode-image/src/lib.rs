//! zcode-image sidecar：解码 → 几何变换 → 编码热路径，替代 TS
//! `adapters/src/image/*` 的 jimp 调用。方法：`info {data}`、
//! `transform {data, ops|op, output, quality?}`；输入问题一律 400，内部编码失败 500。

mod ops;

use std::io;

use base64::Engine;
use serde_json::{json, Map, Value};

use zcode_sidecar_proto::{
    bad_request, optional_u64, require_object, serve, Emitter, HandlerError, Registry,
    SIDECAR_PROTOCOL_VERSION,
};

use ops::{Op, OutputFormat};

/// sidecar 标识（ready 事件回显）。
pub const SIDECAR_NAME: &str = "zcode-image";

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
    register_handlers(&mut registry);

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

fn register_handlers(registry: &mut Registry<io::Stdout>) {
    registry.register("info", |params, _emitter| {
        let obj = require_object(params)?;
        let data = require_data(obj)?;
        let meta = ops::info(&data).map_err(bad_request)?;
        Ok(json!({
            "width": meta.width,
            "height": meta.height,
            "format": meta.format,
            "mime": meta.mime,
        }))
    });

    registry.register("transform", |params, _emitter| {
        let obj = require_object(params)?;
        let data = require_data(obj)?;
        let op_list = parse_ops(obj)?;
        let output = parse_output(obj)?;
        let quality = parse_quality(obj)?;
        let source = ops::decode_rgba(&data).map_err(bad_request)?;
        let transformed = ops::apply_ops(source, &op_list);
        let (width, height) = transformed.dimensions();
        // 编码失败属内部错误（输入位图已验证），映射 500 而非伪装成参数问题。
        let bytes = ops::encode(&transformed, output, quality).map_err(HandlerError::internal)?;
        Ok(json!({
            "data": base64::engine::general_purpose::STANDARD.encode(&bytes),
            "width": width,
            "height": height,
            "mime": output.mime(),
            "sizeBytes": bytes.len(),
        }))
    });
}

/// 取 `data` 字段（必须是字符串，允许空串：空数据由解码层给出 TS 对齐的 empty 文案）。
fn require_data(obj: &Map<String, Value>) -> Result<String, HandlerError> {
    obj.get("data")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| bad_request("params.data must be a string"))
}

/// 解析 `ops` 数组或单个 `op`；两者都缺 → 400；空数组合法（仅重编码）。
fn parse_ops(obj: &Map<String, Value>) -> Result<Vec<Op>, HandlerError> {
    let value = obj
        .get("ops")
        .filter(|value| !value.is_null())
        .or_else(|| obj.get("op").filter(|value| !value.is_null()))
        .ok_or_else(|| bad_request("params.ops or params.op is required"))?;
    match value {
        Value::Array(items) => items.iter().map(parse_op).collect(),
        other => Ok(vec![parse_op(other)?]),
    }
}

/// 单个 op：`{type, width, height}`；type 限 contain/scaleToFit/cover/fill。
fn parse_op(value: &Value) -> Result<Op, HandlerError> {
    let map = value
        .as_object()
        .ok_or_else(|| bad_request("op must be an object"))?;
    let kind = map
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| bad_request("op.type must be a string"))?;
    let kind = match kind {
        "contain" | "scaleToFit" => ops::OpKind::Contain,
        "cover" => ops::OpKind::Cover,
        "fill" => ops::OpKind::Fill,
        other => {
            return Err(bad_request(format!(
                "op.type must be contain|scaleToFit|cover|fill: {other}"
            )));
        }
    };
    Ok(Op {
        kind,
        box_w: require_positive_u32(map, "width")?,
        box_h: require_positive_u32(map, "height")?,
    })
}

/// 正整数 u32（0 / 浮点 / 超范围 / 缺失 → 400）。
fn require_positive_u32(map: &Map<String, Value>, key: &str) -> Result<u32, HandlerError> {
    let bad = || bad_request(format!("op.{key} must be a positive integer"));
    let raw = map
        .get(key)
        .ok_or_else(|| bad_request(format!("op.{key} is required")))?
        .as_u64()
        .ok_or_else(bad)?;
    let value = u32::try_from(raw).map_err(|_| bad())?;
    if value == 0 {
        return Err(bad());
    }
    Ok(value)
}

/// 输出容器：jpeg | png | webp（其余 → 400）。
fn parse_output(obj: &Map<String, Value>) -> Result<OutputFormat, HandlerError> {
    match require_string_field(obj, "output")?.as_str() {
        "jpeg" => Ok(OutputFormat::Jpeg),
        "png" => Ok(OutputFormat::Png),
        "webp" => Ok(OutputFormat::Webp),
        other => Err(bad_request(format!(
            "params.output must be jpeg|png|webp: {other}"
        ))),
    }
}

/// 必填非空字符串字段（缺失/类型不符/空串 → 400）。
fn require_string_field(obj: &Map<String, Value>, key: &str) -> Result<String, HandlerError> {
    let value = obj
        .get(key)
        .ok_or_else(|| bad_request(format!("params.{key} is required")))?;
    let text = value
        .as_str()
        .ok_or_else(|| bad_request(format!("params.{key} must be a string")))?;
    if text.is_empty() {
        return Err(bad_request(format!("params.{key} must not be empty")));
    }
    Ok(text.to_string())
}

/// quality 仅作用于 jpeg；默认 100，钳制 1..=100（png/webp 忽略，镜像 TS 语义）。
fn parse_quality(obj: &Map<String, Value>) -> Result<u8, HandlerError> {
    let raw = optional_u64(obj, "quality", 100)?;
    Ok(u8::try_from(raw).unwrap_or(u8::MAX).clamp(1, 100))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ops_accepts_array_single_and_empty_but_requires_one_shape() {
        assert!(
            parse_ops(json!({}).as_object().unwrap()).is_err(),
            "ops/op 均缺失 → 400"
        );
        let empty = parse_ops(json!({"ops": []}).as_object().unwrap()).unwrap();
        assert!(empty.is_empty(), "空数组 = 仅重编码");
        let single = parse_ops(
            json!({"op": {"type": "cover", "width": 4, "height": 4}})
                .as_object()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(single.len(), 1);
        assert_eq!(single[0].kind, ops::OpKind::Cover);
        let array = parse_ops(
            json!({"ops": [
                {"type": "contain", "width": 2, "height": 2},
                {"type": "scaleToFit", "width": 3, "height": 3},
                {"type": "cover", "width": 4, "height": 4},
                {"type": "fill", "width": 5, "height": 5}
            ]})
            .as_object()
            .unwrap(),
        )
        .unwrap();
        assert_eq!(array.len(), 4);
        assert_eq!(array[0].kind, ops::OpKind::Contain);
        assert_eq!(
            array[1].kind,
            ops::OpKind::Contain,
            "scaleToFit 是 contain 别名"
        );
        assert_eq!(array[2].kind, ops::OpKind::Cover);
        assert_eq!(array[3].kind, ops::OpKind::Fill);
        // ops:null 回落到 op。
        let fallback = parse_ops(
            json!({"ops": null, "op": {"type": "fill", "width": 1, "height": 1}})
                .as_object()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(fallback.len(), 1);
    }

    #[test]
    fn parse_op_rejects_bad_shapes_types_and_ranges() {
        for bad in [
            json!({"type": "rotate", "width": 1, "height": 1}),
            json!({"type": "contain", "width": 0, "height": 1}),
            json!({"type": "contain", "width": 1.5, "height": 1}),
            json!({"type": "contain", "width": -1, "height": 1}),
            json!({"type": "contain", "height": 1}),
            json!({"width": 1, "height": 1}),
            json!("contain"),
            json!(7),
        ] {
            assert!(parse_op(&bad).is_err(), "{bad} → 400");
        }
        assert!(parse_op(&json!({"type":"fill","width":5000000000u64,"height":1})).is_err());
    }

    #[test]
    fn parse_output_and_quality_contract() {
        let jpeg = json!({"output": "jpeg"});
        assert_eq!(
            parse_output(jpeg.as_object().unwrap()).unwrap(),
            OutputFormat::Jpeg
        );
        let upper = json!({"output": "PNG"});
        assert!(
            parse_output(upper.as_object().unwrap()).is_err(),
            "大小写不匹配 → 400"
        );
        let missing = json!({});
        assert!(
            parse_output(missing.as_object().unwrap()).is_err(),
            "缺失 → 400"
        );

        let default = json!({});
        assert_eq!(
            parse_quality(default.as_object().unwrap()).unwrap(),
            100,
            "默认 100"
        );
        let q42 = json!({"quality": 42});
        assert_eq!(parse_quality(q42.as_object().unwrap()).unwrap(), 42);
        let q0 = json!({"quality": 0});
        assert_eq!(
            parse_quality(q0.as_object().unwrap()).unwrap(),
            1,
            "钳制下限 1"
        );
        let q10k = json!({"quality": 10_000});
        assert_eq!(
            parse_quality(q10k.as_object().unwrap()).unwrap(),
            100,
            "钳制上限 100"
        );
        let qfloat = json!({"quality": 1.5});
        assert!(parse_quality(qfloat.as_object().unwrap()).is_err());
    }

    #[test]
    fn require_data_allows_empty_string_but_rejects_missing_and_non_string() {
        let empty = json!({"data": ""});
        assert_eq!(
            require_data(empty.as_object().unwrap()).unwrap(),
            "",
            "空串交给解码层给 TS 文案"
        );
        let missing = json!({});
        assert!(require_data(missing.as_object().unwrap()).is_err());
        let num = json!({"data": 7});
        assert!(require_data(num.as_object().unwrap()).is_err());
        let nul = json!({"data": null});
        assert!(require_data(nul.as_object().unwrap()).is_err());
    }
}
