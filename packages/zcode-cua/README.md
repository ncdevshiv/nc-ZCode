# @zcode/zcode-cua

API-compatible placeholder package for Computer Use (TypeScript side). This build ships without
Computer Use: every runtime surface (Computer Use runtime, broker RPC, Helper
install/launch/verify, PiP session client, native addon loader) reports
**unavailable** and fails closed, predicates about official CUA frames are
`false`, and permission ports keep their privacy fail-closed semantics.

Real native computer-use lives only in Rust:
- `native/crates/nc-code-host/src/tools/computer.rs` (Computer tool + `ComputerControlPort` contract)
- `native/crates/nc-code-shell/src/computer_control.rs` (`ShellComputerControl`, Windows SendInput/GDI)

Without a port the Rust tool fails closed with `computer_unavailable`; non-Windows returns
`capability_unsupported`. This TS package does not drive those Rust paths.

License: Apache-2.0.
