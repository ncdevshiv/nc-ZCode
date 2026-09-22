# zcode-image

S1 image island：解码 → 几何变换 → 编码热路径，替代 TS `apps/zcode-cli/packages/adapters/src/image/*`
的 jimp 调用（`resizeToFit` / `prepareForModel` 底层的位图操作）。NDJSON sidecar 协议复用
`zcode-sidecar-proto`。

## 方法

| method      | params                                                                            | result                                           |
| ----------- | --------------------------------------------------------------------------------- | ------------------------------------------------ |
| `info`      | `{data}`（base64）                                                                | `{width, height, format, mime}`                  |
| `transform` | `{data, ops:[{type,width,height}] \| op, output:"jpeg"\|"png"\|"webp", quality?}` | `{data(base64), width, height, mime, sizeBytes}` |

- `op.type`：`contain` / `scaleToFit`（同义，装入保持比例）、`cover`（覆盖后中心裁切）、
  `fill`（拉伸）。多个 op 顺序执行。
- `quality`：默认 `100`，钳制 `1..=100`，**仅对 jpeg 生效**（png/webp 忽略，镜像 TS）。
- `ops: []` 合法 = 不改尺寸仅重编码；`ops`/`op` 均缺失 → 400。
- 输入问题（空数据 / 非法 base64 / 解码失败 / 形状非法）一律 400；内部编码失败 500。

## 对齐级别：语义，不做字节级一致

- **几何公式镜像 jimp**：contain/cover 的贴合边选择、`round(src*f) || 1` 取整、
  cover 中心裁切 `round((scaled-target)/2)` —— 与 `@jimp/plugin-resize|plugin-cover` 一致，
  单测表格覆盖。
- **不承诺字节一致**：重采样核为 image crate 的 Catmull-Rom（jimp 用 BICUBIC），
  编码器为 image crate（jimp 用 @jimp/js-jpeg、@jimp/png、zune/webp 系），同输入输出字节不同。
- `transform` 执行原始 op 数学（可能放大）；TS `resizeToFit` 的“已在界内则原样返回”优化
  属调用方逻辑，不移植。

## 与 TS 的差异 / 已知边界

- **无 EXIF 自动旋转**：TS jimp 路径同样不旋转，行为一致。
- **png 输出固定 RGBA8 + CompressionType::Level(9)**（TS `deflateLevel: 9`；
  `deflateStrategy: 3` 无法映射到 PNG 行滤波器，改用自适应滤波）。
- **jpeg 丢 alpha**：与 jpeg-js 一致（只写 RGB，无 matte 合成）。
- **webp 输出仅无损**：image crate 的 WebP 编码器只支持 lossless（TS 侧 webp 是
  passthrough 不转码，本 sidecar 的 `output:"webp"` 是新增能力）。
- 16 位/浮点源降位到 8 位 RGBA（jimp bitmap 恒为 8 位 RGBA）。
- 解码格式受 Cargo 特性限制：`bmp / gif / jpeg / png / tiff / webp`；动画格式取首帧。
- `prepareForModel` 的 budget/cascade 压缩策略（多档质量+缩放找预算内候选）**未移植**：
  sidecar 契约没有 budget 参数，仍由 TS 调用方决定档位后逐次调用 `transform`。

## 验证

```sh
cargo test --manifest-path native/Cargo.toml -p zcode-image   # 含 tests/e2e_stdio.rs（真实二进制）
```
