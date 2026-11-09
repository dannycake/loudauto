#!/bin/bash
# Test double for Spotify Soloist, used only by tests (playback's supervisor, the engine's
# Soloist loop). It never prints its -k argument. Behaviour is chosen with FAKE_MODE:
#   ok | expired | locked | slow | nonutf8 | crash | crashloop | env | orphan
# FAKE_PORT is the port it publishes in ws.port (default 45678).
data=""
while [ $# -gt 0 ]; do
  case "$1" in
    -D) data="$2"; shift 2 ;;
    *) shift ;;
  esac
done
# Publish the pid and port, then idle until SIGTERM, which shuts down cleanly.
serve() {
  trap 'echo "shutting down"; rm -f "$data/ws.port"; exit 0' TERM
  echo $$ > "$data/soloist.pid"
  echo "${FAKE_PORT:-45678}" > "$data/ws.port"
  while true; do sleep 0.05; done
}
case "${FAKE_MODE:-ok}" in
  ok)
    printf '20:19:59.979 I [f:347                           ]     codec: flac\033[0m\n'
    printf '20:19:59.979 I [f:348                           ]     bitrate: 1400000 bits/s\033[0m\n'
    printf '20:19:59.979 I [f:349                           ]     sample rate: 44100 Hz\033[0m\n'
    serve
    ;;
  nonutf8)
    printf '\xff\xfe\n'
    printf '00:17:30.863 I [f:128                           ] client expires in 89 days\033[0m\n'
    serve
    ;;
  crash)
    # A crash mid-session, on cue: SIGUSR1 makes it print `boom` and exit FAKE_EXIT (3).
    trap 'echo boom >&2; exit "${FAKE_EXIT:-3}"' USR1
    serve
    ;;
  crashloop)
    # The first FAKE_CRASHES (5) starts print `boom` and exit 3 before opening the port;
    # later ones serve. The count lives in the data dir.
    count="$data/fake-crashes"
    n=$(cat "$count" 2>/dev/null || echo 0)
    if [ "$n" -lt "${FAKE_CRASHES:-5}" ]; then
      echo $((n + 1)) > "$count"
      echo boom >&2
      exit 3
    fi
    serve
    ;;
  orphan)
    # A child that outlives it holding its output pipe: once this process has gone, the
    # child prints an expiry line every 0.1 s for 3 s, then ends.
    parent=$$
    (
      while kill -0 "$parent" 2>/dev/null; do sleep 0.05; done
      for _ in $(seq 30); do
        printf '00:17:30.863 I [f:128                           ] client expires in 7 days\n'
        sleep 0.1
      done
    ) &
    serve
    ;;
  env)
    # Names only: the environment may hold the owner's credentials.
    compgen -e > "$FAKE_ENV_FILE"
    serve
    ;;
  expired)
    echo "client expired"
    exit 10
    ;;
  locked)
    echo "another Spotify Soloist process is using the data directory" >&2
    exit 1
    ;;
  slow)
    # exec: a kill must not leave an orphaned `sleep` holding the output pipes.
    exec sleep 30
    ;;
esac
