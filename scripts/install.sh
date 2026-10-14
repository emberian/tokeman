#!/bin/sh
set -eu

repo_dir=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cargo install --locked --path "$repo_dir" --force "$@"

binary="${CARGO_HOME:-$HOME/.cargo}/bin/tokeman"
if [ "$(uname -s)" = "Darwin" ]; then
  identity=${TOKEMAN_CODESIGN_IDENTITY:-}
  if [ -n "$identity" ]; then
    /usr/bin/codesign \
      --force \
      --sign "$identity" \
      --identifier com.ember.tokeman \
      --timestamp=none \
      "$binary"
  else
    echo "using ad-hoc signing (set TOKEMAN_CODESIGN_IDENTITY to opt into a stable identity)" >&2
    /usr/bin/codesign --force --sign - --timestamp=none "$binary"
  fi
  /usr/bin/codesign --verify --strict "$binary"
  "$binary" rotate install
else
  cat <<EOF
installed $binary

The background rotation service is a macOS LaunchAgent, so none was installed
here. Run the monitor under your own supervisor instead, for example a systemd
user unit whose ExecStart is:

  $binary rotate daemon
EOF
fi
