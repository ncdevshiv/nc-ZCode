# zcode-watch

文件监视 sidecar island（`lib + bin`），镜像
`packages/services/src/fileWatcher/fileWatcherService.ts` 的可观察契约，按
`specs/rust-native-crates.md` §Sidecar stdio protocol 暴露为 stdio NDJSON 服务。

## 方法与事件

| 方法       | 参数                                                    | 结果                          |
| ---------- | ------------------------------------------------------- | ----------------------------- |
| `watch`    | `{paths: string[], recursive?: bool, debounceMs?: u64}` | `{watchId}`                   |
| `unwatch`  | `{watchId}`                                             | `{}`（未知 id → 404）         |
| `list`     | —                                                       | `{ids: string[]}`（数值升序） |
| `shutdown` | —                                                       | `{}` + exit 0（内建）         |

事件：`fs_event {watchId, path, kind}`，`kind ∈ create | change | delete | rename | other`；
启动首帧 `ready {sidecar:"zcode-watch", proto:1}`。

## 线程与状态所有权

```text
notify 回调线程 ──WatchMsg(FIFO)──▶ emitter 线程 ──┐
serve(stdin) 循环（请求/响应） ────────────────────┼── Emitter = Arc<Mutex<Stdout>>
watch/unwatch handler 持有 RecommendedWatcher      │  （唯一写者模型，行级安全交错）
```

- `WatchState`（`BTreeMap<u64, WatchEntry>`）：serve 线程独占；emitter 只经通道被通知。
- `Debouncer`（每 watchId 一个）：emitter 线程独占；`now` 可注入，纯逻辑单测无真实时钟。
- `Add` 在 watcher 创建前入队（通道 FIFO），杜绝「事件先于窗口」竞态。

## 与 TS 源的差异（有意为之 / 已记录）

1. **事件粒度与字段**：TS 发目录级 `FileWatchEvent {dirPath, changedPath?}`（node
   `fs.watch` 只有 `rename|change` + filename，且 `changedPath` 仅在防抖窗口恰含
   一个路径时透传）；sidecar 按 spec 发 per-path `fs_event {watchId, path, kind}`，
   kind 由 notify 细分（create/delete/rename/change/other）——粒度更细是协议规定的形状。
2. **防抖语义镜像**：默认 150ms（= TS `DEBOUNCE_MS`，可用 `debounceMs` 覆盖 ——
   sidecar 扩展），每条原始事件重开窗口（trailing debounce，同 TS `clearTimeout` 重开）；
   窗口内**同路径合并为一条，kind 取首条**（first-wins：create→modify 报 create，
   客户端保守刷新即可）。TS 侧不区分 kind，此合并规则是 per-path 形状下的等价物。
3. **`unwatch` 未知 id**：TS `cleanup()` 对未知 id 静默成功；sidecar 按任务/spec 报
   404（显式分歧）。
4. **watcher error 清理**：TS `fsWatcher.on("error")` → 发最终 `{dirPath}` 事件 +
   从表中删除；sidecar → 为该 watchId 每个 root 发 `fs_event(kind:"other")` + 丢弃
   pending + 标记 dead（`list` 不再返回；watcher 实体保留到 `unwatch` —— unwatch
   dead id 仍成功，等价于 TS 对已 cleanup id 的 no-op 成功）。
5. **无忽略规则**：TS `fileWatcherService` 不过滤任何路径（Git 状态信号依赖 `.git`
   下的事件），sidecar 同样**没有 ignore set** —— 这是镜像，不是遗漏。
6. **`paths[]` 多根**：TS `watch()` 单 `path` 单 watcher；sidecar 一个 watchId 可挂
   多个根（spec 方法形状），语义 = 同参 TS 调用的并集。
7. **路径解析**：镜像 JS `path.resolve` 的词法语义（cwd 绝对化 + `.`/`..` 清理、
   过根保根），不访问文件系统；TS 直接把 `params.path` 交给 `fs.watch`。
8. **错误文案与码**：监视失败保留 TS 文案 `无法监视目录 '<path>': …`，映射 400
   （TS RPC 抛无码 `Error`；spec 的 400=协议/参数）。
9. **rename 事件**：node `fs.watch` 只报 `rename|change`；sidecar 借 notify 对
   改名的 from/to 各发一条 `kind:"rename"` —— 细分扩展，`rename` 覆盖两路径。

## 验证

```sh
cargo fmt   --manifest-path native/Cargo.toml -p zcode-watch
cargo clippy --manifest-path native/Cargo.toml -p zcode-watch --all-targets -- -D warnings
cargo test   --manifest-path native/Cargo.toml -p zcode-watch
```

e2e（`tests/e2e_stdio.rs`）经 `env!("CARGO_BIN_EXE_zcode-watch")` 拉起真实二进制，
事件断言上界 10s（spec S1）。
