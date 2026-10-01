# Spec: TypeScript 只存在于前端（language boundary ratchet）

> 状态：2026-09-28 生效。
> 日期口径：`.architecture-ts-baseline.json` 的 `generatedAt` / `bootstrappedAt` 与
> 逃生舱的 `expires` 比较**一律用 UTC**（`toISOString()`）；本文的散文日期是本地日期。
> 两者可能差一天，读基线时按 UTC 理解。
> **强制范围**：「机器强制」目前 = `pnpm architecture:check` +
> `pnpm verify:pre-push`。本仓库**没有 CI workflow 跑架构门禁**（`.github/` 里没有
> 这一步），所以绕过 `verify:pre-push` 的提交不会被本门禁拦住。这是已知的执行缺口，
> 不代表没有门禁。
> 本 spec 是 `architecture-policy.yaml` 中 `languageBoundary` 段的**规范来源**；
> 两者冲突时以本 spec 为准，policy 是它的可执行投影。
> 执行入口：`pnpm architecture:check`（即 `scripts/architecture/architecture-check.mjs`）。

## 0. 不变量

**TypeScript 只存在于前端。**

仓库中任何**新增**或**扩大**的 TypeScript，必须落在 §2 的允许清单（allowlist）内。
前端之外的一切 TypeScript 都是**迁移债**（debt）。

本规范**不要求删除任何现有 TypeScript**。它是一条**棘轮（ratchet）**：
基线冻结今天的债务，此后只允许债务**变小**。这是一条单调收紧的规则，
永远不能因为它而要求删代码，也永远不能让当前构建失败。

## 1. 判定范围

### 1.1 语言

- 判定扩展名：**`.ts` / `.tsx` / `.mts` / `.cts`**（policy 中 `languageBoundary.extensions`）。
- **`.js` / `.jsx` / `.mjs` / `.cjs` 不在本版判定范围内**，见 §8.1（记录在案的
  待裁决事项，带日期，不是静默遗漏）。理由：用户原话是 "ts"；且仓库里
  `eslint.config.mjs` / `vitest.config.mts` / `tailwind.config.js` /
  `postcss.config.mjs` 等构建配置大量是 JS/MJS，按"是否随产物发布"重新划线
  本身就是一次独立的范围决策（§8.1），不该混进本轮。

### 1.2 文件集合

判定对象 = **`git ls-files --cached --others --exclude-standard`** 枚举到的、
且当前存在于工作区的 TypeScript 文件（仓库根相对路径）。

- 用 git 而不是文件系统遍历：`.gitignore` 已经表达了"这不是仓库资产"的权威判断，
  `node_modules/` / `dist/` / `target/` / `.turbo/` / `old.back/`（若被忽略）等
  自动排除，不需要在 policy 里再抄一份排除名单（避免硬编码漂移）。
- git 枚举失败时 **ratchet 必须报错退出**，不允许静默降级为"本次跳过"。
  静默跳过 = 一个现成的绕过口子。
- 未被跟踪的新文件（`--others`）也在判定范围内：新建一个非前端 `.ts` 立刻违例。

### 1.3 不在判定范围内

- `node_modules/`、`dist/`、`target/`、`.turbo/` 等被忽略的产物与依赖。
- `native/**` 今天的**全部内容**：那里没有 TS。将来若出现，按本 spec 判定为债
  （例外不存在，`native/**` 不是 allowlist 条目）。
- 未跟踪且被 `.gitignore` 忽略的文件（它们不进版本库，因此不是仓库债务）。

## 2. 允许清单

### 2.1 条目（policy 中 `languageBoundary.allowlist[]`）

| id                   | 形式     | 覆盖                                                                                           | 理由                                    |
| -------------------- | -------- | ---------------------------------------------------------------------------------------------- | --------------------------------------- |
| `frontend-webview`   | roots    | `packages/ui`、`packages/web`、`packages/desktop/src/renderer`、`packages/desktop/src/preload` | webview 前端，唯一真正"被渲染"的 TS     |
| `dev-tooling`        | roots    | `.agents/skills`、`scripts`、`harness`、`packages/desktop/scripts`                             | 构建/校验/打包/技能资产，不进产物运行时 |
| `dev-tooling-entry`  | files    | `packages/server/build-remote.ts`                                                              | 打包脚本入口，属 dev-tooling            |
| `dev-tooling-config` | patterns | `**/*.config.ts`、`**/*.d.ts`、`**/*.d.mts`                                                    | 构建配置与类型声明                      |

