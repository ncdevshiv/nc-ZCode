# zcode-server — `packages/server` 的 axum 镜像（S5）

Spec：`specs/rust-native-crates.md`（crate map + S5 验收）、`specs/rust-pure-native-roadmap.md`
（S5 阶段定义）。当前 crate 版本 = 仓库根 `package.json` 的 `version`（测试做漂移守护）。
<!-- trace:2026-11-06 docs sync — replaces stale missing-/ws routing table. channel_router 8 live routes: 4 base + 4 /api/remote/*. -->

## 路由（`channel_router`，`src/lib.rs:274-293`）

| 路由                      | 行为                                                                                     |
| ------------------------- | ---------------------------------------------------------------------------------------- |
| `GET /healthz`            | `200 {"status":"ok"}`（聚合 `HostPort`；host 降级 → `503 {"status":"unavailable",...}`） |
| `GET /api/version`        | `200 {"version": <CARGO_PKG_VERSION>}`，与 `package.json` 一致                           |
| `GET /api/server-info`    | 编译期 server 元信息（`src/lib.rs:285`）                                                 |
| `GET /ws`                 | channel RPC WebSocket 升级（`ws_upgrade` → `nc-code-channels` registry，`lib.rs:286`）    |
| `POST /api/remote/connect` | SSH `-L` / `ws` 转发，registry 建真实 WS facade（`remote_api.rs:53`）                  |
| `POST /api/remote/bind`    | host 侧 bind（`remote_api.rs:54`）                                                       |
| `POST /api/remote/dispose` | 幂等 Dispose（`remote_api.rs:55`）                                                      |
| `GET /api/remote/sessions` | 窗口逻辑会话列表（`remote_api.rs:56`）                                                   |
| 未知 `/api/*`（任意方法） | `404 {"error":"not_found"}`，**绝不**落到 HTML fallback                                  |
| 非 `/api` 的 GET          | tower-http `ServeDir` 静态资源；未命中 → `ServeFile(index.html)` SPA 回退                |

（另有两条 catch-all ` /api` / `/api/{*key}` → JSON 404，`lib.rs:291-292`，不计入 8 条 live 路由。）

## HostPort（开放接线）

```rust
#[async_trait]
pub trait HostPort: Send + Sync {
    async fn ping(&self) -> HostPing;
}
```

router state 持有 `Arc<dyn HostPort>`；生产默认接 [`ProcessLivenessHost`]（进程存活 +
`detail` 显式声明后端未接线），[`InMemoryHost`] 仅供 `tests/` 使用——两者都不冒充真实后端。
完整后端由 `nc-code-shell/src/backend.rs:66`（`nc-code-app::build_with_ports` 装配真实
registry）+ `:93`（`nc_code_server::serve` 真实 serve）驱动，8 条路由全部 live。
`/api/rpc-host-capability` 等 Hono 既有端点如未镜像，以 `host_capability.rs` /
`info.rs` 的显式 named-error 为准，不冒充已接线。

## bin

```sh
cargo run --manifest-path native/Cargo.toml -p zcode-server -- \
  --host 127.0.0.1 --port 0 --static-dir <dir>
# stdout 第一行：LISTENING <addr>（e2e 解析用）；Ctrl-C 优雅退出
```

## 验证

```sh
cargo fmt   --manifest-path native/Cargo.toml -p zcode-server
cargo clippy --manifest-path native/Cargo.toml -p zcode-server --all-targets -- -D warnings
cargo test  --manifest-path native/Cargo.toml -p zcode-server
```

测试覆盖：`tests/router.rs`（tower `oneshot`：健康 JSON、版本 == package.json、SPA 回退、
静态资源、未知 /api JSON 404、非 GET 静态语义、降级 host 503）与
`tests/e2e_stdio_ish.rs`（拉起真实二进制 → 解析 `LISTENING` → 裸 TCP 真实 HTTP → kill）。

依赖变更需按 spec 重新生成 cargo 三方许可
（`node scripts/third-party-cargo.mjs --write && node scripts/licenses.mjs notices`）。
