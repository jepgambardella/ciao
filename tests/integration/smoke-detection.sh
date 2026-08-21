#!/bin/sh
set -eu
root=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
ciao_bin=${CIAO_BIN:-$root/target/debug/ciao}
for example in rust-app go-app bun-app static-site; do
  output=$("$ciao_bin" inspect "$root/examples/$example")
  printf '%s\n' "$output" | grep -q 'project detected:'
done
printf '%s\n' 'integration detection smoke passed'
