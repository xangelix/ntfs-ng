#!/usr/bin/env bash
# Central code quality, test and benchmark dispatcher for ntfs-ng.
# Shared between local runs, the git pre-commit hook and CI.
#
# Usage:
#   ./scripts/check.sh [all|fmt|typos|clippy|test|docs|nostd|msrv|fuzz-smoke|bench-smoke]
set -euo pipefail

SCRIPT_PATH="$(realpath "$0" 2>/dev/null || readlink -f "$0" 2>/dev/null || echo "$0")"
REPO_ROOT="$(cd "$(dirname "$SCRIPT_PATH")/.." && pwd -P)"
cd "$REPO_ROOT"

MSRV="$(sed -n 's/^rust-version = "\(.*\)"/\1/p' Cargo.toml)"

is_ci() {
    [[ "${CI:-false}" == "true" ]] || [[ "${GITHUB_ACTIONS:-false}" == "true" ]]
}

if is_ci || ! git rev-parse --is-inside-work-tree >/dev/null 2>&1; then
    git config --global --add safe.directory "$REPO_ROOT" 2>/dev/null || true
fi

group_start() {
    if is_ci; then echo "::group::$1"; else echo "==> $1"; fi
}

group_end() {
    if is_ci; then echo "::endgroup::"; fi
}

require() {
    if ! command -v "$1" >/dev/null 2>&1 && ! cargo "${1#cargo-}" --version >/dev/null 2>&1; then
        echo "error: '$1' is required but not installed." >&2
        exit 1
    fi
}

check_fmt() {
    group_start "Formatting (cargo fmt)"
    cargo fmt --all -- --check
    (cd fuzz && cargo fmt --all -- --check)
    group_end
}

check_typos() {
    group_start "Typos (typos)"
    require typos
    typos
    group_end
}

check_clippy() {
    group_start "Lints (cargo clippy), every feature combination"
    cargo clippy --all-targets -- -D warnings
    cargo clippy --all-targets --all-features -- -D warnings
    cargo clippy --lib --no-default-features -- -D warnings
    group_end
}

check_test() {
    group_start "Tests"
    if command -v cargo-nextest >/dev/null 2>&1 || cargo nextest --version >/dev/null 2>&1; then
        cargo nextest run --all-features
    else
        cargo test --all-features --all-targets
    fi
    cargo test --doc --all-features
    # The core alone, without std.
    cargo test --lib --no-default-features
    group_end
}

check_docs() {
    group_start "Documentation (rustdoc, warnings denied)"
    RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --all-features
    group_end
}

check_nostd() {
    group_start "no_std builds"
    cargo build --lib --no-default-features
    if rustup target list --installed 2>/dev/null | grep -q '^wasm32-unknown-unknown$'; then
        cargo build --lib --no-default-features --target wasm32-unknown-unknown
        cargo build --lib --target wasm32-unknown-unknown
    else
        echo "wasm32-unknown-unknown not installed: skipped"
    fi
    group_end
}

check_msrv() {
    group_start "Minimum supported Rust version ($MSRV)"
    cargo "+$MSRV" check --all-features --lib --tests
    cargo "+$MSRV" check --lib --no-default-features
    group_end
}

check_fuzz_smoke() {
    local seconds="${FUZZ_SECONDS:-20}"
    group_start "Fuzzing smoke run (${seconds}s per target)"
    require cargo-fuzz
    # cargo-fuzz defaults to the target it was itself built for: a prebuilt one is often musl,
    # which the sanitizers cannot instrument. Fuzz for the host.
    local host
    host="$(rustc +nightly -vV | sed -n 's/^host: //p')"
    for target in $(cd fuzz && cargo fuzz list); do
        echo "--> $target"
        # Inputs that once failed are replayed first (fuzz/regressions, tracked); new ones go to
        # the corpus (untracked).
        local corpora=("corpus/$target")
        mkdir -p "fuzz/corpus/$target"
        if [ -d "fuzz/regressions/$target" ]; then
            corpora+=("regressions/$target")
        fi
        (cd fuzz && cargo +nightly fuzz run --target "$host" "$target" "${corpora[@]}" -- \
            -max_total_time="$seconds" -timeout=10 -rss_limit_mb=4096)
    done
    group_end
}

check_bench_smoke() {
    group_start "Benchmarks: every bench once, every workload on a small corpus"
    cargo bench --all-features -- --test
    ./scripts/bench.sh --records 20000 --iterations 1 --output target/bench-smoke
    group_end
}

check_all() {
    check_fmt
    check_typos
    check_clippy
    check_test
    check_docs
    check_nostd
    echo "==> All checks passed."
}

cmd="${1:-all}"
case "$cmd" in
    fmt) check_fmt ;;
    typos) check_typos ;;
    clippy) check_clippy ;;
    test) check_test ;;
    docs) check_docs ;;
    nostd) check_nostd ;;
    msrv) check_msrv ;;
    fuzz-smoke) check_fuzz_smoke ;;
    bench-smoke) check_bench_smoke ;;
    all) check_all ;;
    *)
        echo "Unknown command: $cmd" >&2
        echo "Usage: $0 [all|fmt|typos|clippy|test|docs|nostd|msrv|fuzz-smoke|bench-smoke]" >&2
        exit 2
        ;;
esac
