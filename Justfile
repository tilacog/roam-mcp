# Install the cargo tools `just ci` needs (coverage + CRAP gate).
# Uses cargo-binstall for prebuilt binaries; falls back to `cargo install`.
setup:
    cargo binstall --no-confirm cargo-llvm-cov cargo-crap || cargo install --locked cargo-llvm-cov cargo-crap

# Install binaries to the cargo bin directory.
install:
    cargo install --path .

clippy:
    cargo clippy --all-targets -- -W clippy::pedantic -D warnings

fmt *args:
    cargo fmt {{args}}

check-fmt:
    just fmt --check

test:
    cargo test

# Generate LCOV coverage data (run `just setup` to install cargo-llvm-cov).
coverage:
    cargo llvm-cov --lcov --output-path lcov.info

# Gate on the CRAP metric — fails if any function scores above 30.
# Run `just setup` to install cargo-crap.
crap: coverage
    cargo crap --lcov lcov.info --threshold 30 --fail-above

ci: check-fmt clippy test crap

# Emacs integration tests: verifies that a Rust-populated org-roam.db can be
# read by Emacs org-roam. Requires Emacs and the org-roam package.
emacs-tests:
    cargo test --features emacs-tests --test emacs_populator_roundtrip
