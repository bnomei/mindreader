#!/bin/sh
set -eu

SCRIPT_DIR=$(cd -- "$(dirname -- "$0")" && pwd)
INSTALLER="$SCRIPT_DIR/install.sh"

fail() { printf 'not ok - %s\n' "$*" >&2; exit 1; }
pass() { printf 'ok - %s\n' "$*"; }

assert_contains() {
  haystack=$1; needle=$2; label=$3
  case "$haystack" in *"$needle"*) pass "$label" ;; *) fail "$label: expected '$needle'" ;; esac
}

target() {
  os=$(uname -s); arch=$(uname -m)
  case "$os:$arch" in
    Linux:x86_64|Linux:amd64) printf '%s\n' x86_64-unknown-linux-gnu ;;
    Linux:aarch64|Linux:arm64) printf '%s\n' aarch64-unknown-linux-gnu ;;
    Darwin:x86_64|Darwin:amd64) printf '%s\n' x86_64-apple-darwin ;;
    Darwin:aarch64|Darwin:arm64) printf '%s\n' aarch64-apple-darwin ;;
    *) fail "unsupported test host: $os/$arch" ;;
  esac
}

sha256_file() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | awk '{print $1; exit}';
  elif command -v shasum >/dev/null 2>&1; then shasum -a 256 "$1" | awk '{print $1; exit}';
  else fail "no SHA-256 tool available"; fi
}

setup() {
  test_dir=$(mktemp -d "${TMPDIR:-/tmp}/mindreader-install-test.XXXXXX")
  fixture_dir=$test_dir/fixtures; fake_bin=$test_dir/bin; install_dir=$test_dir/install
  mkdir -p "$fixture_dir" "$fake_bin" "$install_dir" "$test_dir/build"
  release_target=$(target)
  archive="mindreader-v9.9.9-$release_target.tar.gz"
  printf '#!/bin/sh\nprintf "mindreader fixture\\n"\n' > "$test_dir/build/mindreader"
  chmod 755 "$test_dir/build/mindreader"
  tar -C "$test_dir/build" -czf "$fixture_dir/$archive" mindreader
  digest=$(sha256_file "$fixture_dir/$archive")
  printf '%s  %s\n' "$digest" "$archive" > "$fixture_dir/$archive.sha256"
}

make_fake_curl() {
  cat > "$fake_bin/curl" <<'EOF'
#!/bin/sh
out=; url=
while [ "$#" -gt 0 ]; do
  case "$1" in -o) shift; out=$1 ;; http://*|https://*) url=$1 ;; esac
  shift
done
case "$url" in
  https://api.github.com/repos/bnomei/mindreader/releases/latest)
    printf '{"tag_name":"v9.9.9"}\n' ;;
  https://github.com/bnomei/mindreader/releases/download/v9.9.9/*)
    file=${url##*/}; cp "$MINDREADER_FIXTURE_DIR/$file" "$out" ;;
  *) printf 'unexpected curl URL: %s\n' "$url" >&2; exit 9 ;;
esac
EOF
  chmod 755 "$fake_bin/curl"
}

test_help() {
  output=$(sh "$INSTALLER" --help)
  assert_contains "$output" MINDREADER_VERSION "help documents pinned versions"
  assert_contains "$output" MINDREADER_INSTALL_DIR "help documents install directory"
  assert_contains "$output" Windows "help documents Windows path"
}

test_pinned_install() {
  PATH="$fake_bin:$PATH" MINDREADER_FIXTURE_DIR="$fixture_dir" MINDREADER_VERSION=9.9.9 MINDREADER_INSTALL_DIR="$install_dir" sh "$INSTALLER" >/dev/null
  [ -x "$install_dir/mindreader" ] || fail "pinned version did not install an executable"
  assert_contains "$("$install_dir/mindreader")" "mindreader fixture" "pinned release installs and runs"
}

test_latest_install() {
  PATH="$fake_bin:$PATH" MINDREADER_FIXTURE_DIR="$fixture_dir" MINDREADER_INSTALL_DIR="$install_dir" sh "$INSTALLER" >/dev/null
  [ -x "$install_dir/mindreader" ] || fail "latest version did not install an executable"
  pass "latest release installs"
}

test_checksum_rejection() {
  printf '%064d  %s\n' 0 "$archive" > "$fixture_dir/$archive.sha256"
  if PATH="$fake_bin:$PATH" MINDREADER_FIXTURE_DIR="$fixture_dir" MINDREADER_VERSION=v9.9.9 MINDREADER_INSTALL_DIR="$install_dir" sh "$INSTALLER" >/dev/null 2>&1; then
    fail "checksum mismatch was accepted"
  fi
  [ ! -e "$install_dir/mindreader" ] || fail "checksum failure installed a binary"
  pass "checksum mismatch fails before installation"
}

main() {
  setup
  trap 'rm -rf "$test_dir"' 0 1 2 3 15
  make_fake_curl
  test_help
  test_pinned_install
  rm -f "$install_dir/mindreader"
  test_latest_install
  rm -f "$install_dir/mindreader"
  test_checksum_rejection
}

main "$@"
