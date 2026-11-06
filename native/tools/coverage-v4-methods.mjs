// Forensics and self-testing of v4 method surface boundaries (`V4_METHODS_UNIMPLEMENTED`).
//
// Complementary check for channel members in Rust single test (method list test in `rpc.rs`). Add the other half here,
// Both of these are traces that must be left behind by the decision "not to transplant":
// 1. Each reason for not implementing a method must reference an existing governance document (specs/*.md or crate README),
// "No transplant" without source = cover for missing transplant;
// 2. The method names in the manifest must actually exist in the `V4_METHODS` fact source of `packages/shared`.
// TS side deleted the method but Rust still has reasons for not porting = ghost entries, which also distorts the list.
//Written as a pure function for `--self-test`: the access control that can be triggered without a counterexample is just decoration.

export function collectV4MethodBoundary({ rpcSource, factSource, citationExists }) {
  const localErrors = [];
  const v4Methods = new Set(
    [...factSource.matchAll(/^\s*\w+:\s*"(v4\/[^"]+)"/gm)].map((match) => match[1]),
  );
  if (v4Methods.size === 0) {
    localErrors.push(
      "v4 method access: Unable to parse V4_METHODS from transport.ts, fact source shape changed",
    );
  }
  const table = rpcSource.match(
    /V4_METHODS_UNIMPLEMENTED:\s*&\[\(&str,\s*&str\)\]\s*=\s*&\[([\s\S]*?)\n\];/,
  );
  if (!table) {
    localErrors.push(
      "v4 method access: V4_METHODS_UNIMPLEMENTED constant not found, manifest fact source moved",
    );
    return { entries: [], methods: v4Methods, errors: localErrors };
  }
  const entries = [];
  for (const [, name, reason] of table[1].matchAll(/\(\s*"([^"]+)"\s*,\s*([\s\S]*?)\s*\),/g)) {
    entries.push({ name, reason });
  }
  for (const { name, reason } of entries) {
    const citation = reason.match(/specs\/[\w.-]+\.md|README\.md/);
    if (!citation) {
      localErrors.push(
        `v4 method ${name}: Unimplemented reason does not reference the governance document and cannot be accepted as a boundary`,
      );
      continue;
    }
    if (!citationExists(citation[0])) {
      localErrors.push(
        `v4 method ${name}: The referenced document does not exist (${citation[0]})`,
      );
    }
    if (v4Methods.size > 0 && !v4Methods.has(name)) {
      localErrors.push(
        `v4 method ${name}: present in bounds list but not in V4_METHODS of packages/shared - ghost entry`,
      );
    }
  }
  return { entries, methods: v4Methods, errors: localErrors };
}

// Counterexample collection: Each item specifies the "error that must be reported", `expect: null` means that this item must pass cleanly.
const FACT_SOURCE = 'export const V4_METHODS = {\n conversationX: "v4/conversation/x",\n};\n';
const tableOf = (body) => `pub const V4_METHODS_UNIMPLEMENTED: &[(&str, &str)] = &[\n${body}\n];\n`;
const SELF_TEST_CASES = [
  {
    expect: "Unimplemented reason does not reference the governance document",
    rpc: tableOf(' ("v4/conversation/x", "Missing subsystem"),'),
  },
  {
    expect: "The referenced document does not exist",
    rpc: tableOf(' ("v4/conversation/x", "see specs/nope-gone.md"),'),
  },
  {
    expect: "ghost entry",
    rpc: tableOf(' ("v4/conversation/ghost", "see specs/disabled-features.md"),'),
  },
  { expect: "V4_METHODS_UNIMPLEMENTED constant not found", rpc: "pub struct Nothing;" },
  {
    expect: null,
    rpc: tableOf(' ("v4/conversation/x", "See specs/disabled-features.md §4.2"),'),
  },
  {
    expect: "v4 method access: Unable to parse V4_METHODS",
    rpc: tableOf(' ("v4/conversation/x", "see specs/disabled-features.md"),'),
    factSource: "export const NOT_A_TABLE = 1;",
  },
];

// The execution body of `--self-test`: returns [number of failures, number of use cases], and the exit code is determined by the caller.
export function runV4MethodSelfTest() {
  let failed = 0;
  for (const testCase of SELF_TEST_CASES) {
    const { errors } = collectV4MethodBoundary({
      rpcSource: testCase.rpc,
      factSource: testCase.factSource ?? FACT_SOURCE,
      citationExists: (citation) => citation === "specs/disabled-features.md",
    });
    const hit =
      testCase.expect === null
        ? errors.length === 0
        : errors.some((error) => error.includes(testCase.expect));
    if (!hit) {
      failed += 1;
      console.log(
        `[coverage] SELF-TEST FAIL: Expected "${testCase.expect}", actual ${JSON.stringify(errors)}`,
      );
    }
  }
  console.log(
    failed === 0
      ? `[coverage] self-test ok (${SELF_TEST_CASES.length} positive and negative examples are all triggered as expected)`
      : `[coverage] self-test FAILED(${failed}/${SELF_TEST_CASES.length})`,
  );
  return [failed, SELF_TEST_CASES.length];
}