`roots` 是目录前缀（`p` 命中 `p/**`）；`files` 是精确路径；`patterns` 是 glob，
支持 `**`（跨任意层目录，可匹配零层）与 `*`（单层内任意字符），其余按字面量。

### 2.2 优先级：最具体者胜，allowlist 永远压过逃生舱

对每个路径，收集所有命中的条目，按**字面前缀长度**（`roots` 取目录串长度，
`patterns` 取第一个通配符之前的部分）排序，最长者胜出。

1. 命中 allowlist → **合法**。此时即使同时命中了某条已过期的逃生舱，
   路径仍然合法，但那条逃生舱仍然按 §5 报 `expired-exception`（过期必须可见，
   不能被更宽的 allowlist 吞掉）。
2. 未命中 allowlist、命中未过期逃生舱 → **合法**。
3. 未命中 allowlist、也未命中有效逃生舱 → **迁移债**，进入 §3 判定。
4. **同优先级冲突**：两条 allowlist/逃生舱条目字面前缀长度相同且同时命中，
   且其中有已过期条目 → 报 `language-boundary-ambiguous`（策略歧义，
   属于配置缺陷，必须改 policy，不允许门禁自己猜）。

逃生舱**永远不能**把一个路径从"债务"变成"合法"以外的任何东西，也不能
**反向**把一个 allowlist 路径标成债务。它只能加宽"合法"，不能收窄。
这是刻意的：门禁只应变得更严，不会因为某个逃生舱过期而要求删代码。

## 3. 违例定义

对每个判定对象（TS 文件），在 §2 分类完成后：

| 情形                                                     | 判定                                                                                       |
| -------------------------------------------------------- | ------------------------------------------------------------------------------------------ |
| 落在 allowlist                                           | 合法，不进基线                                                                             |
| 落在有效逃生舱                                           | 合法，不进基线                                                                             |
| 其余，且**不在基线里**                                   | 违例 `ts-frontend-only`（新增）                                                            |
| 其余，且基线行数 < 当前行数                              | 违例 `ts-frontend-only`（扩大）                                                            |
| 其余，且基线行数 >= 当前行数                             | 合法（缩小或持平）                                                                         |
| 在基线里但**工作区**已无此文件                           | 合法（真的删了即减债；基线里的陈旧条目由生成器清理）                                       |
| 在基线里，文件**还在磁盘上但离开了 git 索引**            | 算减债。**这不是删除**——文件仍被构建、仍占代码量，见 §8.9                                  |
| 从一个非 allowlist 路径**移动**到另一个非 allowlist 路径 | 新路径不在基线 → 违例（新增）。旧路径消失 → 减债。移动到 allowlist 路径 → 合法（转入前端） |

**行数口径**：`source.split(/\r?\n/).length`，与本仓库架构门禁既有的
`max-file-lines` 规则（`scripts/architecture/index.mjs`）完全一致。
选择行数而不是 AST 度量：2400+ 个文件需要在 CI 里稳定复现，行数是确定性的、
与格式化工具无关的、无需解析依赖的度量；AST 度量会被 prettier/oxfmt 重排影响，
会产生大量与"是否扩大了非前端 TS"无关的噪声。

**`dev-tooling` 停车位规则**（`dev-tooling-import`）：任何**非 allowlist** 的
TS 文件都不得 `import` / `require` `dev-tooling` 允许清单下的文件。
**范围限定（本轮不做别名解析）**：只覆盖**相对说明符**（`./`、`../`）。经 tsconfig
path alias / package exports 指到 `scripts/` 的依赖**检测不到**。实测当前没有任何
`tsconfig*.json` 把路径映射进 `scripts/`，因此今天的爆炸半径是 0——这是现状，不是保证。
否则就是把后端依赖藏进构建脚本目录 —— 允许清单会退化成停车场。
违例必须给出**导入点的 `file:line`** 与被导入文件。

这条规则**也是棘轮**，判定单位是「边」而不是「文件」：

