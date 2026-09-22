//! e2e：真实 `zcode-image` 二进制走完整 sidecar 协议 + 真实位图往返
//! （生成 PNG → info/transform → 解码断言尺寸/魔数/错误路径）。

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use base64::Engine;
use serde_json::{json, Value};
use zcode_sidecar_proto::VERSION_MISMATCH_PREFIX;

/// 协议客户端：单问单答（sidecar 单线程 serve，顺序确定）。
struct Client {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
}

impl Client {
    fn spawn() -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_zcode-image"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn zcode-image");
        let stdin = child.stdin.take().expect("stdin piped");
        let stdout = BufReader::new(child.stdout.take().expect("stdout piped"));
        Self {
            child,
            stdin: Some(stdin),
            stdout,
        }
    }

    fn read_frame(&mut self) -> Value {
        let mut line = String::new();
        let read = self.stdout.read_line(&mut line).expect("read frame");
        assert!(read > 0, "sidecar 在预期响应前关闭了 stdout");
        serde_json::from_str(line.trim()).expect("frame must be json")
    }

    fn send_raw(&mut self, line: &str) {
        let stdin = self.stdin.as_mut().expect("stdin open");
        writeln!(stdin, "{line}").expect("write frame");
        stdin.flush().expect("flush frame");
    }

    fn send(&mut self, frame: Value) {
        let line = frame.to_string();
        self.send_raw(&line);
    }

    fn request(&mut self, id: u64, method: &str, params: Value) -> Value {
        self.send(json!({"v":1,"id":id,"method":method,"params":params}));
        let frame = self.read_frame();
        assert_eq!(frame["id"], json!(id), "响应回显 id");
        frame
    }

    fn wait_exit(&mut self) -> i32 {
        let status = self.child.wait().expect("wait exit");
        status.code().unwrap_or(-1)
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn spawn_ready() -> Client {
    let mut client = Client::spawn();
    let ready = client.read_frame();
    assert_eq!(ready["event"], json!("ready"), "首帧必须是 ready: {ready}");
    assert_eq!(ready["data"]["sidecar"], json!("zcode-image"));
    assert_eq!(ready["data"]["proto"], json!(1));
    client
}

/// 生成一张带渐变的 RGBA PNG（测试输入，非固定 fixture）。
fn make_png(width: u32, height: u32) -> Vec<u8> {
    let image = image::RgbaImage::from_fn(width, height, |x, y| {
        image::Rgba([
            (x * 37 % 251) as u8,
            (y * 53 % 241) as u8,
            ((x + y) * 17 % 239) as u8,
            255,
        ])
    });
    use image::ImageEncoder;
    let mut buffer = Vec::new();
    image::codecs::png::PngEncoder::new(&mut buffer)
        .write_image(
            image.as_raw(),
            width,
            height,
            image::ExtendedColorType::Rgba8,
        )
        .expect("png encode");
    buffer
}

fn to_b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn from_b64(text: &str) -> Vec<u8> {
    base64::engine::general_purpose::STANDARD
        .decode(text)
        .expect("result base64")
}

fn assert_error_code(frame: &Value, code: u64) {
    assert_eq!(frame["ok"], json!(false), "期望错误响应: {frame}");
    assert_eq!(frame["error"]["code"], json!(code), "错误码: {frame}");
}

#[test]
fn protocol_lifecycle_ready_mismatch_404_and_shutdown() {
    let mut client = spawn_ready();

    // 版本不匹配 → 400 + 前缀 + exit 1。
    client.send(json!({"v":2,"id":9,"method":"info","params":{"data":"x"}}));
    let frame = client.read_frame();
    assert_error_code(&frame, 400);
    let message = frame["error"]["message"].as_str().expect("message");
    assert!(
        message.starts_with(VERSION_MISMATCH_PREFIX),
        "消息须带版本前缀: {message}"
    );
    drop(client);
    // 重新起一个实例测 404/shutdown（上一个已 fail-closed）。
    let mut client = spawn_ready();
    client.send_raw("not json at all");
    let event = client.read_frame();
    assert_eq!(event["event"], json!("proto_error"));
    let resp = client.request(1, "no_such_method", json!({}));
    assert_error_code(&resp, 404);
    client.send(json!({"v":1,"id":2,"method":"shutdown","params":{}}));
    let frame = client.read_frame();
    assert_eq!(frame["ok"], json!(true));
    assert_eq!(client.wait_exit(), 0, "shutdown → exit 0");
}

#[test]
fn info_reports_dimensions_format_and_mime() {
    let mut client = spawn_ready();
    let png = make_png(3, 2);
    let resp = client.request(1, "info", json!({"data": to_b64(&png)}));
    assert_eq!(resp["ok"], json!(true), "info 失败: {resp}");
    assert_eq!(resp["result"]["width"], json!(3));
    assert_eq!(resp["result"]["height"], json!(2));
    assert_eq!(resp["result"]["format"], json!("png"));
    assert_eq!(resp["result"]["mime"], json!("image/png"));
    client.send(json!({"v":1,"id":2,"method":"shutdown","params":{}}));
    assert_eq!(client.read_frame()["ok"], json!(true));
    assert_eq!(client.wait_exit(), 0);
}

#[test]
fn transform_geometry_contain_cover_fill_and_sequence() {
    let mut client = spawn_ready();
    let src = to_b64(&make_png(8, 4));

    // contain：8x4 装 4x4 → 宽贴合 4x2。
    let resp = client.request(
        1,
        "transform",
        json!({
            "data": src,
            "op": {"type": "contain", "width": 4, "height": 4},
            "output": "png",
        }),
    );
    assert_eq!(resp["ok"], json!(true), "contain 失败: {resp}");
    assert_eq!(resp["result"]["width"], json!(4));
    assert_eq!(resp["result"]["height"], json!(2));
    assert_eq!(resp["result"]["mime"], json!("image/png"));
    let bytes = from_b64(resp["result"]["data"].as_str().expect("b64"));
    assert_eq!(
        &bytes[..8],
        &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A],
        "png 魔数"
    );
    assert_eq!(
        resp["result"]["sizeBytes"].as_u64().expect("size") as usize,
        bytes.len(),
        "sizeBytes 与实际字节一致"
    );
    // 结果可再次 info（真实往返解码）。
    let info = client.request(2, "info", json!({"data": to_b64(&bytes)}));
    assert_eq!(info["result"]["width"], json!(4));
    assert_eq!(info["result"]["height"], json!(2));

    // cover：8x4 覆盖 4x4 → 横向溢出中心裁切 → 4x4。
    let resp = client.request(
        3,
        "transform",
        json!({
            "data": src,
            "ops": [{"type": "cover", "width": 4, "height": 4}],
            "output": "png",
        }),
    );
    assert_eq!(resp["result"]["width"], json!(4));
    assert_eq!(resp["result"]["height"], json!(4));

    // fill：拉伸到 3x3。
    let resp = client.request(
        4,
        "transform",
        json!({
            "data": src,
            "op": {"type": "fill", "width": 3, "height": 3},
            "output": "png",
        }),
    );
    assert_eq!(resp["result"]["width"], json!(3));
    assert_eq!(resp["result"]["height"], json!(3));

    // 顺序执行：contain 8x4→(2,1)，再 fill →(5,1)。
    let resp = client.request(
        5,
        "transform",
        json!({
            "data": src,
            "ops": [
                {"type": "contain", "width": 2, "height": 2},
                {"type": "fill", "width": 5, "height": 1},
            ],
            "output": "png",
        }),
    );
    assert_eq!(resp["result"]["width"], json!(5));
    assert_eq!(resp["result"]["height"], json!(1));

    // 空 ops = 仅重编码，尺寸不变。
    let resp = client.request(
        6,
        "transform",
        json!({
            "data": src,
            "ops": [],
            "output": "png",
        }),
    );
    assert_eq!(resp["result"]["width"], json!(8));
    assert_eq!(resp["result"]["height"], json!(4));

    client.send(json!({"v":1,"id":7,"method":"shutdown","params":{}}));
    assert_eq!(client.read_frame()["ok"], json!(true));
    assert_eq!(client.wait_exit(), 0);
}

