import io

# --- provider timing: drop the non-ASCII note, keep ASCII-free Chinese comments consistent with repo style but valid ---
path = 'crates/zcode-provider/src/timing.rs'
s = io.open(path, encoding='utf-8').read()
s = s.replace('''//! Transport wrapper that records the arrival time of the block; the rest of the behavior is completely forwarded to inner.''',
              '''/// The transport wrapper that records the arrival time of the block; the rest of the behavior is completely forwarded to inner. ''')
# replace deprecated fetch_update with compare_exchange loop
s = s.replace(''' self.first_chunk_ms
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |current| {
                if current == 0 {
                    Some(now)
                } else {
                    None
                }
            })
            .ok();''',''' // First block is remembered only once: CAS loop instead of fetch_update (the latter is deprecated in new compilers).
        let mut current = self.first_chunk_ms.load(Ordering::SeqCst);
        loop {
            if current != 0 {
                break;
            }
            match self
                .first_chunk_ms
                .compare_exchange(current, now, Ordering::SeqCst, Ordering::SeqCst)
            {
                Ok(_) => break,
                Err(seen) => current = seen,
            }
        }''')
io.open(path, 'w', encoding='utf-8', newline='').write(s)
print('timing patched')

# --- host turn: measure with timing transport, attach throughput to assistant final row ---
path = 'crates/zcode-host/src/session.rs'
s = io.open(path, encoding='utf-8').read()
s = s.replace(''' pub fn project_assistant_final(
        &self,
        row_id: u64,
        response_id: &str,
        text: &str,
        state: &str,
        model: Option<&str>,
    ) -> Vec<String> {''',''' pub fn project_assistant_final(
        &self,
        row_id: u64,
        response_id: &str,
        text: &str,
        state: &str,model: Option<&str>,
        throughput: Option<&Value>,
    ) -> Vec<String> {'''')
s = s.replace(''' let row = rows::assistant_text_row(&base, response_id, text, state, model);
        self.upsert_row(row.clone());
        self.project_deltas(vec![rows::row_upserted(row)], true)''',''' let mut row = rows::assistant_text_row(&base, response_id, text, state, model);
        // additive measured throughput (tok/s): only written in the real generation window, this field is not written by default.
        if let Some(throughput) = throughput {
            if let Some(object) = row.as_object_mut() {
                object.insert("throughput".to_string(), throughput.clone());
            }
        }
        self.upsert_row(row.clone());
        self.project_deltas(vec![rows::row_upserted(row)], true)''')
io.open(path, 'w', encoding='utf-8', newline='').write(s)
print('session patched')