| 情形                               | 判定                               |
| ---------------------------------- | ---------------------------------- |
| 边 `<importer>\|<target>` 在基线里 | 继承债，合法                       |
| 同一条边出现次数 > 基线条数        | 违例（逐条指向超出部分的具体行号） |
| 新增的边（不在基线里）             | 违例                               |
| 边消失                             | 减债                               |

边的 key 用 `importer\|target` 而不是行号：行号会随编辑漂移，key 不会。
「只减不增」（§4）同样覆盖这条轴：行数与边数任一增长都需要 `--accept-growth`，
并把增长轴（`axes`）写进 `acceptedGrowth`。

### 3.4.1 继承债：今天已存在的 5 条边

本规则落地时实测命中 **5 条**（不是 0 条；早期探针只解析 `.ts` 目标，漏掉了
`.mjs`，属于探针缺陷，已修正为解析全部源码扩展名）。这 5 条连同今天的 2,282
个债文件一起进基线，**不在本轮修**：

| 运行时侧（迁移债）                                        | 开发工具侧                                                                 |
| --------------------------------------------------------- | -------------------------------------------------------------------------- |
| `packages/desktop/src/main/chromeCredentialManager.ts:4`  | `packages/desktop/scripts/bundle.mjs:15`                                   |
| `packages/desktop/src/main/desktopRuntimeEnv.ts:28`       | `packages/desktop/scripts/ensure-local-runtime-assets.mjs:12`              |
| `packages/desktop/src/main/index.ts:132`                  | `packages/desktop/scripts/prepare-runtime-assets.mjs:8`                    |
| `packages/desktop/src/main/windowsAumidIcon.ts:4`         | `packages/desktop/electron-builder.config.js:18,22`                        |
| `packages/desktop/src/main/windowsChromeAppBoundKey.ts:8` | `packages/desktop/tsup.config.ts:7` · `packages/desktop/vite.config.ts:10` |

全部指向 `packages/desktop/scripts/{powershell-command,target-platform,desktop-product-identity}.mjs`。

**根因**：这三个文件是**被误放在构建脚本目录的共用工具**，两侧都在 import
（Electron main 运行时 + 打包/构建配置），所以「运行时依赖开发工具」在这里
并不是真的依赖方向错了，而是**归类错了**。

**真实修法（见 §8.5，本轮明确不做）**：把这三个模块连同
`powershell-command.d.mts` / `target-platform.d.mts` 移到中立共享模块，
让工具侧从产品代码 import。本轮不做是因为 `electron-builder.config.js` /
`tsup.config.ts` / `vite.config.ts` 都直接引用这些文件，移动会扰动桌面打包路径，
属于未请求的爆炸半径，且该区域本身是 Rust 迁移目标（desktop main 47,468 loc）。

**每条违例都必须能回答"怎么修"**，报告里带修复路径：

- 新增：① 不要加这个文件；② 确实是前端 → 落到 allowlist 内；③ 确实需要非前端
  TS（例如与冻结 fixture 对比的测试）→ 走 §5 逃生舱，留下可复核的记录。
- 扩大：把新增/增长的那部分拆出去（拆成 allowlist 内的文件，或改成 Rust），
  直到该文件行数回到基线。**禁止**为了压行数把后端逻辑搬进 `packages/ui` ——
  那是把债换个目录存放，已有 `ui-implementation-import` 规则会拦跨层实现导入，
  但搬进去的代码本身仍是债。
- `dev-tooling-import`：把该依赖反转（dev-tooling 依赖业务，而不是反过来），
  或删掉这条跨方向的边。

## 4. Ratchet 基线

**`.architecture-ts-baseline.json` 必须与门禁代码在同一个 commit 里落地。**
它是承重件：缺它时 `checkLanguageBoundary` 直接抛错（`language-boundary.mjs` 的
ENOENT 分支），门禁不是「变松」而是「整个跑不起来」。它也**不能进 `.gitignore`**——
棘轮要对比的正是入库的那份基线。已核实当前未被 gitignore。

- 文件：`.architecture-ts-baseline.json`（与 `.architecture-baseline.json` 同族，
  独立存放，**不会被 `architecture:baseline:update` 覆盖** —— 模块规则的基线
  更新命令绝不能顺手把新的语言债洗白，这是两条基线物理分开的原因）。
