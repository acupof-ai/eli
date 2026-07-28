set shell := ["bash", "-eu", "-o", "pipefail", "-c"]

# Check local prerequisites
doctor:
    ./scripts/doctor.sh

# Build release binary
build:
    cargo build --release

# Install globally
install:
    cargo install --path crates/eli

# Rust fmt + lint + tests
check:
    ./scripts/check.sh

# Run Rust tests only
test-rust:
    cargo test --workspace

# Run Python smoke tests
test-py:
    ./scripts/test_python.sh

# Run practical local validation across languages
test-all:
    cargo test --workspace
    ./scripts/test_python.sh

# Keep old shorthand behavior
test:
    cargo test --workspace

# Release-oriented validation
release-check:
    ./scripts/release_check.sh

# Run with arguments
run *ARGS:
    cargo run -p eli -- {{ARGS}}

# Clean
clean:
    cargo clean
