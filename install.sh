#!/usr/bin/env sh
set -eu

repo_url="${CODEX_RAIL_REPO:-}"
prefix="${PREFIX:-$HOME/.local}"
bin_dir="$prefix/bin"

if ! command -v cargo >/dev/null 2>&1; then
  echo "cargo is required to install Codex Rail from source." >&2
  echo "Install Rust first, then rerun this script." >&2
  exit 1
fi

workdir=""
cleanup() {
  if [ -n "$workdir" ] && [ -d "$workdir" ]; then
    rm -rf "$workdir"
  fi
}
trap cleanup EXIT INT TERM

if [ -f Cargo.toml ] && grep -q 'name = "codex-rail"' Cargo.toml; then
  src_dir="$(pwd)"
else
  if [ -z "$repo_url" ]; then
    echo "Run this inside the repo, or set CODEX_RAIL_REPO to the git URL." >&2
    exit 1
  fi
  if ! command -v git >/dev/null 2>&1; then
    echo "git is required when installing from CODEX_RAIL_REPO." >&2
    exit 1
  fi
  workdir="$(mktemp -d)"
  git clone --depth 1 "$repo_url" "$workdir/codex-rail"
  src_dir="$workdir/codex-rail"
fi

cd "$src_dir"
cargo build --release
mkdir -p "$bin_dir"
cp target/release/rail "$bin_dir/rail"
printf 'installed rail to %s\n' "$bin_dir/rail"
