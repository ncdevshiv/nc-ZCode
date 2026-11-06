// Inventory of every RPC service channel exposed today by the TypeScript backend.
//
// WHY: the Rust backend must expose the *same* channels with the *same* method names,
// because the current frontend calls them through @zcode/client's ProxyChannel
// (`channel.call("<method>", arg)`). This script asks the TypeScript checker for the
// authoritative surface (descriptors + interface methods, inherited members included)
// and writes it to native/fixtures/service-channels.json. The Rust service crates use
// that file as the porting checklist and completeness test.
//
//   node native/tools/inventory-service-channels.mjs
//
// Requires: repo node_modules (typescript).

import { execFileSync } from "node:child_process";
import { createRequire } from "node:module";
import { existsSync, readdirSync, readFileSync, writeFileSync } from "node:fs";
import { join, relative } from "node:path";
import { fileURLToPath } from "node:url";

const repoRoot = fileURLToPath(new URL("../../", import.meta.url));
const require = createRequire(import.meta.url);

// This checkout has a TypeScript install whose lib/*.d.ts files are unreadable
// (EPERM), which makes any type-checker query degrade to `{}`. Prefer the first
// installation whose default lib can actually be read.
function loadTypeScript() {
  const candidates = [
    join(repoRoot, "node_modules", "typescript", "lib", "typescript.js"),
    join(
      repoRoot,
      "apps",
      "zcode-cli",
      "node_modules",
      ".ignored",
      "typescript",
      "lib",
      "typescript.js",
    ),
  ];
  for (const candidate of candidates) {
    if (!existsSync(candidate)) continue;
    const typescript = require(candidate);
    const lib = typescript.getDefaultLibFilePath({ target: typescript.ScriptTarget.ES2024 });
    try {
      if (readFileSync(lib, "utf8").length > 0) return typescript;
    } catch {
      // unreadable install: try the next candidate
    }
  }
  throw new Error("no readable TypeScript installation found");
}

const ts = loadTypeScript();

// pnpm workspace links in this checkout can point at a moved repository. Resolve
// @zcode/* packages from source directly so the inventory never depends on the
// state of node_modules symlinks.
const compilerPaths = {};
for (const entry of readdirSync(join(repoRoot, "packages"), { withFileTypes: true })) {
  if (!entry.isDirectory()) continue;
  const packageDir = join(repoRoot, "packages", entry.name);
  const manifestPath = join(packageDir, "package.json");
  if (!existsSync(manifestPath)) continue;
  const manifest = JSON.parse(readFileSync(manifestPath, "utf8"));
  if (typeof manifest.name !== "string") continue;
  const rel = relative(repoRoot, packageDir).replaceAll("\\", "/");
  if (existsSync(join(packageDir, "src", "index.ts"))) {
    compilerPaths[manifest.name] = [`${rel}/src/index.ts`];
    compilerPaths[`${manifest.name}/*`] = [`${rel}/src/*`];
  } else if (existsSync(join(packageDir, "index.d.ts"))) {
    compilerPaths[manifest.name] = [`${rel}/index.d.ts`];
    compilerPaths[`${manifest.name}/*`] = [`${rel}/*`];
  }
}

const roots = ["packages/services/src", "packages/shared/src", "packages/rpc/src"];
const rootNames = execFileSync(
  "git",
  ["ls-files", "--cached", "--others", "--exclude-standard", ...roots],
  { cwd: repoRoot, encoding: "utf8" },
)
  .split(/\r?\n/)
  .filter((name) => name.endsWith(".ts") && !name.endsWith(".d.ts"))
  .map((name) => join(repoRoot, name));

const program = ts.createProgram(rootNames, {
  target: ts.ScriptTarget.ES2024,
  lib: ["es2024", "dom", "dom.iterable"],
  module: ts.ModuleKind.ESNext,
  moduleResolution: ts.ModuleResolutionKind.Bundler,
  allowImportingTsExtensions: true,
  baseUrl: repoRoot,
  paths: compilerPaths,
  skipLibCheck: true,
  noEmit: true,
  strict: false,
  jsx: ts.JsxEmit.Preserve,
});
const checker = program.getTypeChecker();

