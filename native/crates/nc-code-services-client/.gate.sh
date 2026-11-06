#!/usr/bin/env bash
cd /g/ZCode-main/native || exit 1
for i in $(seq 1 90); do
  out=$(cargo check -p zcode-services-client --all-targets 2>&1)
  if ! echo "$out" | grep -q "failed to load manifest"; then
    echo "WORKSPACE-LOADED attempt=$i"
    echo "===== CHECK ====="; echo "$out"
    echo "===== CLIPPY ====="; cargo clippy -p zcode-services-client --all-targets -- -D warnings 2>&1
    echo "===== FMT ====="; cargo fmt -p zcode-services-client --check 2>&1
    echo "===== TEST ====="; cargo test -p zcode-services-client 2>&1
    exit 0
  fi
  sleep 15
done
echo "TIMEOUT: workspace never loaded (zcode-cdp teammate still incomplete)"
