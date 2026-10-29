#!/bin/sh
# Test-VM workaround: clamp negative OCI oomScoreAdj values before runc create/run.
set -eu

bundle=
expect_bundle=
for arg in "$@"; do
    if [ "$expect_bundle" = yes ]; then
        bundle=$arg
        expect_bundle=
        continue
    fi
    case $arg in
        --bundle=*) bundle=${arg#--bundle=} ;;
        --bundle|-b) expect_bundle=yes ;;
    esac
done

if [ -n "$bundle" ] && [ -f "$bundle/config.json" ]; then
    sed -i -E 's/"oomScoreAdj"[[:space:]]*:[[:space:]]*-[0-9]+/"oomScoreAdj":0/g' \
        "$bundle/config.json"
fi

exec "${FILAR_RUNC_REAL:-/bin/runc.real}" "$@"
