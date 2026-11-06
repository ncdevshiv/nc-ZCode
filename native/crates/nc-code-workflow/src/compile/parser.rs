use oxc_allocator::Allocator;
use oxc_parser::Parser;
use oxc_span::SourceType;

pub fn parse_workflow_script(source_text: &str, file_name: &str) -> Result<bool, String> {
    let allocator = Allocator::default();
    let source_type = SourceType::from_path(file_name).unwrap_or_else(|_| SourceType::ts());
    let parser_ret = Parser::new(&allocator, source_text, source_type).parse();
    if !parser_ret.errors.is_empty() {
        let errors: Vec<String> = parser_ret.errors.iter().map(|e| e.to_string()).collect();
        return Err(errors.join("\n"));
    }
    Ok(true)
}

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
