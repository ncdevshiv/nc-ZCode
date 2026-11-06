// Golden fixture for credential cipher compatibility.
//
// Generates `enc:v1:` ciphertexts with the REAL TypeScript cipher provider
// (`packages/services/src/credential/providers/credentialCipherProvider.ts`) using a
// fixed secret, so the Rust implementation can prove it decrypts existing credentials.
//
// node native/tools/generate-credential-fixture.mjs

import { createRequire } from "node:module";
import { mkdtempSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { fileURLToPath } from "node:url";

const repoRoot = fileURLToPath(new URL("../../", import.meta.url));
const require = createRequire(import.meta.url);
const esbuild = require(join(repoRoot, "node_modules", "esbuild", "lib", "main.js"));

const workDir = mkdtempSync(join(tmpdir(), "zcode-credential-"));
const bundlePath = join(workDir, "cipher.cjs");
esbuild.buildSync({
  stdin: {
    contents: `export { createCredentialCipherProvider } from "./credentialCipherProvider.js";`,
    resolveDir: join(repoRoot, "packages", "services", "src", "credential", "providers"),
    sourcefile: "credential-entry.ts",
    loader: "ts",
  },
  bundle: true,
format: "cjs",
  platform: "node",
  outfile: bundlePath,
  logLevel: "silent",
});

const { createCredentialCipherProvider } = require(bundlePath);
const secret = "zcode-cross-language-test";
const cipher = createCredentialCipherProvider({ env: { ZCODE_CREDENTIAL_SECRET: secret } });
const values = ["hello", "", "oauth-token/中文 ✓", "x".repeat(300)];
const vectors = values.map((plaintext) => ({
  plaintext,
  encrypted: cipher.encrypt(plaintext),
}));

const fixture = {
  generator: "native/tools/generate-credential-fixture.mjs", secret, format: "enc:v1:<iv>.<authTag>.<ciphertext> (base64url, no padding, AES-256-GCM)",
  vectors,
};
writeFileSync(
  join(repoRoot, "native", "fixtures", "credential-cipher.json"),
  `${JSON.stringify(fixture, null, 2)}\n`,
);
process.stderr.write(`vectors=${vectors.length}\n`);