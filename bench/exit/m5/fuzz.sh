#!/usr/bin/env bash
# Runs every cargo-fuzz target for FUZZ_SECONDS each (default 300) in a
# nightly Rust container, so Windows and macOS hosts can fuzz too. Cargo's
# registry, cargo-fuzz and the build cache live in Docker volumes. The
# sources are copied into the container at start, so editing the tree
# during a long run does not change what is fuzzed; the corpus is copied
# back to fuzz/corpus/<target> and crashes to fuzz/artifacts/.
#
# Usage (repo root): FUZZ_SECONDS=600 TARGETS="router snapshot_json" bench/exit/m5/fuzz.sh
# Summary: bench/exit/results/m5-fuzz.txt
set -euo pipefail
export MSYS_NO_PATHCONV=1
ROOT=$(cd "$(dirname "$0")/../../.." && (pwd -W 2>/dev/null || pwd))
SECONDS_EACH=${FUZZ_SECONDS:-300}
TARGETS=${TARGETS:-$(sed -n 's/^name = "\(.*\)"$/\1/p' "$(dirname "$0")/../../../fuzz/Cargo.toml" | grep -v '^ferry-fuzz$')}
IMAGE=${RUST_NIGHTLY_IMAGE:-rustlang/rust:nightly-bookworm}

docker run --rm \
  -v "$ROOT:/ferry" \
  -v ferry-fuzz-cargo:/usr/local/cargo/registry \
  -v ferry-fuzz-bin:/opt/cargo-bin \
  -v ferry-fuzz-target:/ferry-target \
  -e CARGO_TARGET_DIR=/ferry-target   -e RUSTUP_TOOLCHAIN=nightly \
  -e SECONDS_EACH="$SECONDS_EACH" -e TARGETS="$TARGETS" \
  -w /work/fuzz "$IMAGE" bash -euo pipefail -c '
    export PATH=/opt/cargo-bin/bin:$PATH
    mkdir -p /work
    tar -C /ferry --exclude=./target --exclude=./fuzz/target --exclude=./.git -cf - . | tar -C /work -xf -
    save() { mkdir -p /ferry/fuzz/corpus; cp -r corpus/. /ferry/fuzz/corpus/; [ -d artifacts ] && mkdir -p /ferry/fuzz/artifacts && cp -r artifacts/. /ferry/fuzz/artifacts/ || true; }
    trap save EXIT
    command -v cargo-fuzz >/dev/null || cargo install --locked --root /opt/cargo-bin cargo-fuzz
    out=/ferry/bench/exit/results/m5-fuzz.txt
    echo "cargo-fuzz, $SECONDS_EACH s per target, $(rustc --version)" > "$out"
    status=0
    for t in $TARGETS; do
      mkdir -p corpus/$t
      echo "== $t"
      if cargo fuzz run "$t" corpus/$t seeds/$t -- -max_total_time=$SECONDS_EACH -rss_limit_mb=2048 -print_final_stats=1 > /tmp/$t.log 2>&1; then
        result=ok
      else
        result=CRASH
        status=1
      fi
      runs=$(sed -n "s/^stat::number_of_executed_units: *//p" /tmp/$t.log | tail -1 || true)
      cov=$(grep -o "cov: [0-9]*" /tmp/$t.log | tail -1 || true)
      corpus=$(ls corpus/$t | wc -l)
      printf "%-18s %-6s %12s execs  %-10s corpus %s\n" "$t" "$result" "${runs:-?}" "${cov:-cov: ?}" "$corpus" | tee -a "$out"
      [ "$result" = ok ] || tail -40 /tmp/$t.log
    done
    exit $status
  '
