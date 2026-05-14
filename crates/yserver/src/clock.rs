//! Monotonic millisecond timestamp shared between the libinput thread
//! and the KMS backend.
//!
//! X11 event timestamps are 32-bit milliseconds; clients (notably
//! window managers like marco) compare them against the server's own
//! notion of "current time" obtained via `XGetServerTime`. Using
//! `SystemTime::now() - UNIX_EPOCH` for input events produced
//! wall-clock-mod-2^32 values ~7.8 days ahead of `ServerState::timestamp_now`'s
//! since-server-start baseline, tripping marco's
//! `"buggy client sending inaccurate timestamps"` workaround on every
//! event with a non-zero timestamp.
//!
//! The baseline is whatever `Instant` `server_time_ms` first observes.
//! Across `ServerState::timestamp_now` (millis-since-its-own-start) and
//! this function (millis-since-first-call) there is a sub-millisecond
//! skew at startup; both are monotonic from a point near process start,
//! which is what timestamp consumers actually rely on.

use std::{sync::LazyLock, time::Instant};

static START: LazyLock<Instant> = LazyLock::new(Instant::now);

#[must_use]
pub fn server_time_ms() -> u32 {
    #[allow(clippy::cast_possible_truncation)]
    let ms = START.elapsed().as_millis() as u32;
    ms
}