#[test]
fn transform_output_containers_jpeg_webp_and_quality_effect() {
    let mut client = spawn_ready();
    let src = to_b64(&make_png(16, 9));
    let fill = json!({"type": "fill", "width": 8, "height": 8});

    // jpeg + 显式 quality → FFD8 魔数与 mime。
    let resp = client.request(
        1,
        "transform",
        json!({
            "data": src, "op": fill, "output": "jpeg", "quality": 10,
        }),
    );
    assert_eq!(resp["ok"], json!(true), "jpeg 失败: {resp}");
    assert_eq!(resp["result"]["mime"], json!("image/jpeg"));
    let jpeg = from_b64(resp["result"]["data"].as_str().expect("b64"));
    assert_eq!(&jpeg[..2], &[0xFF, 0xD8], "jpeg 魔数");
    // 低质量应明显小于默认质量（有损编码单调性烟测）。
    let resp_high = client.request(
        2,
        "transform",
        json!({
            "data": src, "op": fill, "output": "jpeg",
        }),
    );
    let high_size = resp_high["result"]["sizeBytes"].as_u64().expect("size");
    let low_size = resp["result"]["sizeBytes"].as_u64().expect("size");
    assert!(
        low_size < high_size,
        "quality 10 ({low_size}B) 应小于默认100 ({high_size}B)"
    );

    // webp 输出：RIFF/WEBP 魔数 + mime。
    let resp = client.request(
        3,
        "transform",
        json!({
            "data": src, "op": fill, "output": "webp",
        }),
    );
    assert_eq!(resp["result"]["mime"], json!("image/webp"));
    let webp = from_b64(resp["result"]["data"].as_str().expect("b64"));
    assert_eq!(&webp[..4], b"RIFF");
    assert_eq!(&webp[8..12], b"WEBP");

    client.send(json!({"v":1,"id":4,"method":"shutdown","params":{}}));
    assert_eq!(client.read_frame()["ok"], json!(true));
    assert_eq!(client.wait_exit(), 0);
}

