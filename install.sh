#!/bin/sh
# Install ack-bot (public repo, no GitHub login needed):
#   rm -rf /tmp/ack-bot-install && git clone -q --depth 1 -b release https://github.com/pankajyadav-blitz/slack-alert-ack.git /tmp/ack-bot-install && sh /tmp/ack-bot-install/install.sh; rm -rf /tmp/ack-bot-install
# Change the install dir with ACK_BOT_INSTALL_DIR=/usr/local/bin
set -eu

release_repository_url="${ACK_BOT_REPO_URL:-https://github.com/pankajyadav-blitz/slack-alert-ack.git}"
install_dir="${ACK_BOT_INSTALL_DIR:-$HOME/.local/bin}"
slack_dir="$HOME/.slack"

case "$(uname -s)" in
  Linux)  os_part="unknown-linux-musl" ;;
  Darwin) os_part="apple-darwin" ;;
  *) echo "ack-bot: unsupported OS $(uname -s) (on Windows, use WSL)" >&2; exit 1 ;;
esac

case "$(uname -m)" in
  x86_64 | amd64)  arch_part="x86_64" ;;
  arm64 | aarch64) arch_part="aarch64" ;;
  *) echo "ack-bot: unsupported CPU $(uname -m)" >&2; exit 1 ;;
esac

archive_name="ack-bot-${arch_part}-${os_part}.tar.gz"

temporary_dir="$(mktemp -d)"
trap 'rm -rf "$temporary_dir"' EXIT

release_dir="$(cd "$(dirname "$0")" 2>/dev/null && pwd || true)"
if [ -z "$release_dir" ] || [ ! -f "${release_dir}/${archive_name}" ]; then
  release_dir="${temporary_dir}/release"
  echo "ack-bot: fetching the release branch from ${release_repository_url}"
  git clone -q --depth 1 -b release "$release_repository_url" "$release_dir"
fi

version="$(cat "${release_dir}/VERSION" 2>/dev/null || echo unknown)"
echo "ack-bot: installing ${archive_name} (${version})"

expected_checksum="$(cut -d ' ' -f 1 "${release_dir}/${archive_name}.sha256")"
if command -v sha256sum >/dev/null 2>&1; then
  actual_checksum="$(sha256sum "${release_dir}/${archive_name}" | cut -d ' ' -f 1)"
else
  actual_checksum="$(shasum -a 256 "${release_dir}/${archive_name}" | cut -d ' ' -f 1)"
fi
if [ "$expected_checksum" != "$actual_checksum" ]; then
  echo "ack-bot: checksum mismatch, aborting" >&2
  exit 1
fi

tar -xzf "${release_dir}/${archive_name}" -C "$temporary_dir"
mkdir -p "$install_dir"
install -m 755 "${temporary_dir}/ack-bot" "${install_dir}/ack-bot"
echo "ack-bot: installed to ${install_dir}/ack-bot"

mkdir -p "$slack_dir"
chmod 700 "$slack_dir"
if [ -f "$slack_dir/config.yaml" ]; then
  echo "ack-bot: kept your existing $slack_dir/config.yaml"
elif [ -f "${release_dir}/config.example.yaml" ]; then
  cp "${release_dir}/config.example.yaml" "$slack_dir/config.yaml"
  echo "ack-bot: wrote a starter config to $slack_dir/config.yaml (edit channels there)"
fi

case ":$PATH:" in
  *":${install_dir}:"*) ;;
  *) echo "ack-bot: add ${install_dir} to your PATH, e.g. echo 'export PATH=\"${install_dir}:\$PATH\"' >> ~/.zshrc" ;;
esac
echo "ack-bot: next, run 'ack-bot get-user-token', then 'ack-bot paths' to check; update later with 'ack-bot update'"
