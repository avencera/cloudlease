# check formatting
fmt:
    cargo fmt --all -- --check

# apply formatting
fmt-write:
    cargo fmt --all

# lint native and wasm targets with warnings as errors
clippy:
    cargo clippy --workspace --all-targets -- -D warnings
    cargo clippy -p cloudlease-worker -p cloudlease-test-worker --target wasm32-unknown-unknown -- -D warnings

# run the Rust tests
test:
    cargo test --workspace

# build the test Worker and run the miniflare suite
test-worker:
    pnpm install --frozen-lockfile
    pnpm run typecheck
    pnpm run test:worker

# everything CI runs
check: fmt clippy test test-worker
