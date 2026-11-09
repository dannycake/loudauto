#!/bin/sh
# Stands in for the `loudauto` window in the window launcher's tests. Started through a
# link in a test's tempdir, it appends its pid to `pids` and the activation token it was
# given (`<none>` without one) to `ran`, beside that link, then exits. A `mode` file there
# keeps it running for up to 10 s instead: `stay` until SIGTERM (noted in `signals`),
# `stubborn` ignoring SIGTERM, so only SIGKILL ends it.
dir=$(dirname "$0")
mode=$(cat "$dir/mode" 2>/dev/null)
# The traps first, so a signal that comes once the pid is out is never missed.
case "$mode" in
stay) trap 'echo TERM >> "$dir/signals"; kill "$sleeper" 2>/dev/null; exit 143' TERM ;;
stubborn) trap '' TERM ;;
esac
printf '%s\n' "$$" >> "$dir/pids"
printf '%s\n' "${XDG_ACTIVATION_TOKEN:-<none>}" >> "$dir/ran"
case "$mode" in
stay)
    sleep 10 &
    sleeper=$!
    wait "$sleeper"
    ;;
# An ignored signal stays ignored across exec.
stubborn) exec sleep 10 ;;
esac
