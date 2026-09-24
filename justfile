default: dev

# Run the app in debug mode.
#
# `ferret-searchd` is built first because Ferret launches it from beside its own
# binary. Without it search still works — it falls back to walking the disk —
# but nothing exercises the index, which is the opposite of what a dev run is
# for.
dev:
    cargo build -p ferret-searchd
    cargo run -p ferret

# Run the optimized release build.
run:
    cargo build --release -p ferret-searchd
    cargo run --release -p ferret

# Build the release binaries. Both ship: Ferret expects the helper beside it.
build:
    cargo build --release -p ferret -p ferret-searchd

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
    cargo test --workspace -- --test-threads=1

# The allocations-per-frame budget (docs/audit.html §11).
#
# Its own recipe because it installs a global allocator and therefore has to
# run alone.
budget:
    cargo test -p ferret-ui --features dhat-heap \
        allocation_budget -- \
        --nocapture --test-threads=1