function readServiceChannels() {
  const channelsPath = join(repoRoot, "packages", "shared", "src", "channels.ts");
  const source = program.getSourceFile(channelsPath);
  if (!source) throw new Error("packages/shared/src/channels.ts not in program");
  let result;
  const objectLiteralOf = (expression) => {
    let current = expression;
    while (
      ts.isAsExpression(current) ||
      ts.isSatisfiesExpression(current) ||
      ts.isParenthesizedExpression(current) ||
      ts.isTypeAssertionExpression(current)
    ) {
      current = current.expression;
    }
    return ts.isObjectLiteralExpression(current) ? current : undefined;
  };
  const visit = (node) => {
    if (ts.isVariableStatement(node)) {
      for (const declaration of node.declarationList.declarations) {
        if (
          ts.isIdentifier(declaration.name) &&
          declaration.name.text === "ServiceChannels" &&
          declaration.initializer
        ) {
          const literal = objectLiteralOf(declaration.initializer);
          if (!literal) continue;
          result = {};
          for (const property of literal.properties) {
            if (ts.isPropertyAssignment(property) && ts.isStringLiteral(property.initializer)) {
              result[property.name.getText(source)] = property.initializer.text;
            }
          }
        }
      }
    }
    ts.forEachChild(node, visit);
  };
  visit(source);
  if (!result) throw new Error("ServiceChannels object literal not found");
  return result;
}

const declaredChannels = readServiceChannels();

function typeTextOf(symbol, location) {
  return checker.typeToString(
    checker.getTypeOfSymbolAtLocation(symbol, location),
    undefined,
    ts.TypeFormatFlags.NoTruncation,
  );
}

function optionalFlag(symbol) {
  return (symbol.flags & ts.SymbolFlags.Optional) !== 0;
}

function memberRole(name) {
  if (name.length >= 10 && name.startsWith("onDynamic") && /^[A-Z]$/.test(name[9])) {
    return "dynamic-event";
  }
  if (name.length >= 3 && name.startsWith("on") && /^[A-Z]$/.test(name[2])) {
    return "event";
  }
  return "rpc";
}

function methodOf(symbol, location) {
  const anchor = symbol.valueDeclaration ?? symbol.declarations?.[0] ?? location;
  const type = checker.getTypeOfSymbolAtLocation(symbol, anchor);
  const signatures = type.getCallSignatures();
  const role = memberRole(symbol.getName());
  if (signatures.length === 0) {
    return {
      kind: "property",
      role,
      name: symbol.getName(),
      optional: optionalFlag(symbol),
      type: typeTextOf(symbol, anchor),
    };
  }
  const signature = signatures[0];
  const returnType = checker.typeToString(
    signature.getReturnType(),
    undefined,
    ts.TypeFormatFlags.NoTruncation,
  );
  return {
    kind: "method",
    role,
    name: symbol.getName(),
    optional: optionalFlag(symbol),
    typeParameters: (signature.typeParameters ?? []).map((parameter) =>
      checker.symbolToString(parameter),
    ),
    parameters: signature.parameters.map((parameter) => ({
      name: parameter.getName(),
      optional: optionalFlag(parameter),
      type: typeTextOf(parameter, parameter.valueDeclaration ?? anchor),
    })),
    returnType,
  };
}

function interfaceSource(interfaceType) {
  const declaration = interfaceType.getSymbol()?.declarations?.[0];
  if (!declaration) return undefined;
  return relative(repoRoot, declaration.getSourceFile().fileName).replaceAll("\\", "/");
}

const services = [];
const problems = [];

