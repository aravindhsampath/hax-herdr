#!/bin/sh
# End to end: runs hax-herdr in a herdr-core terminal with hax's mock provider, then checks the
# statuses Herdr recorded: idle at start, a finished turn, an Esc-interrupted turn, and that the
# watcher leaves with hax. Needs hc (herdr-core), hax and jq; touches no real hax session.
set -eu

bin=${1:-target/release/hax-herdr}
bin="$(cd "$(dirname "$bin")" && pwd)/$(basename "$bin")"
work=$(mktemp -d)
t=

cleanup() {
	kill "$events" 2>/dev/null || true
	if [ -n "$t" ]; then
		hc kill "$t" >/dev/null 2>&1 || true
		hc rm "$t" >/dev/null 2>&1 || true
	fi
	rm -rf "$work"
}

state() { hc get "$t" --json | jq -r '.status.state // "none"'; }

# Waits up to 10 s for the terminal's status to read $1.
await() {
	i=0
	while [ "$(state)" != "$1" ]; do
		i=$((i + 1))
		[ "$i" -le 40 ] || { echo "FAIL: status is $(state), wanted $1" >&2; exit 1; }
		sleep 0.25
	done
}

# One scripted reply per turn: a short one, a slow one to interrupt, and the continuation.
cat >"$work/mock.txt" <<'EOF'
text Done in one go.
end-turn
delay 700
text This reply streams slowly so that Esc can cut it off long before it ends, word by word.
end-turn
delay 0
text Picked up where it stopped.
end-turn
EOF

hc events --json >"$work/events.jsonl" &
events=$!
trap cleanup EXIT INT TERM

t=$(hc run --bg --json --no-parent --cwd "$work" -- env XDG_STATE_HOME="$work/state" \
	HAX_PROVIDER=mock HAX_NO_SESSION=0 HAX_MOCK_SCRIPT="$work/mock.txt" "$bin" | jq -r '.address')
echo "terminal $t"
await idle

hc send "$t" "first prompt" >/dev/null
hc wait "$t" >/dev/null
await "done"
echo "ok   finished turn: done"

hc send "$t" "second prompt" >/dev/null
await working
i=0
until hc read "$t" | grep -q 'streams slowly'; do
	i=$((i + 1))
	[ "$i" -le 40 ] || { echo "FAIL: the reply never streamed" >&2; exit 1; }
	sleep 0.25
done
# Mid-reply: one Esc asks for a pause after this step, the second interrupts at once.
hc keys "$t" esc esc >/dev/null
await idle
echo "ok   Esc Esc turn: idle"

# Enter on the empty prompt continues: a turn with no typed prompt.
hc keys "$t" enter >/dev/null
await "done"
echo "ok   continued turn: done"

watcher=$(pgrep -f "$bin" || true)
[ -n "$watcher" ] || { echo "FAIL: no watcher process" >&2; exit 1; }
echo "ok   watcher pid $watcher, $(ps -o rss= -p "$watcher" | tr -d ' ') KB resident"

hc keys "$t" ctrl+d >/dev/null
hc wait "$t" >/dev/null || true
sleep 0.5
[ "$(hc get "$t" --json | jq -r '.state')" = finished ] || { echo "FAIL: hax still running" >&2; exit 1; }
! pgrep -f "$bin" >/dev/null || { echo "FAIL: watcher outlived hax" >&2; exit 1; }
echo "ok   hax exited, watcher gone"

seen=$(jq -r --arg t "$t" 'select(.params.type? == "status" and .params.terminal == $t)
	| .params.data.status.state // "none"' "$work/events.jsonl" | tr '\n' ' ')
echo "statuses: $seen"
[ "$seen" = "idle working done working idle working done none " ] || { echo "FAIL: unexpected status sequence" >&2; exit 1; }
echo "PASS"
