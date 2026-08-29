default: dev

# Run the app in debug mode.
dev:
    cargo run -p ferret

# Run the optimized release build.
run:
    cargo run --release -p ferret

# Build the release binary.
build:
    cargo build --release -p ferret

# Type-check the whole workspace.
check:
    cargo check --workspace --all-targets

# Format, lint and test.
lint:
    cargo fmt --all -- --check
    cargo clippy --workspace --all-targets -- -D warnings

fmt:
    cargo fmt --all

test:
    cargo test --workspace

# The allocations-per-frame budget (docs/audit.html §11).
#
# Its own recipe because it installs a global allocator and therefore has to
# run alone.
budget:
    cargo test -p ferret-ui --features dhat-heap \
        folder::alloc_budget::frame_allocation_budget -- \
        --exact --nocapture --test-threads=1
