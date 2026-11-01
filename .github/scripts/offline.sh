#!/usr/bin/env bash
# Runs a bash command in a network namespace where only loopback is up, so a test that
# tries to reach a real provider fails instead of sending anything. The command runs as
# the calling user, so file-permission tests behave as usual. CPA_TEST_NO_SKIP makes a
# test that would skip for missing PostgreSQL fail instead.
#
#   .github/scripts/offline.sh '<bash command>'
set -euo pipefail

vars=(
  "PATH=$PATH" "HOME=$HOME"
  "CARGO_HOME=${CARGO_HOME:-$HOME/.cargo}" "RUSTUP_HOME=${RUSTUP_HOME:-$HOME/.rustup}"
  CARGO_NET_OFFLINE=true CARGO_TERM_COLOR=always CPA_TEST_NO_SKIP=1
)
for name in CARGO_INCREMENTAL CARGO_PROFILE_TEST_DEBUG CPA_TEST_PG_BIN; do
  if [ -n "${!name:-}" ]; then vars+=("$name=${!name}"); fi
done

# shellcheck disable=SC2016 # $1 to $3 belong to the inner shell
exec sudo env "${vars[@]}" \
  unshare --net -- sh -c 'ip link set lo up && exec setpriv --reuid="$1" --regid="$2" --init-groups -- bash -eo pipefail -c "$3"' \
  sh "$(id -u)" "$(id -g)" "$1"