- 内容：机器生成、可 review、可再生成。

  ```jsonc
  {
    "version": 1,
    "rule": "ts-frontend-only",
    "generatedAt": "2026-09-28",
    "generator": "pnpm architecture:ts-baseline:update",
    "totals": { "files": 2282, "lines": 510755 },
    "debt": { "apps/zcode-cli/…/foo.ts": 123 }, // 路径 -> 生成当天的行数
    "acceptedGrowth": [], // 每次被批准的扩大事件，追加不覆盖
  }
  ```

- 语义：**基线 = 生成当天的债务快照**。之后任何路径只要行数超过自己那条基线，
  就是违例；删除或缩小自动减债。
- 再生成：`pnpm architecture:ts-baseline:update`。它打印
  新增 / 删除 / 扩大 / 缩小 的分类计数与总行数差。
- **默认只允许收缩**。若新基线总行数 > 旧基线总行数，命令**拒绝写入**，
  必须显式 `--accept-growth --reason "<为什么>" --approved-by "<谁>"`。
  批准后该事件被**追加**到 `acceptedGrowth`（时间、理由、批准人、行数差），
  不覆盖历史。基线是入库文件，所以每次扩大都会出现在 code review 的 diff 里，
  试图洗白债务必须先说服 reviewer。
- **授权人**：只有仓库 owner 能批准基线扩大（`--approved-by` 记录其身份）。
  门禁本身不判断"这次扩大是否合理"，它只保证**扩大是显式的、有日期的、
  可归因的、并且出现在 diff 里**。

## 5. 逃生舱（escape hatch）

- 形式：policy 中 `languageBoundary.escapeHatches[]`，每条含
  `id` / `paths`（files 或 patterns，同 §2.1 语法）/ `reason`（为什么合法）/
  `approvedBy`（授权人）/ `expires`（到期日）。
- 授权人：仓库 owner（与 §4 一致）。逃生舱是写进 policy 的代码，走 code review。
- 到期：`expires` 早于当天 → 复用门禁既有的 `expired-exception` 规则报违例。
  **不设到期日上限**：`expires: 2999-01-01` / `9999-12-31` 都会被接受。设上限需要一次
  产品决定（多长算合理），本轮不做；因此**代码 review 是唯一控制**——一条
  远期逃生舱不会自己提醒你。见 §8.7。
  到期本身是**强制**的：过期的舱会被报 `expired-exception`，并且**失去豁免**（该路径
  回到迁移债判定）。被强制的只是"到期"这个动作，**不是到期日的长度**——不设上限，
  也不设宽限。
- **活逃生舱把路径整段移出棘轮**（本节是规范性条款，不是待裁决项）：命中**活**逃生舱的
  路径**不进 `debt`、不受基线比较、可以无限增长，并且被排除在上报的债务总量之外**。
  这不是 bug，是本节语义的直接后果，但后果的严重程度必须写下来——审计实测：一条
  `expires: 9999-12-31` 的逃生舱盖住 `packages/services/src/index.ts`，把它从 302 行
  涨到 9,000 行，门禁仍打印 `OK debt=2281 files / 510453 lines` 并 **exit 0**，
  而真实树里有 519,453 行。**全仓情形更极端**：一条 `paths: ["**/*.ts"]` 的舱
  **加上**一次再生成 ⇒ `debt=0 files / 0 lines`、基线归零、门禁此后常绿。
  处置：**只让放行量可见**——`ts-boundary` 同一行并排打印
  `hatched=<files> files / <lines> lines`，`report --markdown` 同样并排，
  `ts-baseline:update` 的输出也并排打印放行量（否则 Δ 为负会被读成减债）。
  **不堵增长通道**：「活逃生舱是否仍受棘轮约束」以及「逃生舱的**宽度**是否该设上限」
  都是设计决策，由 owner 另行裁定；到期日长度的问题见 §8.7。
- 今天本 spec 落地时**不带任何逃生舱**（实测 0 例需要）。需要的人自己加一条，
  写清楚"为什么这个非前端 TS 合法"。

## 6. 本 guard 明确不做的事

