// 违例的共同形状：模块规则（index.mjs）与语言边界棘轮（language-boundary.mjs）共用。
//
// 单独成文件是因为它**不属于任何一类规则**：放在 language-boundary.mjs 里会让通用
// 引擎 index.mjs 反向依赖具体规则模块（依赖方向错了），放在 index.mjs 里则让
// 棘轮反向依赖模块规则引擎。两者都把「中立件」绑到了某一侧。
import { createHash } from "node:crypto";

import { posix } from "./policy.mjs";

function fingerprint(rule, file, detail = "") {
  return createHash("sha256").update(`${rule}\0${file}\0${detail}`).digest("hex").slice(0, 16);
}

export function violation({ rule, file, detail, message, module, global = false }) {
  return {
    rule,
    file: posix(file),
    module: module?.id ?? null,
    detail,
    message,
    fingerprint: fingerprint(rule, posix(file), detail),
    global,
  };
}
