//! `ServiceArgs` decodes positional channel arguments the way the proxy sends them.

use serde_json::json;

#[test]
fn service_args_decode_positional_arguments() {
    use nc_code_rpc::WireValue;
    let args = nc_code_service_core::ServiceArgs::new(vec![
        WireValue::Json(json!({"path": "a.txt"})),
        WireValue::Json(json!({"mode": "build"})),
    ]);
    let first = args.required_json(0, "params").expect("params");
    assert_eq!(first["path"], json!("a.txt"));
    let second = args.required_json(1, "options").expect("options");
    assert_eq!(second["mode"], json!("build"));
    let error = args.required_json(2, "extra").expect_err("missing arg");
    assert_eq!(error.name, "ZodError");
}
