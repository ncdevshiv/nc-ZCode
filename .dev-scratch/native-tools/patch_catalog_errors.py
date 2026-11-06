import io

path = 'crates/zcode-services-provider/src/catalog.rs'
s = io.open(path, encoding='utf-8').read()

s = s.replace(''' text_chars: usize,
    error: Option<String>,
}'',''' text_chars: usize,
    error: Option<Value>,
}''')

s = s.replace(''' let latency_ms = started.elapsed().as_millis() as u64;
    let error = execution.failure.map(|failure| failure.message);
    let ok = error.is_none() && !execution.events.is_empty();''',''' let latency_ms = started.elapsed().as_millis() as u64;
    // The failure facts must be able to distinguish "model is offline / current limiting / timeout / network": harness report and Auto fetch
    // All panels rely on it, instead of just one sentence "Model request failed.".
    let error = execution.failure.as_ref().map(failure_facts);
    let ok = error.is_none() && !execution.events.is_empty();''')

s = s.replace('''/// Actual measurement result of a single detection. ''','''/// executor final state failure → displayable failure fact (code + statusCode + whether it can be retried).
fn failure_facts(error: &zcode_provider::AdapterError) -> Value {
    json!({
        "code": serde_json::to_value(error.code).unwrap_or(Value::String("unknown".to_string())),
        "message": error.message,
        "statusCode": error.context.get("statusCode").cloned().unwrap_or(Value::Null),
        "reason": error.context.get("reason").cloned().unwrap_or(Value::Null),
        "retryable": error.context.get("retryable").cloned().unwrap_or(Value::Null),
    })
}

/// Actual measurement results of a single detection. ''')

io.open(path, 'w', encoding='utf-8', newline='').write(s)
print('patched catalog error facts')