1. **不删代码。** 违例只让 `architecture:check` 失败并说明修法。
2. **不判断 Rust 移植是否完成。** "这个模块能不能删"由
   `old.back/README.md` 的四条件与 `RUST-PORT-MATRIX.md` §4 决定，
   与本 guard 无关。RUST-PORT-MATRIX 记录的 5 个 1:1 候选全部在条件 3/4 失败，
   **本 guard 不改变这个结论，也不允许任何人借"门禁过了"来移动它们**。
3. **不替代 typecheck / lint / knip / cargo test。** 它是语言边界这一条轴。
4. **不判断代码质量。** 0/1 债务里的一行和一千行同样合法，区别只在"是否超过基线"。
5. **不判定被 gitignore 的文件。** 见 §1.2。
6. **不阻止在前端内继续加 TS。** 前端 TS 的规模治理是另一条轴（未做，见 §8.3）。

## 7. 与既有 spec / 现状的关系

- `specs/rust-pure-native-roadmap.md` S6：「TS remains only where it is rendered
  (UI source) or where tests still compare against frozen fixtures」。
  本 spec 是它的**可执行子集**：
  - "rendered" → §2.1 `frontend-webview`；
  - "tests still compare against frozen fixtures" → **不预置 allowlist**。
    今天仓库里符合这个描述的非前端 TS 测试文件只有 8 个
    （`apps/zcode-cli/packages/sidecar/test/*.test.ts` 等），全部留在基线里。
    **新增**这样的测试文件必须走 §5 逃生舱 —— "still"指的是存量，
    新增必须是一次显式的、被记录的决策。这么定是为了让 guard 只收紧。
- `RUST-PORT-MATRIX.md` §4 依赖顺序（host default flip → JS 工具删除 →
  首个 `old.back` 移动 → workflow 引擎 → Electron→Rust shell）
  完全不受本 guard 影响：guard 不删文件、不翻转任何默认值。
- `old.back/**` **不在** allowlist 内（用户 Decision 2 的清单是穷举的，
  且该目录今天为空）。后果与处置见 §8.2 —— 需要用户裁决。
- `apps/zcode-cli/packages/tui` **不在** allowlist 内（用户未列入；
  `specs/rust-pure-native-roadmap.md` S5 已决定 TUI 迁 `ratatui`）。
  它的 91 个文件因此全部计入基线并会持续被棘轮盯着。

## 8. 记录在案的待裁决事项（不是静默遗漏）

### 8.1 JS / MJS 是否纳入（2026-09-28 提出）

产物运行时路径里的 `.js/.mjs`（如 `apps/zcode-cli` 的 SEA 脚本链）同样是 Node
遗留，S5/S6 会删掉它们，但它们**不在本版 ratchet 的语言范围**。
**待用户裁决**：是否在下一版把 `.js/.mjs` 纳入（需要先决定"随产物发布"
的判定方式）。owner：`main` / 用户。

### 8.2 `old.back/**` 与棘轮的交互（2026-09-28 提出）

四条件达成、真正执行第一次 `git mv` 到 `old.back/` 时，
新路径不在基线 → 棘轮报违例。两个候选处置：
(a) 每次合法移动附一条 `old.back/**` 的到期逃生舱（记录这次移动）；
(b) 把 `old.back/**` 加进 allowlist，并用 knip/零引用检查单独治理。
两者都需要用户拍板，本轮不猜。

### 8.3 前端 TS 自身的规模治理（2026-09-28 提出）

`packages/ui` 已有 1,480 个 TS 文件 / 324,803 行（§3 行数口径实测）。允许清单**不设上限**，
因此前端可以无限增长。本轮明确不做规模限制（那需要另一个有依据的阈值，
不是本轮能拍脑袋定的数）。**待裁决**：是否需要为前端 TS 也立一条基线。

### 8.4 `.oxlintrc.json` 忽略 `apps/zcode-cli`（2026-09-28 提出）

CLI 的 TS 现在**完全不过 lint**。把这 1,369 个 **TypeScript 文件**纳入 lint 会立刻
产生大量新 warning，属于独立的变更，本轮不动。**待裁决**：单独排一次 lint 收敛。

