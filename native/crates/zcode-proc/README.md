# zcode-proc

S1 process-tree island：进程树枚举/强杀 + Job 作用域 spawn，替代 TS
`taskkill /T /F` 与抓取式进程列表（`apps/zcode-cli/packages/adapters/src/mcp/{process-tree,windows-job-object}.ts`、
`packages/services/src/process/*`）。NDJSON sidecar 协议复用 `zcode-sidecar-proto`。

## 方法

| method         | params                     | result                                                                  |
| -------------- | -------------------------- | ----------------------------------------------------------------------- |
| `list_tree`    | `{pid}`                    | `{processes:[{pid,ppid,name}]}`（根优先 BFS 闭包，含根；根缺失 → `[]`） |
| `kill_tree`    | `{pid, timeoutMs?=2000}`   | `{killed:[pid...], failed:[{pid,error}]}`                               |
| `spawn_in_job` | `{cmd, args?, cwd?, env?}` | `{pid, jobKey}`（`job-{seq}-{pid}`）                                    |
| `kill_job`     | `{jobKey}`                 | `{jobKey, terminated:bool}`                                             |

## 行为契约（镜像 TS 可观察语义）

- 根 PID 已死/不存在：`kill_tree` 与 `list_tree` 均为无操作成功（空结果，不报错）。
- 杀树顺序：深度降序（叶子优先）、同深度 pid 升序；25ms 轮询，超时后对幸存者
  force 一次并确认 500ms；`failed` 只收结束时仍存活的幸存者。
- `spawn_in_job` 入 Job 失败（如拒绝访问）→ **409**，消息带 `pid` 与
  `fallback: kill_tree {"pid":…}`；此时子进程保持运行，由调用方决定回退，不隐式杀。
- `kill_job`：首次 true；重复/未知 key → `terminated:false`（幂等）。

## 与 TS 的差异 / 已知边界

- **Windows**：Job Object（`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`）。sidecar 退出
  （正常返回或进程被杀）关闭句柄 → 残留子树被终止。
- **unix（编译级验证，运行时未测）**：`spawn_in_job` = `setsid`（Linux 加
  `PR_SET_PDEATHSIG=SIGKILL`），`kill_job` = `killpg(组长)`；sidecar 死亡时由
  PDEATHSIG 兜底终止子进程。非 Linux unix 无 PDEATHSIG，仅靠父亡时 init 收养。
- Linux 进程表走 `/proc`；非 Linux unix 降级 `ps -axo pid=,ppid=,comm=`（可能丢字段）。
- Windows 存活判定 `OpenProcess+GetExitCodeProcess`：打不开且错误码≠87 按存活保守处理
  （宁可进 `failed` 不误报 `killed`）。
- `kill_tree` 在首轮逐 pid 杀完后，若根是进程组组长则补一次 `killpg`（Windows 无此步）。
- spawn 的子进程由 sidecar 内部 reaper 线程回收（unix 防僵尸）；输出继承自 sidecar 的
  stdio 语义与 TS `child_process.spawn` 一致地不可回读。

## 验证

```sh
cargo test --manifest-path native/Cargo.toml -p zcode-proc   # 含 tests/e2e_stdio.rs（真实二进制）
cargo check --manifest-path native/Cargo.toml -p zcode-proc --target x86_64-unknown-linux-musl
```
