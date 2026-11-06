# zcode-diff

把 `apps/zcode-cli`via TS`diff`npm package(`diff@9.0.0`, jsdiff) implemented`diff`The tool hot path is transplanted into the Rust sidecar crate, which provides unified diff, structured diff, row-level diff and patch application capabilities through the NDJSON stdio protocol to replace the jimp/TS hot path (see`specs/rust-native-crates.md`）。

## protocol

Ready 帧：`{"sidecar":"zcode-diff","proto":1}`。四个方法（参数校验失败返回 400，未知方法返回 404）：

| 方法              | 参数（`?` 可选）                                                                    | 结果                                                                                                                   |
| ----------------- | ----------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------- |
| `diff.unified`    | `old`、`new`、`oldLabel?`、`newLabel?`、`context?`(default 4),`timeoutMs?`(default 5000) |`{"patch": string \| null}`; timeout is`null`                                                                             |
| `structuredPatch`| Same`diff.unified`, plus`oldHeader?`、`newHeader?`| StructuredPatch object (`oldFileName`→`newFileName`→`oldHeader`→`newHeader`→`hunks`, undefined key omitted) or`null`（超时） |
| `diff.lines`      | `old`、`new`                                                                        | `{"chunks": [{count, added, removed, value}]}`                                                                         |
| `diff.applyPatch` | `source`、`hunks`(UnifiedPatch object or array thereof),`fuzzFactor?`(default 0) |`{"applied": true, "result": string}` or `{"applied": false}`                                                          |

`diff.unified`of`oldLabel`/`newLabel`only enter`---`/`+++`header, structured hunk does not carry header (with TS`diff.ts`Consistent: label is not passed to`structuredPatch`）。`diff.applyPatch`of`fuzzFactor`Only integer 0 is allowed (caller fixed`fuzzFactor:0`），非 0/非整数返回 400。

## parity level with jsdiff

- 逐字节 parity（golden fixture）：`diff.unified`(string),`structuredPatch`(Structured JSON, consistent key order),`diff.lines`(chunks array),`diff.applyPatch`(applied + result). fixture comes from real npm`diff@9.0.0`,lie in`tests/fixtures/jsdiff-parity.json`,Depend on`tests/jsdiff_parity.rs` 全量比对。
- 手工对齐的实现细节（源码级复刻 jsdiff `patch/create.js`、`patch/apply.js`、`diff/line.js`、`diff/base.js`、`util/distance-iterator.js`、`util/string.js`): when participle`\n`/`\r\n`The delimiter is merged into the previous token; sentinel lines (files without newlines at the end) use explicit blank lines;`oldLines==0`When hunk is written first`oldStart-1`;Index/67 line/quote rules;apply's`nConsecutive`Only reset, DistanceIterator if context row does not match or delete row matches`start=2,[0,4]` 序列、越界行不匹配等。

## List of differences from jsdiff

- 只实现调用方实际使用的组合：`fuzzFactor`Fixed 0 (fuzzy matching >0 is not implemented, non-0 is directly 400),`autoConvertLineEndings`Fixed false (CRLF source files are matched line by line as is, consistent with TS calls),`diff.lines` 不支持 timeout（调用方未传，视为无穷）。
- 超时判定为毫秒粒度（`as_millis() <= timeoutMs`); and when the input is identical (no changes required), the result is returned directly before the timeout check, which is the same as jsdiff`timeout` 快路径一致但可能比 JS 提前返回。
- `context`/`timeoutMs`/`fuzzFactor` 要求非负整数；jsdiff 接受任意 number（非整数会产出非整数 hunk 头）。400 报错文案为本 crate 文案，不复刻 jsdiff 文本。
- 结构化结果的 header 参数为 JSON `null`Press "Not Provided" when processing (result is that the key is omitted and the same as the TS transmission`undefined`'s JSON.stringify behaves the same); the jsdiff ontology will retain`null`。
- 字符串以 Rust `String`Bearer, lone surrogate in the input frame fails to be parsed at the protocol layer (`proto_error`), jsdiff can be held as is; within the patch`\ No newline` 标记行为一致。
- `diff.unified`/`structuredPatch`Timeout returns JSON`null`(Corresponding to the TS side`undefined` 经 JSON 序列化后的空值语义），消费方需按"No results"处理。
- 未知参数一律忽略（与 jsdiff 一致），不报错。

## fixture generation

```bash
# Requires diff@9.0.0 in the repository root node_modules
node native/crates/zcode-diff/tools/gen-jsdiff-fixtures.mjs
```

## verify

```bash
cargo fmt --manifest-path native/Cargo.toml -p zcode-diff
cargo clippy --manifest-path native/Cargo.toml -p zcode-diff --all-targets -- -D warnings
cargo test --manifest-path native/Cargo.toml -p zcode-diff
cargo check --manifest-path native/Cargo.toml --target x86_64-unknown-linux-musl -p zcode-diff
```