> 口径：1,369 是 **TS 家族**文件数，可复现命令为
> `git ls-files --cached --others --exclude-standard -- apps/zcode-cli | grep -cE '\.(ts|tsx|mts|cts)$'`。
> 该目录下的**全部**文件是 1,510（多出的是 `.json` 47 / `.md` 26 / `.mjs` 25 /
> `.gz` 14 / `.map` 8 / `.zip` 4 等非 TS 资产）。审计曾按全量口径把这一条判为
> 「1,369 应为 1,510」并要求改数——**该判定本身是口径错配**：本条讲的是 TS 不过 lint，
> 用全量文件数替换会把非 TS 资产也算进来。数字保留 1,369，但「TS 文件」这个限定词
> 必须留在句子里，否则读者会拿它当目录全量。

### 8.5 `packages/desktop/scripts` 里三个共用工具归类错误（2026-09-28 提出）

`powershell-command.mjs` / `target-platform.mjs` / `desktop-product-identity.mjs`
是运行时与构建配置共用的工具，却被放在构建脚本目录，造成 §3.4.1 的 5 条继承边。
修法：连同两个 `.d.mts` 一起移到中立共享模块，工具侧改为从产品代码 import。
**本轮不做**（会扰动 `electron-builder.config.js` / `tsup` / `vite` 打包路径）。
迁移债减少后，棘轮会盯住「不许再加新的这条方向的边」。owner：`main`。

### 8.6 `**/*.config.ts` / `**/*.d.ts` 是全仓无上限的口子（2026-09-28 提出）

实测：把 40 行后端声明放进 `packages/services/src/zz.d.ts`、或把 40 行后端代码
放进 `apps/zcode-cli/packages/adapters/src/zz.config.ts`，`architecture:check`
**都 exit 0**——因为这两个 pattern 是**全仓无上限**的。当前爆炸半径有限
（全仓 30 个 config 文件 / 1,815 行），且 `dev-tooling-import` 会拦住后端代码去
**import** 它们；但没有任何东西限制它们的**体量**，也没有限制它们出现在哪个目录。

两个候选收紧方式（**本轮都不做**，需要用户裁决）：

1. 把 pattern 收窄到顶层与约定配置目录（例如各包的 `vite.config.ts` /
   `tsup.config.ts` 逐个列举，或限制到深度 1-2）；
2. 给 allowlist 内的 config/声明文件设**行数上限**并进棘轮。

为什么不自己选：(1) 会让新增包的配置文件变成一次策略修改（摩擦大但可发现）；
(2) 引入一个人为阈值，而阈值需要依据。两者的取舍不同，owner：`main` / 用户。

### 8.7 逃生舱到期日没有上限（2026-09-28 提出）

实测：`expires: 2999-01-01` 与 `9999-12-31` 都被接受，代码里没有任何 horizon 检查。
后果：一条实质上「永不到期」的逃生舱在门禁上看起来仍然合规，而唯一能发现它的
场合是人眼 review。**到期日的长度与逃生舱的宽度是两个独立问题**：本条只谈时间
（horizon），宽度（`paths` 能盖多大）见 §5「活逃生舱把路径整段移出棘轮」——
审计实测一条 `paths: ["**/*.ts"]` 的舱能让整个棘轮归零，`expires` 设成 1 天也一样糟。
两个候选（**本轮都不做**，horizon 本身需要产品决定）：

1. 设一个最大 horizon（例如 180 天），到期必须续期；
2. 保留无上限，但把远期逃生舱单列成一份需要 owner 签字的报告。

owner：`main` / 用户。

### 8.8 `yaml` 是未登记依赖（既有基线，本轮不修）

`scripts/architecture/policy.mjs:4` 直接 `import { parse as parseYaml } from "yaml"`，
但根 `package.json` 的 `devDependencies` 里没有 `yaml`，它只是靠 pnpm 的提升
（hoisting）才解析得到。`pnpm knip` 把它报在 _Unlisted dependencies_ 下（全仓 8 条，
全部是既有基线）。

**这不是本次变更引入的**：main 已用 `git show HEAD:scripts/architecture/policy.mjs`
核实该 import 在 HEAD 上就存在。本轮只改了这个文件里与语言边界相关的部分。

正确修法：把 `yaml` 按工作区其它包已在用的版本加进根 `package.json` 的
`devDependencies`，并更新 lockfile。**本轮不做**，两个理由：

