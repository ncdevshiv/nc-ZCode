# zcode-scan

存储/工作区扫描 sidecar island（`lib + bin`），镜像
`packages/services/src/storage/adapters/fsWalker.ts` 的可观察契约，按
`specs/rust-native-crates.md` §Sidecar stdio protocol 暴露为 stdio NDJSON 服务。

## 方法与事件

| 方法       | 参数                                                     | 结果                         |
| ---------- | -------------------------------------------------------- | ---------------------------- |
| `scan`     | `{path, scanId, recursive?, maxDepth?, followSymlinks?}` | `{}`（进度走事件）           |
| `cancel`   | `{scanId}`                                               | `{}`（未知 id → no-op 成功） |
| `shutdown` | —                                                        | `{}` + exit 0（内建）        |

事件（`scanId` 关联）：

- `scan_entry {scanId, entry}` —— `entry = {relativePath, bytes, mtimeMs}`，逐文件推送；
- `scan_error {scanId, path, code}` —— `path` 为根相对路径，根级失败为 `""`，`code` 为
  Node errno 风格字符串（镜像 TS `errorCode()`，未知 → `UNKNOWN`）；
- `scan_done {scanId, directoriesScanned, filesScanned, missingRoot, cancelled}` ——
  恰好一条的终态事件；
- 启动首帧 `ready {sidecar:"zcode-scan", proto:1}`。

参数默认值全部对齐 TS 行为：`recursive = true`（TS 恒递归）、`maxDepth = 无上限`
（TS 无深度限制）、`followSymlinks = false`（TS `isSymbolicLink → skip`）。

## 线程与状态所有权

```text
扫描线程（每 scanId 一个）──scan_entry/scan_error/scan_done──┐
serve(stdin) 循环（scan/cancel/shutdown 请求与响应）──────────┼── Emitter = Arc<Mutex<Stdout>>
cancel/shutdown 经 ScanState.running 的取消位停止遍历        │  （唯一写者模型，行级安全交错）
```

- `ScanState.running`（`scanId → Arc<AtomicBool>`）：serve 线程登记占位，扫描线程
  完成时删除自己的条目；cancel/shutdown 只置位，遍历逐目录/条目检查（无事件竞态：
  取消信号挂在 Arc 上，先于线程启动登记也有效）。
- `ScanState.handles`：全部扫描线程句柄；**退出前统一 join**，保证在途扫描先发出
  `cancelled:true` 的 `scan_done` 再退出进程（spec 规则 5：终态不静默）。
- `scanId` 在 `scan_done` **之前**释放：客户端收到终态事件后复用 id 必然成功；
  线程存活判断不依赖 `running` 表，统一走 `handles` join。
- 重复 `scanId`（运行中）→ 409（spec 规则 5）；`scan` 参数问题 → 400。
- stdout 写失败（客户端断开）→ 置取消位尽快停扫，与 TS emit 中断同语义。

## 与 TS 源的差异（有意为之 / 已记录）

1. **条目字段以源码为准**：`scan_entry.entry` 镜像 TS `StorageScanEntry`
   `{relativePath, bytes, mtimeMs}`（`usageAggregate.ts` / `fsWalker.ts`）；任务
   提示中出现的 `path/name/size/dir` 形状在 `fsWalker.ts` 中不存在，不虚构。
2. **进度事件化**：TS 是进程内回调（`onEntry` / `onError` → `StorageScanProgress`
   节流上报）；sidecar 改为逐条 `scan_entry` / `scan_error` 事件 + 单条 `scan_done`
   终态，用 `scanId` 关联。TS 侧没有事件帧对照物，字段级镜像其数据结构。
3. **遍历顺序**：TS 默认 `concurrency = 4`，兄弟子树交错且顺序非确定；sidecar 串行
   LIFO，等价于 TS `concurrency = 1` 的可执行语义（spec 的 byte-equal 条款只约束
   TS 侧确定性的输出）。条目集合、计数与错误集合与并发度无关，保持一致。
4. **missingRoot**：镜像 TS「根不存在 = 空根非错误」——`scan_done.missingRoot =
true`，不产生 `scan_error`，请求仍成功。
5. **根为文件 / 根不可读**：镜像 TS `onError({path:"", code})` 容错语义——发
   `scan_error` 事件后空完成（Windows `read_dir(文件)` → 原始码 267 → `ENOTDIR`；
   POSIX → `ENOTDIR`），不是请求级错误。
6. **取消**：TS `AbortError` 使 runner promise reject（部分汇总随之丢失）；sidecar
   `cancel` 置位 → 尽快停 → `scan_done {cancelled:true, 部分计数}`（终态携带部分
   汇总是 sidecar 扩展）。未知 `scanId` 的 cancel 为 no-op 成功（同 TS「非当前 job
   为 no-op」）。
7. **scanId 冲突**：重复 id（运行中）→ 409；TS 侧无 id 冲突面（spec 规则 5）。
   `shutdown` / EOF 期间在途扫描同样先发终态再退出。
8. **mtimeMs 整数**：JS `stats.mtimeMs` 是带小数的 Number，亚毫秒在 Rust 侧不可观
   测，截断为整数毫秒；1970 年前时间戳归 0，保证非负 JSON 整数。
9. **sidecar 扩展参数**：`recursive` / `maxDepth` / `followSymlinks` 是 TS 没有的
   能力，默认值全部对齐 TS；`followSymlinks=true` 时目录按 canonical 去重防环。
   **无 ignore 集**：TS `fsWalker` 不忽略任何路径，sidecar 同样没有——这是镜像。
10. **让出事件循环**：TS `yieldEvery = 256` 每 256 条让出事件循环以免独占；Rust
    扫描在独立线程执行，天然不阻塞 serve 循环，语义等价、无需复刻该计数。

## 验证

```sh
cargo fmt   --manifest-path native/Cargo.toml -p zcode-scan
cargo clippy --manifest-path native/Cargo.toml -p zcode-scan --all-targets -- -D warnings
cargo test   --manifest-path native/Cargo.toml -p zcode-scan
```

单元测试（`src/lib.rs` 遍历算法 + `src/main.rs` 协议处理）覆盖：LIFO 顺序 oracle、
深度/递归开关、无 ignore 集、missingRoot、ENOTDIR、取消部分完成、软链跳过/跟随与
环终止、errno 映射、重复 scanId 409、参数 400、cancel no-op。e2e
（`tests/e2e_stdio.rs`）经 `env!("CARGO_BIN_EXE_zcode-scan")` 拉起真实二进制，
事件断言上界 10s（spec S1）。
