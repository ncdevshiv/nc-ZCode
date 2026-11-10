use std::process::ExitCode;

fn main() -> ExitCode {
    ExitCode::from(nc_code_diff::run() as u8)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    /// The binary's dispatch entry produces a real unified diff for two texts.
    #[test]
    fn handle_builds_a_real_unified_diff() {
        let result = nc_code_diff::handle(
            "diff.unified",
            &json!({"old": "a\nb\n", "new": "a\nc\n", "path": "p.txt"}),
        )
        .expect("diff");
        let rendered = serde_json::to_string(&result).expect("json");
        assert!(
            rendered.contains("-b") || rendered.contains("+c"),
            "{rendered}"
        );
    }
}
