with open('G:/ZCode-main/native/crates/nc-code-services-system/src/lib.rs', 'r') as f:
    content = f.read()

old = '''                results.push(json!({
                    "targetId": target_id,
                    "kind": "tcp",
                    "host": host,
                    "port": port,
                    "reachable": reachable,
                    "attemptCount": attempt_count,
                    "latencyMs": latency_ms,
                    "error": if !reachable { Some("probe failed") } else { serde_json::Value::Null }
                }));'''

new = '''                let err = if !reachable { Some(json!("probe failed")) } else { None };
                results.push(json!({
                    "targetId": target_id,
                    "kind": "tcp",
                    "host": host,
                    "port": port,
                    "reachable": reachable,
                    "attemptCount": attempt_count,
                    "latencyMs": latency_ms,
                    "error": err
                }));'''

content = content.replace(old, new)
with open('G:/ZCode-main/native/crates/nc-code-services-system/src/lib.rs', 'w') as f:
    f.write(content)
print('done')
