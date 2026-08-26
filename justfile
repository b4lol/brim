# Brim task runner. Run `just` to list the available recipes.

default:
    @just --list

# Full CI verification suite (fmt + clippy + test + release build).
check: fmt clippy test build

fmt:
    cargo fmt --all -- --check

clippy:
    cargo clippy --all-targets -- -D warnings

test:
    cargo test --all-targets

build:
    cargo build --release

# Install for the current user only (~/.cargo/bin).
install-user:
    cargo install --path . --locked

# Install system-wide into /usr/local/bin so `sudo brim ...` works too.
# (sudo's secure_path excludes ~/.cargo/bin; /usr/local/bin is always in it.)
install: install-user
    sudo install -m 0755 ~/.cargo/bin/brim /usr/local/bin/brim
    @echo "brim installed to /usr/local/bin — 'sudo brim' now works."

# Remove both the user and the system-wide install.
uninstall:
    -cargo uninstall brim
    sudo rm -f /usr/local/bin/brim

# Install bash and zsh completions for the current user.
completions: install-user
    #!/usr/bin/env bash
    set -euo pipefail
    mkdir -p ~/.local/share/bash-completion/completions ~/.local/share/zsh/site-functions
    brim completions bash > ~/.local/share/bash-completion/completions/brim
    brim completions zsh > ~/.local/share/zsh/site-functions/_brim
    echo "Completions installed. zsh: ensure ~/.local/share/zsh/site-functions is in \$fpath."
