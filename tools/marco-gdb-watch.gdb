# Watchpoint hunt for marco lock_fns->lock_display corruption.
#
# The first Display* opened in marco's process is a transient one from
# GTK's AT-SPI accessibility init — it gets XCloseDisplay'd very early,
# at which point its lock_fns is free()d and the address gets reused by
# subsequent malloc()s. Naive "watch first dpy" fires spuriously
# forever after that close.
#
# Strategy: re-arm the watchpoint on every XQueryExtension call,
# deleting the previous one. So the live watchpoint always tracks the
# most-recently-used dpy's lock_fns slot. By the time of the real
# corruption (which happens to marco's real X11 display, the LAST one
# touched before XResQueryExtension), we'll be watching the right
# address, and the WATCH HIT will pinpoint the corrupting frame.

set confirm off
set pagination off
set print thread-events off
set debuginfod enabled on
set breakpoint pending on
handle SIGPIPE nostop noprint pass
handle SIGUSR1 nostop noprint pass

set $watch_num = 0

break XQueryExtension
commands
    silent
    # Drop the previous watchpoint if any.
    if $watch_num != 0
        delete $watch_num
        set $watch_num = 0
    end
    set $dpy = (char*) $rdi
    set $lock_fns = *(char**) ($dpy + 0x968)
    if $lock_fns != 0
        set $lock_display = *(void**) $lock_fns
        printf "[XQE] dpy=%p lock_fns=%p lock_display=%p\n", $dpy, $lock_fns, $lock_display
        # Arm a fresh hw write-watchpoint on this dpy's lock_display slot.
        watch *(void**) $lock_fns
        set $watch_num = $bpnum
        commands $watch_num
            silent
            printf "\n=== CORRUPTING WRITE to lock_fns->lock_display ===\n"
            printf "new value = %p\n", *(void**) $lock_fns
            bt 25
            printf "==================================================\n\n"
            continue
        end
    end
    continue
end

run
echo \n--- inferior stopped (signal or exit) ---\n
thread apply all bt 15
quit
