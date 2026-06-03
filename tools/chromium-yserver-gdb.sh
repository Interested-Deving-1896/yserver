#!/usr/bin/env bash
# Run Chromium's main process under gdb against yserver to capture the
# backtrace of the startup SIGTRAP (a Chromium CHECK/IMMEDIATE_CRASH in
# its X11/Ozone code — the class of bug as the ListInputDevices /
# QueryTree fixes). The fatal CHECK prints no message and generates no
# coredump, so gdb is the only way to see which X-protocol expectation
# yserver is violating.
#
# Usage (run from a terminal on the yserver session, so DISPLAY=:7):
#
#     tools/chromium-yserver-gdb.sh [backend] [logfile]
#
#   backend   ANGLE backend to use (default: vulkan; the one that gets
#             far enough to hit the X11 CHECK). Also accepts gl, gles,
#             swiftshader.
#   logfile   where to capture gdb + chromium output (default: chromium-gdb.log)
#
# When it stops on SIGTRAP, gdb auto-prints `bt` + all-thread backtraces
# and quits. If it stops on an EARLIER benign trap (not the crash),
# it'll just continue automatically. Send the resulting logfile.

set -uo pipefail

BACKEND="${1:-vulkan}"
LOGFILE="${2:-chromium-gdb.log}"

if [[ -z "${DISPLAY:-}" ]]; then
	echo "DISPLAY is unset — run from a terminal on the yserver session (wezterm on :7)" >&2
	exit 1
fi

BIN=""
for cand in /usr/lib/chromium/chromium /usr/lib64/chromium/chromium \
	"$(command -v chromium 2>/dev/null)" \
	"$(command -v google-chrome-stable 2>/dev/null)"; do
	if [[ -n "$cand" && -x "$cand" ]]; then
		BIN="$cand"
		break
	fi
done
if [[ -z "$BIN" ]]; then
	echo "no chromium/chrome binary found" >&2
	exit 1
fi
if ! command -v gdb >/dev/null 2>&1; then
	echo "gdb not installed (pacman -S gdb)" >&2
	exit 1
fi

PROFILE="$(mktemp -d /tmp/cr-gdb.XXXXXX)"

echo "gdb-running $BIN (--use-angle=$BACKEND) on DISPLAY=$DISPLAY → $LOGFILE" >&2

# Keep the GL stack debuggable + the trap in the main process where gdb
# can catch it: no sandbox, no breakpad (so gdb gets SIGTRAP, not crashpad),
# in-process-gpu so a GL/Vulkan abort also surfaces in this process.
CHROME_FLAGS=(
	--ozone-platform=x11
	--use-angle="$BACKEND"
	--ignore-gpu-blocklist
	--no-sandbox
	--disable-breakpad
	--in-process-gpu
	--enable-logging=stderr
	--v=1
	--vmodule="*gl*=3,*egl*=3,*angle*=3,*dri*=3,*x11*=2,*ozone*=2,gpu*=2"
	--user-data-dir="$PROFILE"
)

# gdb batch script:
#  - tell gdb to print (not swallow) SIGTRAP and stop on it
#  - run; on the first stop, dump backtraces; if it's a benign trap
#    (SIGTRAP but program still live and not a fatal frame) we still dump
#    and quit — the dump is what we want either way.
GDB_CMDS=(
	-q
	-ex 'set pagination off'
	-ex 'set confirm off'
	-ex 'handle SIGTRAP stop print'
	-ex 'run'
	-ex 'echo \n===== BACKTRACE (faulting thread) =====\n'
	-ex 'bt 40'
	-ex 'echo \n===== ALL THREADS =====\n'
	-ex 'thread apply all bt 12'
	-ex 'echo \n===== REGISTERS =====\n'
	-ex 'info registers rip'
	-ex 'quit'
)

env DISPLAY="$DISPLAY" GDK_BACKEND=x11 MESA_DEBUG=1 \
	gdb "${GDB_CMDS[@]}" --args "$BIN" "${CHROME_FLAGS[@]}"

echo "done — see $LOGFILE (grep for 'Check failed', 'x11', 'XProto', 'IMMEDIATE_CRASH')" >&2
echo "profile dir: $PROFILE (rm when done)" >&2
