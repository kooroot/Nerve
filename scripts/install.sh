#!/usr/bin/env sh
set -eu

repo="${NERVE_REPO:-kooroot/Nerve}"
bin_name="nv"
install_dir="${NERVE_INSTALL_DIR:-}"
tmp_dir="${TMPDIR:-/tmp}/nerve-install-$$"

cleanup() {
  rm -rf "$tmp_dir"
}
trap cleanup EXIT INT TERM

need_cmd() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "error: required command not found: $1" >&2
    exit 1
  fi
}

need_cmd uname
need_cmd curl
need_cmd tar

os="$(uname -s)"
arch="$(uname -m)"

case "$os" in
  Darwin)
    case "$arch" in
      arm64|aarch64) target="aarch64-apple-darwin" ;;
      x86_64|amd64) target="x86_64-apple-darwin" ;;
      *) echo "error: unsupported macOS architecture: $arch" >&2; exit 1 ;;
    esac
    archive="tar.gz"
    ;;
  Linux)
    case "$arch" in
      x86_64|amd64) target="x86_64-unknown-linux-gnu" ;;
      *) echo "error: unsupported Linux architecture: $arch" >&2; exit 1 ;;
    esac
    archive="tar.gz"
    ;;
  *)
    echo "error: unsupported operating system: $os" >&2
    exit 1
    ;;
esac

if [ -z "$install_dir" ]; then
  for candidate in "/opt/homebrew/bin" "/usr/local/bin" "$HOME/.local/bin"; do
    case "$os:$candidate" in
      Darwin:*|Linux:/usr/local/bin|Linux:"$HOME/.local/bin")
        if [ -d "$candidate" ] && [ -w "$candidate" ]; then
          install_dir="$candidate"
          break
        fi
        ;;
    esac
  done
fi

if [ -z "$install_dir" ]; then
  install_dir="$HOME/.local/bin"
fi

asset="nerve-${target}.${archive}"
url="https://github.com/${repo}/releases/latest/download/${asset}"

mkdir -p "$tmp_dir" "$install_dir"

echo "Downloading ${asset}..."
curl -fsSL "$url" -o "$tmp_dir/$asset"

tar -xzf "$tmp_dir/$asset" -C "$tmp_dir"

found_bin="$(find "$tmp_dir" -type f -name "$bin_name" -perm -u+x | head -n 1)"
if [ -z "$found_bin" ]; then
  echo "error: ${bin_name} binary was not found in ${asset}" >&2
  exit 1
fi

installed_bin="$install_dir/$bin_name"
cp "$found_bin" "$installed_bin"
chmod 755 "$installed_bin"

echo "Installed ${bin_name} to ${installed_bin}"

case ":${PATH:-}:" in
  *":$install_dir:"*) ;;
  *)
    echo "Add ${install_dir} to PATH to run '${bin_name}' from any directory."
    echo "For zsh: echo 'export PATH=\"${install_dir}:\$PATH\"' >> ~/.zshrc"
    ;;
esac

"$installed_bin" --version >/dev/null

active_bin="$(command -v "$bin_name" 2>/dev/null || true)"
if [ -n "$active_bin" ] && [ "$active_bin" != "$installed_bin" ]; then
  echo "Warning: this shell resolves '${bin_name}' to ${active_bin}, not ${installed_bin}."
  echo "Run: which -a ${bin_name}"
  echo "Then remove the stale earlier binary or put ${install_dir} earlier in PATH."
  echo "For zsh: hash -r"
  echo "Direct run: ${installed_bin} setup"
else
  echo "Run: ${bin_name} setup"
fi