const visitFile = (sourceFile) => {
  const relativePath = relative(repoRoot, sourceFile.fileName).replaceAll("\\", "/");
  if (!relativePath.startsWith("packages/services/src/")) return;
  const visit = (node) => {
    if (
      ts.isVariableDeclaration(node) &&
      node.initializer &&
      ts.isCallExpression(node.initializer)
    ) {
      const callee = node.initializer.expression;
      const calleeText = ts.isIdentifier(callee)
        ? callee.text
        : ts.isPropertyAccessExpression(callee)
          ? callee.name.text
          : "";
      if (calleeText === "createServiceDescriptor") {
        const [typeArgument] = node.initializer.typeArguments ?? [];
        if (!typeArgument) {
          problems.push(
            `${relative(repoRoot, sourceFile.fileName)}: descriptor without type argument`,
          );
          ts.forEachChild(node, visit);
          return;
        }
        const descriptorName = node.name.getText(sourceFile);
        const interfaceType = checker.getTypeAtLocation(typeArgument);
        const channelArgument = node.initializer.arguments[0];
        let channelName;
        if (channelArgument && ts.isStringLiteral(channelArgument)) {
          channelName = channelArgument.text;
        } else if (channelArgument && ts.isPropertyAccessExpression(channelArgument)) {
          const key = channelArgument.name.text;
          channelName = declaredChannels[key];
          if (!channelName) {
            problems.push(`${descriptorName}: ServiceChannels.${key} is not declared`);
          }
        } else {
          problems.push(`${descriptorName}: channelName is not a literal/property access`);
        }
        const methods = checker
          .getPropertiesOfType(interfaceType)
          .map((property) => methodOf(property, typeArgument))
          .sort((left, right) => left.name.localeCompare(right.name));
        services.push({
          descriptor: descriptorName,
          channelName: channelName ?? null,
          interfaceName: interfaceType.getSymbol()?.getName() ?? descriptorName,
          source:
            interfaceSource(interfaceType) ??
            relative(repoRoot, sourceFile.fileName).replaceAll("\\", "/"),
          methods,
        });
      }
    }
    ts.forEachChild(node, visit);
  };
  visit(sourceFile);
};

for (const sourceFile of program.getSourceFiles()) visitFile(sourceFile);
services.sort((left, right) => (left.channelName ?? "").localeCompare(right.channelName ?? ""));

const descriptorChannels = new Set(services.map((service) => service.channelName));
const declaredValues = new Set(Object.values(declaredChannels));
for (const channel of declaredValues) {
  if (!descriptorChannels.has(channel)) {
    problems.push(`declared channel "${channel}" has no service descriptor`);
  }
}
for (const channel of descriptorChannels) {
  if (channel && !declaredValues.has(channel)) {
    problems.push(`descriptor channel "${channel}" is missing from ServiceChannels`);
  }
}

const totals = {
  channels: services.length,
  members: services.reduce((sum, service) => sum + service.methods.length, 0),
  rpcMethods: services.reduce(
    (sum, service) => sum + service.methods.filter((method) => method.role === "rpc").length,
    0,
  ),
  events: services.reduce(
    (sum, service) => sum + service.methods.filter((method) => method.role === "event").length,
    0,
  ),
  dynamicEvents: services.reduce(
    (sum, service) =>
      sum + service.methods.filter((method) => method.role === "dynamic-event").length,
    0,
  ),
  properties: services.reduce(
    (sum, service) => sum + service.methods.filter((method) => method.kind === "property").length,
    0,
  ),
};

const output = {
  generator: "native/tools/inventory-service-channels.mjs",
  typescriptVersion: ts.version,
  generatedAt: new Date().toISOString().slice(0, 10),
  note: "Generated from the TypeScript checker. Do not hand-edit; regenerate after changing any service interface.",
  declaredChannels,
  services,
  totals,
  problems,
};

writeFileSync(
  join(repoRoot, "native", "fixtures", "service-channels.json"),
  `${JSON.stringify(output, null, 2)}\n`,
);

process.stderr.write(
  `channels=${totals.channels} members=${totals.members} rpc=${totals.rpcMethods} events=${totals.events} dynamicEvents=${totals.dynamicEvents} properties=${totals.properties} problems=${problems.length}\n`,
);
for (const service of services) {
  const rpc = service.methods.filter((method) => method.role === "rpc").length;
  const events = service.methods.filter((method) => method.role !== "rpc").length;
  process.stderr.write(
    `  ${String(service.channelName).padEnd(30)} ${service.descriptor.padEnd(40)} rpc=${
      rpc
    } events=${events}\n`,
  );
}
for (const problem of problems) process.stderr.write(`  problem: ${problem}\n`);
