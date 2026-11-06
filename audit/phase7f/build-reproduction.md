# Build Reproduction

cargo check -p zcode-host --lib → PASS (exit 0)
cargo check --workspace --lib → FAIL (zcode-services-agent: missing HostServer methods)
cargo test -p zcode-host --no-run → FAIL (test targets)
cargo check --workspace --all-targets → FAIL (test targets + ws_e2e moved)