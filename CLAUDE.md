
# Rust Project Rules

- Write idiomatic and safe Rust.
- Prefer borrowing over unnecessary cloning.
- Do not use unwrap() or expect() without a clear reason.
- Use Result and ? for recoverable errors.
- Keep public APIs small and well documented.
- Prefer existing dependencies over adding new ones.
- Do not change public APIs without explaining the change.
- Use rust-analyzer for semantic code navigation.
- Use Context7 when current crate documentation is needed.

## Verification

After changing Rust code, run:

1. cargo fmt --all -- --check
2. cargo clippy --workspace --all-targets --all-features -- -D warnings
3. cargo test --workspace --all-features

If a command fails, report the failure and its cause.
Do not claim success unless the checks pass.
