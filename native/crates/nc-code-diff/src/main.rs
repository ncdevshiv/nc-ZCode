//! nc-code-diff executable entry: `ready` → serve loop → return the exit code according to the shutdown reason.

use std::process::ExitCode;

fn main() -> ExitCode {
    ExitCode::from(nc_code_diff::run() as u8)
}

#[cfg(test)]
mod tests {
    #[test]
    fn basic_test() {
        assert!(true);
    }
}
