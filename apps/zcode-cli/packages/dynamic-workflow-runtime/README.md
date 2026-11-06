# @zcode/dynamic-workflow-runtime

沙箱 harness（dynamic workflow 执行引擎）。把一份 workflow 脚本在受控子进程里跑起来，
用 NDJSON 把子进程的 `__host.*`call bridged to`@zcode/dynamic-workflow` 的纯引擎核心。

## Dependency boundaries

**Only** depends on `@zcode/dynamic-workflow` (workspace) and node builtins. **Never** import `@zcode/core` /
`@zcode/contracts` / `@zcode/bootstrap` / `@zcode/adapters`——本包是「整条 sandbox↔engine
管线 app-free 可跑」的证明。

## Usage

```ts
import { runWorkflowScript } from "@zcode/dynamic-workflow-runtime";

const settlement = await runWorkflowScript({
  scriptText,                 // 或 lowered: <async 函数体>
  caps: { maxConcurrency: 16 },
  askSpecs,                   // site id ∈ 合成 schemas 记录即 typed
  validate,                   // @zcode/dynamic-workflow 的 validate（适配到 ValidateFn）
  makeDriver: (sink) => driver, // driver 自带 journal + emit；sink 是引擎的向上回报面
  signal,                     // 可选：AbortSignal
  timeoutMs,                  // 可选：墙钟超时
});
// settlement: { status: "completed", artifact } | { status: "failed", error } | { status: "cancelled" }
```

## Architecture

```
┌─ parent (harness) ──────────────┐  NDJSON  ┌─ child (vm.createContext) ──────┐
│ runWorkflowScript               │  stdio   │ 只含 ES intrinsics + __host       │
│  - lower(scriptText)            │◀────────▶│  createActor 同步返回 local 句柄  │
│  - WorkflowEngine(driver,...)   │          │  ask/worldRead → 请求父进程       │
│  - 桥接 __host.* ↔ engine       │          │  args 冻结全局（spawn 时过界一次）  │
│  - spawn/kill/timeout/abort     │          │  Date.now/Math.random 运行期禁令  │
└─────────────────────────────────┘          └──────────────────────────────────┘
```

## NDJSON wire protocol

见 `src/protocol.ts`(The One True Source). child→parent:`create-actor`(Send it and forget it)/`request`（ask、
world-read）/ `event`（log）/ `complete`；parent→child：`response`。

## Build order

测试与 typecheck 通过 `@zcode/dynamic-workflow`The **has built dist** resolution dependency, so`pretest` /
`pretypecheck`Will first`bun run --filter @zcode/dynamic-workflow build`. Brand new check out directly`bun test` 即可，
不会踩到 stale-dist。

## Failure Judgment and Choices

- run 的裁决归引擎所有。终结失败（脚本抛错 / 子进程崩溃 / 超时 / 协议损坏）都调
  `engine.fail(error)`——Settlement`failed`, driver side cancel on the fly ask, journal note`dwf_run.status =
  "failed"` + `failure_json`, journal is consistent with the result seen by the caller. abort signal is unique"真取消"，
  调 `engine.cancel()`(Settlement`cancelled`，可 resume）。harness 侧的 first-wins finalize 只管
  子进程清理（清 timer、关 stdin、kill child），不自造结算。
