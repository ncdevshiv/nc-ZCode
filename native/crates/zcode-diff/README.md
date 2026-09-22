# zcode-diff

把 `apps/zcode-cli` 中经由 TS `diff` npm 包（`diff@9.0.0`，jsdiff）实现的 `diff` 工具热路径移植为 Rust sidecar crate，通过 NDJSON stdio 协议对外提供统一 diff、结构化 diff、行级 diff 与补丁应用能力，用于替代 jimp/TS 热路径（见 `specs/rust-native-crates.md`）。

## 协议

Ready 帧：`{"sidecar":"zcode-diff","proto":1}`。四个方法（参数校验失败返回 400，未知方法返回 404）：

| 方法              | 参数（`?` 可选）                                                                    | 结果                                                                                                                   |
| ----------------- | ----------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------- |
| `diff.unified`    | `old`、`new`、`oldLabel?`、`newLabel?`、`context?`(默认 4)、`timeoutMs?`(默认 5000) | `{"patch": string \| null}`；超时为 `null`                                                                             |
| `structuredPatch` | 同 `diff.unified`，另加 `oldHeader?`、`newHeader?`                                  | StructuredPatch 对象（`oldFileName`→`newFileName`→`oldHeader`→`newHeader`→`hunks`，undefined 键省略）或 `null`（超时） |
| `diff.lines`      | `old`、`new`                                                                        | `{"chunks": [{count, added, removed, value}]}`                                                                         |
| `diff.applyPatch` | `source`、`hunks`（UnifiedPatch 对象或其数组）、`fuzzFactor?`(默认 0)               | `{"applied": true, "result": string}` 或 `{"applied": false}`                                                          |

`diff.unified` 的 `oldLabel`/`newLabel` 只进入 `---`/`+++` 头，结构化 hunk 不携带 header（与 TS `diff.ts` 一致：label 不传给 `structuredPatch`）。`diff.applyPatch` 的 `fuzzFactor` 仅允许整数 0（调用方固定 `fuzzFactor:0`），非 0/非整数返回 400。

## 与 jsdiff 的 parity 级别

- 逐字节 parity（golden fixture）：`diff.unified`（字符串）、`structuredPatch`（结构化 JSON，键序一致）、`diff.lines`（chunks 数组）、`diff.applyPatch`（applied + result）。fixture 来自真实 npm `diff@9.0.0`，位于 `tests/fixtures/jsdiff-parity.json`，由 `tests/jsdiff_parity.rs` 全量比对。
- 手工对齐的实现细节（源码级复刻 jsdiff `patch/create.js`、`patch/apply.js`、`diff/line.js`、`diff/base.js`、`util/distance-iterator.js`、`util/string.js`）：分词时把 `\n`/`\r\n` 分隔符并入前一 token；sentinel 行（文件无换行结尾）用显式空行；`oldLines==0` 时 hunk 头写 `oldStart-1`；Index/67 线/引号规则；apply 的 `nConsecutive` 只在上下文行不匹配或删除行匹配时重置、DistanceIterator `start=2,[0,4]` 序列、越界行不匹配等。

## 与 jsdiff 的差异清单

- 只实现调用方实际使用的组合：`fuzzFactor` 固定 0（>0 的模糊匹配未实现，非 0 直接 400）、`autoConvertLineEndings` 固定 false（CRLF 源文件按原样逐行匹配，与 TS 调用一致）、`diff.lines` 不支持 timeout（调用方未传，视为无穷）。
- 超时判定为毫秒粒度（`as_millis() <= timeoutMs`）；且输入恒等（无需变更）时在超时检查前直接返回结果，与 jsdiff `timeout` 快路径一致但可能比 JS 提前返回。
- `context`/`timeoutMs`/`fuzzFactor` 要求非负整数；jsdiff 接受任意 number（非整数会产出非整数 hunk 头）。400 报错文案为本 crate 文案，不复刻 jsdiff 文本。
- 结构化结果的 header 参数为 JSON `null` 时按"未提供"处理（结果省略该键，与 TS 传 `undefined` 的 JSON.stringify 行为一致）；jsdiff 本体会保留 `null`。
- 字符串以 Rust `String` 承载，输入帧中的孤立代理项（lone surrogate）在协议层解析失败（`proto_error`），jsdiff 可原样持有；补丁内 `\ No newline` 标记行为一致。
- `diff.unified`/`structuredPatch` 超时返回 JSON `null`（对应 TS 侧的 `undefined` 经 JSON 序列化后的空值语义），消费方需按"无结果"处理。
- 未知参数一律忽略（与 jsdiff 一致），不报错。

## fixture 生成

```bash
# 需要仓库根 node_modules 中的 diff@9.0.0
node native/crates/zcode-diff/tools/gen-jsdiff-fixtures.mjs
```

## 验证

```bash
cargo fmt --manifest-path native/Cargo.toml -p zcode-diff
cargo clippy --manifest-path native/Cargo.toml -p zcode-diff --all-targets -- -D warnings
cargo test --manifest-path native/Cargo.toml -p zcode-diff
cargo check --manifest-path native/Cargo.toml --target x86_64-unknown-linux-musl -p zcode-diff
```
