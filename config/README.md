# 内置默认配置

> **No ZCode infrastructure (2026-11-05):** All ZCode services were removed per
> `specs/disabled-features.md:4-5`. This build makes no connections to ZCode infrastructure —
> no `GET /api/v1/client/configs` or `GET /api/v1/client/scenes` request is made. Remote
> config/scene reads answer locally (`{pluginStoreOrder: null}` per disabled-features §2.3,
> empty catalog per §2.4) or use the built-in `config/default.json` below.

`config/default.json` 是随客户端发布的默认配置，必须保留。Desktop 从打包文件读取，
Web 在构建时导入；远端请求失败或缺少有效字段时使用内置值。

## 帮助配置来源

Historically, community/feedback entries were requested via `GET /api/v1/client/configs`.
That remote call is now **disabled** (no ZCode infrastructure, see above); `data.configs.feedbackUrl` is not fetched.
读取 `data.configs.feedbackUrl`：

- `community_urls["zh-CN" | "en-US"]`：只按当前语言回退到内置入口，不跨语言回退。
- `feedback_url` / `feedback_use_external_form`：使用内置值（历史“远端优先”仅为旧版兼容，不再请求）。

（历史请求曾携带 `app_version` / `platform-arch` 并做 1 小时内存缓存；现已移除。）

```text
内置 default.json -> 平台入口（无远端请求）
```

default.json 为随客户端分发的内置默认配置，是当前唯一配置来源（无远端请求）；其他字段与既有消费者保持不变。

详细规则见 [用户社群入口配置](../docs/ui/settings-community-link-config.md)。