#[test]
fn invalid_inputs_map_to_400_and_keep_serving() {
    let mut client = spawn_ready();
    let png = to_b64(&make_png(2, 2));

    // 空 data → 400（TS empty 文案），继续服务。
    let resp = client.request(1, "info", json!({"data": ""}));
    assert_error_code(&resp, 400);
    assert!(
        resp["error"]["message"]
            .as_str()
            .expect("message")
            .contains("empty"),
        "{}",
        resp["error"]["message"]
    );

    // 非法 base64 → 400。
    let resp = client.request(2, "info", json!({"data": "!!!not-base64!!!"}));
    assert_error_code(&resp, 400);

    // 合法 base64 但非图片 → 400。
    let resp = client.request(3, "info", json!({"data": to_b64(b"hello")}));
    assert_error_code(&resp, 400);
    assert_eq!(
        resp["error"]["message"],
        json!("Unable to decode image data"),
        "与 TS 解码失败文案一致"
    );

    // 缺 data → 400。
    let resp = client.request(4, "info", json!({}));
    assert_error_code(&resp, 400);

    // transform：未知 op 类型 / 缺 ops / 非法 output / width 0 → 400。
    let resp = client.request(
        5,
        "transform",
        json!({
            "data": png, "op": {"type": "rotate", "width": 1, "height": 1}, "output": "png",
        }),
    );
    assert_error_code(&resp, 400);
    let resp = client.request(6, "transform", json!({"data": png, "output": "png"}));
    assert_error_code(&resp, 400);
    let resp = client.request(
        7,
        "transform",
        json!({
            "data": png, "op": {"type": "fill", "width": 1, "height": 1}, "output": "gif",
        }),
    );
    assert_error_code(&resp, 400);
    let resp = client.request(
        8,
        "transform",
        json!({
            "data": png, "op": {"type": "fill", "width": 0, "height": 1}, "output": "png",
        }),
    );
    assert_error_code(&resp, 400);

    // 错误之后仍正常服务。
    let resp = client.request(9, "info", json!({"data": png}));
    assert_eq!(resp["ok"], json!(true), "错误后必须继续服务: {resp}");

    client.send(json!({"v":1,"id":10,"method":"shutdown","params":{}}));
    assert_eq!(client.read_frame()["ok"], json!(true));
    assert_eq!(client.wait_exit(), 0);
}
