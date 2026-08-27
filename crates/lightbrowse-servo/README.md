# lightbrowse-servo — PoC (blocked)

Attempt to add a Servo (Rust-native) backend as tier-1.5 (JS rendering with
~50-150MB instead of Chromium's ~450MB).

## Status: BLOCKED — Servo 0.4/0.5 dependency chain broken (2026-08-27)

Tried, in order:

| Attempt | Result |
|---|---|
| servo 0.5.0 + `no-wgl` | ❌ `mozangle 0.6` panics on non-Windows when `egl` feature is on (servo's `no-wgl` enables it) |
| servo 0.4.0 + `no-wgl` | ❌ same panic in `mozangle 0.5.5` |
| servo 0.4.0 + patched mozangle (vendor/, panic removed) | ❌ `p256`/`p384` `0.14.0-rc.14` require `elliptic_curve::WnafSize`, which **does not exist** in `elliptic-curve 0.14.1` (grep whole crate — absent). Fails on rustc 1.95 and 1.98. |

`servo-script 0.4.0` pins `p256 = "=0.14.0-rc.14"` — can't upgrade to 0.14.0 stable.
(servo-fetch has the same lock; their CI presumably benefits from a cached
older elliptic-curve or is currently broken too.)

## What's ready for when Servo fixes this
- `crates/lightbrowse-servo` scaffold + `examples/fetch.rs` (navigate -> JS eval -> DOM)
- embed pattern learned from servo-fetch: ServoBuilder + SoftwareRenderingContext (no GPU) + WebView + spin_event_loop + `document.readyState` polling
- user-space build deps extracted to /tmp/servo-deps (freetype/harfbuzz/fontconfig .pc + headers, no root needed)
- `[patch.crates-io] mozangle` in workspace root (vendor/)

## Re-check trigger
- servo 0.5.x+ release that fixes mozangle non-Windows EGL
- servo-script moving off `p256 =0.14.0-rc.14` / RustCrypto RC chain
- or: upstream servo-fetch confirms a working lockfile