1. 改根 manifest + lockfile 是本次变更范围之外的爆炸半径；
2. 无法在离线环境里安全验证 lockfile 的完整性——写坏一半的 lockfile 比留一个
   诚实的缺口更贵。

与语言边界无关：本条不改变任何 `languageBoundary` 行为，也不影响棘轮数字。

顺带澄清，避免以后被误读成回归：`pnpm knip` **可以正常运行**（exit 1 是"有发现"，
不是"跑不起来"），当前实测 unused files 29 / unused exports 361 / unused dependencies 54 /
unlisted dependencies 8。其中 unlisted 段有一条指向 `scripts/architecture/policy.mjs:4:36`
的 `yaml`——**该 import 在 HEAD 上就存在**（`git show HEAD:scripts/architecture/policy.mjs`），
不是本轮引入；而 29 个 unused files 里**本轮自己的文件贡献 0 条**。

> **更正记录（本条曾被写错）**：本 spec 早前版本称 `pnpm knip` "当前 exit 2、
> 根因既有、因此本轮不修"。那是**错的**——它并非既有仓库缺陷，而是审计自己在
> round 1 的一次误操作删掉了 `packages/*/node_modules`（pnpm 工作区软链）造成的
> 临时环境损坏。环境修复后 knip 完全可复现上述数字。**教训**：「既有缺陷 / 不是回归」
> 这类判断必须附带能区分二者的状态检查（本例即"先确认 node_modules 完整再下结论"），
> "我没改过那些文件"并不能推出"根因是既有的"。下面第 2 点的**推迟理由与工具无关**，
> 仍然成立：真正的原因是爆炸半径与离线 lockfile 风险，不是 knip 坏了。

### 8.9 `.gitignore` 能藏债，而且棘轮把隐藏算成进步（2026-09-28 提出）

实测两类绕过：

1. 一个**未跟踪**的债 `.ts` + 一行 `.gitignore` → 门禁 `OK`，文件完全不可见；
2. 对**已跟踪**的债文件 `git rm --cached` + 一行 `.gitignore` → 门禁报出一个 302 行的
   **减债**，而那 302 行**仍在磁盘上、仍被构建**。

所以 §3 表格里「文件已不存在」指的是**工作区**没有这个文件，**不是**「它离开了 git
索引」。**索引移除不是删除。**

本 guard **不检查、也不打算检查 `.gitignore`**——它管语言边界；把「有没有被藏起来」
也纳入会让它变成内容安全门禁，那是另一个职责、另一套威胁模型。可选加固（本轮**不做**）：
对「离开索引但仍在磁盘上」的基线条目发一条 `ts-debt-untracked` 诊断。
owner：`main` / 用户。

### 8.10 `.gitattributes` 只对部分扩展名钉 LF（2026-09-28 提出）

`.gitattributes` 只覆盖 `*.mjs|*.sh|*.ts|*.tsx|*.json|*.md`。在一台
`core.autocrlf=true` 的 Windows 主机上，新 checkout 的 `*.js` / `*.html` / `*.css` /
`*.yml` 会被转成 CRLF，于是 `pnpm fmt:check` **在零内容变化的情况下失败**。

对棘轮无影响：CRLF 与 LF 的行数完全相同。

一行修法（本轮**不应用**）：`git config core.autocrlf false`，或在 `.gitattributes`
加一条兜底 `* text=auto eol=lf`。owner：仓库维护者。

## 9. 当前数字（2026-09-28，生成器实测）

| 桶                    |    文件数 |        行数 |
| --------------------- | --------: | ----------: |
| frontend（allowlist） |     1,518 |     330,326 |
| dev-tooling           |        86 |       7,655 |
| dev-tooling config    |        30 |       1,815 |
| dev-tooling entry     |         1 |          59 |
| **迁移债（基线）**    | **2,282** | **510,755** |
| 合计                  |     3,917 |     850,610 |

行数口径见 §3（`split(/\r?\n/).length`，含文件尾换行带来的 +1/文件）。
与 `RUST-PORT-MATRIX.md` 的 487,949 loc 口径不同（它按模块枚举口径统计），
两者指向同一结论：非前端 TS 是 Rust 的 17 倍以上。
