#!/usr/bin/env node
// Backend replacement completion access control - acceptance criteria of specs/rust-backend-replacement.md §2.
//
// The source of truth is native/fixtures/service-channels.json (inventory-service-channels.mjs from true
// TS interface machine extraction), not any manual schedule: this tool is found in the source code of each Rust service crate
// Neither "forward distribution of evidence" nor "explicit removal of evidence" can be guessed based on grammatical shapes, so evidence is obtained according to the following rules:
// Forward: `"name" =>` (match arm) or `== "name"` (event name equality judgment).
// Remove: string literals in the removed/unsupported/unimplemented/unrouted area
// (`matches!(…)` blocks, `is_removed_*` functions, `V4_METHODS_UNIMPLEMENTED`, `UNROUTED_COMMANDS`).
// Each removed member must be able to find a reason to reference spec in BOUNDARY, otherwise the access control will fail——
// "Remove" is a product decision and cannot be used to cover up missing ports. The same name has both positive and removal evidence and the same error is reported.
//
// Usage: node native/tools/check-channel-coverage.mjs [--json] [--allow-partial]

import { existsSync, readFileSync, readdirSync, statSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import { ADDITIONAL_CHANNELS, CHANNEL_BOUNDARY, MEMBER_BOUNDARY } from "./coverage-boundaries.mjs";
import { collectV4MethodBoundary, runV4MethodSelfTest } from "./coverage-v4-methods.mjs";

const here = dirname(fileURLToPath(import.meta.url));
const nativeRoot = resolve(here, "..");
const repoRoot = resolve(nativeRoot, "..");
const cratesRoot = join(nativeRoot, "crates");

const inventory = JSON.parse(
  readFileSync(join(nativeRoot, "fixtures", "service-channels.json"), "utf8"),
);

// Only distributions in service crates (and composition roots) count as channel member evidence: zcode-proc / zcode-scan
// These internal registries also use `registry.register("list"...)`. If scanned together, the internal RPC name will be mistaken for
// channel or member (e.g. `list` of skills).
// The Rust port renamed the crates to `nc-code-*`; keep the historical `zcode-*` prefix accepted
// so this gate works during the migration window instead of scanning nothing.
const SERVICE_CRATE = /^(?:nc-code|zcode)-(services-|app$|channels$)/;
//Characteristics of an explicit "non-portable" list: the item name contains these words.
const REMOVAL_ITEM = /removed|unrouted|unimplemented|unsupported|not_wired|not_supported/i;

function walk(dir) {
  const out = [];
  for (const entry of readdirSync(dir)) {
    const full = join(dir, entry);
    const st = statSync(full);
    if (st.isDirectory()) out.push(...walk(full));
    else if (full.endsWith(".rs")) out.push(full);
  }
  return out;
}

function sourceFiles() {
  const files = [];
  for (const crate of readdirSync(cratesRoot)) {
    if (!SERVICE_CRATE.test(crate)) continue;
    const srcDir = join(cratesRoot, crate, "src");
    try {
      files.push(
        ...walk(srcDir).filter(
          (file) => !/[\\/](?:tests|target)[\\/]/.test(file) && !file.endsWith("/main.rs"),
        ),
      );
    } catch {
      // crate has no src (rare), skipped.
    }
  }
  return files;
}

// Skip the entire module starting from `#[cfg(test)]` to avoid using fake channel/member in single test as evidence.
function stripTestModules(text) {
  let cut = text;
  for (;;) {
    const at = cut.indexOf("#[cfg(test)]");
    if (at === -1) break;
    const open = cut.indexOf("{", at);
    if (open === -1) {
      cut = cut.slice(0, at);
      break;
    }
    let depth = 0;
    let i = open;
    for (; i < cut.length; i += 1) {
      if (cut[i] === "{") depth += 1;
      else if (cut[i] === "}") {
        depth -= 1;
        if (depth === 0) break;
      }
    }
    cut = cut.slice(0, at) + cut.slice(i + 1);
  }
  return cut;
}

// Parse the evidence range channel by channel without using the global name set: member names are highly duplicated (`list`, `get`, `create`),
// Scoring all channels as long as there is `"list" =>` in any crate will hide the gap.
// Parsing chain: `registry.register("x", self.field.clone())` → in the same file `field: Arc<Type>` →
// The file where the full position `impl Channel for Type` is located = the distribution range of the channel.
function collectEvidence() {
  const files = sourceFiles();
  const texts = new Map(files.map((file) => [file, stripTestModules(readFileSync(file, "utf8"))]));
  const channelScope = new Map();
  const allNames = new Map();
  const crateOf = new Map();
  for (const [file, text] of texts) {
    const dispatched = new Set();
    const removed = new Set();
    const items = [...text.matchAll(/(?:fn|const)\s+([A-Za-z0-9_]+)/g)];
    const itemAt = (index) => {
      let name = "";
      for (const item of items) {
        if (item.index > index) break;
        name = item[1];
      }
      return name;
    };
    for (const match of text.matchAll(
      /"([A-Za-z0-9_]+)"\s*=>|"([A-Za-z0-9_]+)"\s*\||\|\s*"([A-Za-z0-9 _]+)"|[=!]=\s*"([A-Za-z0-9_]+)"|\.insert\(\s*"([A-Za-z0-9_]+)"/g,
    )) {
      const name = match[1] ?? match[2] ?? match[3] ?? match[4] ?? match[5];
      const owner = itemAt(match.index);
      if (owner && REMOVAL_ITEM.test(owner)) removed.add(name);
      else dispatched.add(name);
    }
    // `matches!(command, "a" | "b" | ...)`: One item per member. The **last item** is not visible in the regular regex.
    // (it is followed by `)` instead of `|`), the last member in the list will be misjudged as "not connected" (ENTRY 104
    // Change `matches!` to match form for this purpose). Here is the whole analysis: only accept "literal + |"
    // Select a list, do not relax to any string position, so `unwrap_or("list")` will not be recorded as distributed.
    for (const block of text.matchAll(
      /matches!\s*\(\s*[A-Za-z_][A-Za-z0-9_.]*\s*,\s*((?:"[A-Za-z0-9_]+"\s*\|?\s*)+)\)/g,
    )) {
      const owner = itemAt(block.index);
      for (const entry of block[1].matchAll(/"([A-Za-z0-9_]+)"/g)) {
        const name = entry[1];
        if (owner && REMOVAL_ITEM.test(owner)) removed.add(name);
        else dispatched.add(name);
      }
    }
    for (const name of dispatched) if (removed.has(name)) dispatched.delete(name);
    allNames.set(file, { dispatched, removed });
    crateOf.set(file, file.split(/[\\/]/).at(-2));
  }
  const registered = new Map();
  const removedGlobal = new Set();
  for (const [file, text] of texts) {
    for (const match of text.matchAll(/registry\.register\(\s*"([^"]+)"\s*,\s*([^;\n]+)/g)) {
      const channel = match[1];
      const expr = match[2];
      if (!registered.has(channel)) registered.set(channel, new Set());
      registered.get(channel).add(crateOf.get(file));
      const fieldName = /self\.([a-z_0-9]+)/.exec(expr)?.[1];
      // Only recognize the line of the structure field (anchored at the beginning of the line), otherwise lines containing colons such as `use oauth::{…}` will be matched first.
      const fieldLine = fieldName
        ? (new RegExp(`^\\s*(?:pub\\s+)?${fieldName}\\s*:\\s*.*$`, "m").exec(text)?.[0] ?? "")
        : "";
      const localType = /:\s*(?:Arc\s*<\s*)?((?:[a-z_][a-z_0-9]*::)*[A-Z][A-Za-z0-9_]*)/.exec(
        fieldLine,
      )?.[1];
      const typeShort = localType?.split("::").at(-1);
      const implFiles = typeShort
        ? [...texts.entries()]
            .filter(
              ([path, body]) =>
                path !== file &&
                new RegExp(`impl\\s+(?:[\\w:]+::)?Channel\\s+for\\s+${typeShort}\\b`).test(body),
            )
            .map(([path]) => path)
        : [];
      if (!channelScope.has(channel)) channelScope.set(channel, new Set());
      const scope = channelScope.get(channel);
      scope.add(file);
      for (const implFile of implFiles) scope.add(implFile);
      // When the impl file cannot be parsed, return the entire crate: rather count distributions with the same name in the same crate (rare),
      // Nor can we report wired members as notches because the tool cannot parse the shape - that would push the work in the wrong direction.
      if (implFiles.length === 0) scope.add(`${crateOf.get(file)}__WHOLE_CRATE__`);
    }
    for (const name of allNames.get(file).removed) removedGlobal.add(name);
  }
  const dispatchedFor = (channel) => {
    const scope = channelScope.get(channel);
    if (!scope || scope.size === 0) return new Set();
    const out = new Set();
    for (const [file, entry] of allNames) {
      const whole = scope.has(`${crateOf.get(file)}__WHOLE_CRATE__`);
      if (!scope.has(file) && !whole) continue;
      for (const name of entry.dispatched) if (!removedGlobal.has(name)) out.add(name);
    }
    return out;
  };
  return { registered, channelScope, dispatchedFor, removedGlobal };
}

function boundaryFor(channel, member) {
  const channelReason = CHANNEL_BOUNDARY[channel];
  if (channelReason) return channelReason;
  for (const [pattern, reason] of MEMBER_BOUNDARY[channel] ?? []) {
    if (pattern === "*") return reason;
    if (pattern.endsWith("*") && member.startsWith(pattern.slice(0, -1))) return reason;
    if (pattern === member) return reason;
  }
  return null;
}

const { registered, channelScope, dispatchedFor, removedGlobal } = collectEvidence();
const errors = [];
const rows = [];
let memberTargets = 0;
let memberPorted = 0;
let memberBoundary = 0;

for (const name of registered.keys()) {
  if (
    !inventory.services.some((service) => service.channelName === name) &&
    !ADDITIONAL_CHANNELS[name]
  ) {
    errors.push(`Rust registered a channel that is not in the TS list and has no source: ${name}`);
  }
}

for (const service of inventory.services) {
  const channel = service.channelName;
  const members = service.methods.map((method) => method.name);
  const isRegistered = registered.has(channel);
  const channelBoundary = CHANNEL_BOUNDARY[channel] ?? null;
  const dispatched = dispatchedFor(channel);
  if (isRegistered && !channelBoundary && dispatched.size === 0) {
    errors.push(
      `${channel}: registered but the tool cannot see any evidence of dispatch in its distribution scope - either it is really not wired, or the scope cannot be resolved, neither of which is allowed to silently count 0`,
    );
  }
  const missing = [];
  const undocumentedRemovals = [];
  let ported = 0;
  for (const member of members) {
    const reason = boundaryFor(channel, member);
    const isRemovedEvidence = removedGlobal.has(member) && !dispatched.has(member);
    if (isRemovedEvidence && !reason) {
      undocumentedRemovals.push(member);
      continue;
    }
    if (reason) {
      memberBoundary += 1;
      continue;
    }
    memberTargets += 1;
    if (dispatched.has(member)) {
      ported += 1;
      memberPorted += 1;
    } else {
      missing.push(member);
    }
  }
  if (undocumentedRemovals.length > 0) {
    errors.push(
      `${channel}: These members were explicitly removed in the code, but the bounds manifest did not reference spec: ${undocumentedRemovals.join(", ")}`,
    );
  }
  rows.push({
    channel,
    registered: isRegistered,
    boundary: channelBoundary,
    memberCount: members.length,
    ported,
    missing,
    crates: [...(registered.get(channel) ?? [])],
  });
}

// Claimed to have been removed, but no evidence of removal found in the code = false boundary, error reported.
for (const [channel, patterns] of Object.entries(MEMBER_BOUNDARY)) {
  const service = inventory.services.find((entry) => entry.channelName === channel);
  if (!service) continue;
  for (const [pattern] of patterns) {
    if (pattern === "*") continue;
    const exists = service.methods.some(
      (method) =>
        method.name === pattern ||
        (pattern.endsWith("*") && method.name.startsWith(pattern.slice(0, -1))),
    );
    if (!exists)
      errors.push(
        `${channel}: ${pattern} in the boundary list does not exist in the TS member table`,
      );
  }
}

if (process.argv.includes("--self-test")) {
  const [failed] = runV4MethodSelfTest();
  process.exit(failed === 0 ? 0 : 1);
}

const v4Boundary = collectV4MethodBoundary({
  rpcSource: readFileSync(resolve(cratesRoot, "nc-code-host/src/rpc.rs"), "utf8"),
  factSource: readFileSync(
    resolve(repoRoot, "packages/shared/src/zcode-protocol-v4/transport.ts"),
    "utf8",
  ),
  citationExists: (citation) =>
    existsSync(
      citation === "README.md"
        ? resolve(cratesRoot, "nc-code-host/README.md")
        : resolve(repoRoot, citation),
    ),
});
const v4BoundaryEntries = v4Boundary.entries;
const v4Methods = v4Boundary.methods;
errors.push(...v4Boundary.errors);
const portTargets = inventory.services.filter((service) => !CHANNEL_BOUNDARY[service.channelName]);
const registeredPortTargets = rows.filter(
  (row) => row.registered && !CHANNEL_BOUNDARY[row.channel],
).length;

const summary = {
  factSource: "native/fixtures/service-channels.json",
  channels: {
    total: inventory.services.length,
    boundaryRemoved: Object.keys(CHANNEL_BOUNDARY).length,
    expectedToRegister: portTargets.length,
    registered: registeredPortTargets,
  },
  members: {
    total: inventory.services.reduce((sum, service) => sum + service.methods.length, 0),
    boundaryRemoved: memberBoundary,
    expectedToPort: memberTargets,
    ported: memberPorted,
    percent: memberTargets ? Number(((memberPorted / memberTargets) * 100).toFixed(1)) : 0,
  },
  additionalChannels: ADDITIONAL_CHANNELS,
  gaps: rows
    .filter((row) => !CHANNEL_BOUNDARY[row.channel])
    .map((row) => ({
      channel: row.channel,
      registered: row.registered,
      missingCount: row.missing.length,
      missing: row.missing,
    }))
    .filter((row) => !row.registered || row.missingCount > 0),
  v4Methods: { boundary: v4BoundaryEntries.length, factSource: v4Methods.size },
  errors,
};

if (process.argv.includes("--json")) {
  console.log(JSON.stringify(summary, null, 2));
} else {
  console.log(
    `[coverage] channels ${summary.channels.registered}/${summary.channels.expectedToRegister} registered` +
      `(Another ${summary.channels.boundaryRemoved} are removed entirely by spec)`,
  );
  console.log(
    `[coverage] members ${summary.members.ported}/${summary.members.expectedToPort} has distribution evidence` +
      `(${summary.members.percent}%); list ${summary.members.total}, boundary removed ${summary.members.boundaryRemoved}`,
  );
  console.log(
    `[coverage] The unimplemented boundaries of v4 method ${summary.v4Methods.boundary} all reference existing governance documents;` +
      `Fact source V4_METHODS ${summary.v4Methods.factSource} methods`,
  );
  for (const gap of summary.gaps) {
    console.log(
      gap.registered
        ? ` - ${gap.channel}: registered, missing ${gap.missingCount} → ${gap.missing.join(", ")}`
        : ` - ${gap.channel}: Not registered`,
    );
  }
  for (const error of summary.errors) console.log(`[coverage] ERROR ${error}`);
}

const incomplete = summary.gaps.length > 0 || summary.errors.length > 0;
if (incomplete && !process.argv.includes("--allow-partial")) {
  console.log("[coverage] Not completed: Still channel/member gaps or inconsistent boundaries");
  process.exitCode = 1;
} else if (incomplete) {
  console.log("[coverage] not completed (--allow-partial: report only)");
} else {
  console.log(
    "[coverage] All transplantable channels have been registered, members have distribution point evidence, and all boundaries have spec sources",
  );
}
