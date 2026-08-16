set windows-shell := ["powershell.exe", "-NoLogo", "-Command"]

fmt:
    cargo fmt --all -- --check

check:
    cargo check --locked --all-targets --all-features

clippy:
    cargo clippy --locked --all-targets --all-features -- -D warnings

test:
    cargo test --locked --all-targets --all-features

verify-full: fmt clippy test

target-report:
    cargo clean --dry-run

clean-preview:
    cargo clean --dry-run -v
