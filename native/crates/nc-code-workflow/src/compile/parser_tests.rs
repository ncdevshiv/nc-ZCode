#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_valid_ts() {
        let result = parse_workflow_script("const x: number = 1;", "test.ts");
        assert!(result.is_ok());
    }

    #[test]
    fn test_parse_invalid_ts() {
        let result = parse_workflow_script("const x = ;", "test.ts");
        assert!(result.is_err());
    }
}
