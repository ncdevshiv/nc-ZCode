# zcode-sidecar-proto

NDJSON sidecar stdio 协议（spec: `specs/rust-native-crates.md` §Sidecar stdio protocol，
协议版本 `1`）。S1 各 island bin（`zcode-watch`、`zcode-scan` 等）唯一允许依赖的公共 crate。

## 提供的能力

- `Frame`：request `{v,id,method,params}` / response `{v,id,ok,result|error}` /
  event `{v,event,data}`；`encode()` 输出单行 JSON（key 顺序即线协议顺序），
  `parse()` 从 `&str` 返回类型化 `ParseError`（`InvalidJson` / `NotAnObject` /
  `UnknownShape` / `MissingField` / `InvalidFieldType`）。
- `serve(reader, &Emitter, &Registry)`：`BufRead` 行循环。内建 `shutdown`
  （成功响应 + `Shutdown::Graceful`）、版本检查（400 `PROTOCOL_VERSION_MISMATCH` +
  `Shutdown::ProtocolError` → exit 1）、未知方法 404、handler panic → 500、
  非法行 → `proto_error` 事件（raw 截断 512 字节）后**继续服务**；stdin EOF = 优雅退出。
- `Emitter<W>`：`Arc<Mutex<W>>`，handler 与服务循环共用一把锁，事件可任意穿插响应；
  可 `clone()` 进其它线程（如 watcher emitter 线程、扫描线程）。
- `params::*`：`require_object` / `require_string` / `require_string_array` /
  `optional_bool` / `optional_u64` / `optional_u32`，失败统一为 400 `HandlerError`。

## 与 spec 的取舍（有意为之）

- **缺失 `params` 按 `null` 解析**：交给 handler 校验并回 400，而不是把整行判为
  `proto_error`（更细的诊断，仍满足“400 协议/参数”）。
- **客户端发来的 response / event 帧**：形状合法但方向错误，统一上报
  `proto_error`（reason: `unexpected response/event frame from client`）并继续服务。
- **非 UTF-8 行**：`read_until` + lossy 解码，转 `proto_error`；不终止循环。
- **handler panic**：`catch_unwind` 吸收 → 500 `internal error: handler panicked`，
  循环存活（handler 的 panic 消息仍会落到 stderr 诊断通道）。
- **`shutdown` 幂等**：收到第一条即回成功响应并返回退出路径（进程随后 exit(0)，
  与 TS“回成功后退出”的可观察行为一致）；重复 `serve` 调用同样返回成功 + `Graceful`。
- 版本检查对**所有**帧生效（request/response/event），id 原样回显；事件帧无 id 时
  错误响应 id 置 `null`。

## 验证

```sh
cargo fmt   --manifest-path native/Cargo.toml -p zcode-sidecar-proto
cargo clippy --manifest-path native/Cargo.toml -p zcode-sidecar-proto --all-targets -- -D warnings
cargo test   --manifest-path native/Cargo.toml -p zcode-sidecar-proto
```
