#!/bin/sh
# Type-check, lint and build the unit tests for Linux from a macOS checkout,
# the way CI's Ubuntu job does, so a Linux-only warning is caught before push.
#
# Bundled SQLite and ring compile C for the target, which needs a cross C
# compiler; `zig cc` provides one with its own sysroot. cc-rs passes
# `--target=x86_64-unknown-linux-gnu`, a spelling zig rejects, so the compiler
# is a small wrapper that rewrites it.
set -eu

target=${TOKEMAN_LINUX_TARGET:-x86_64-unknown-linux-gnu}
zig_target=$(printf '%s' "$target" | sed 's/-unknown-linux-/-linux-/')

if ! command -v zig >/dev/null 2>&1; then
  echo "check-linux: zig is not installed (brew install zig); cannot cross-compile C for $target" >&2
  exit 2
fi
if ! rustup target list --installed | grep -qx "$target"; then
  echo "check-linux: run: rustup target add $target" >&2
  exit 2
fi

wrapper=$(mktemp -d "${TMPDIR:-/tmp}/tokeman-zig-cc.XXXXXX")/cc
trap 'rm -rf "$(dirname "$wrapper")"' EXIT
cat > "$wrapper" <<EOF
#!/bin/sh
args=""
skip=0
for a in "\$@"; do
  if [ "\$skip" = 1 ]; then skip=0; continue; fi
  case "\$a" in
    --target=*) continue ;;
    -target) skip=1; continue ;;
    *) args="\$args \\"\$a\\"" ;;
  esac
done
eval set -- \$args
exec zig cc -target $zig_target "\$@"
EOF
chmod +x "$wrapper"

# cc-rs reads CC_<target with underscores>; cargo reads the upper-case form.
cc_target=$(printf '%s' "$target" | tr '-' '_')
env_target=$(printf '%s' "$target" | tr 'a-z-' 'A-Z_')
export "CC_${cc_target}=$wrapper" "AR_${cc_target}=zig ar" \
  "CARGO_TARGET_${env_target}_LINKER=$wrapper"

cd "$(dirname "$0")/.."
cargo clippy --target "$target" --all-targets --locked -- -D warnings
cargo test --target "$target" --locked --no-run
