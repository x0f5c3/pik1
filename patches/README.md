# Smart-proxy: windlass + anchor improvement opportunities

This document describes what else the smart-proxy (`windlass-bridge --smart`)
can gain from improvements in the `windlass` and `anchor` upstream crates,
and which copies of upstream code exist only because of `pub(crate)` barriers.

The two patch files in this directory fully address the visibility gaps:

| Patch file | Crate | What it unlocks |
|---|---|---|
| `windlass-expose-transport.patch` | `x0f5c3/windlass` | Remove 400-line `mcu_transport.rs` copy |
| `anchor-export-config.patch` | `x0f5c3/anchor` | Clean top-level imports in `smart_host.rs` |

---

## 1. The main blocker: `windlass::Transport` is `pub(crate)`

### What is copied today

`serialmux-rs/src/windlass/mcu_transport.rs` (~400 lines) is a near-verbatim
copy of `windlass/src/transport.rs`.  The comment at the top of that file
explains this explicitly.  The copy includes:

- `LowlevelReader` — byte-by-byte frame parser (sync, length, seq, CRC-16)
- `LowlevelWriter` — async frame writer
- `TransportState` — ACK/NAK + retransmit state machine (RFC 6298 RTT)
- `encode_frame` — CRC + seq framer
- `encode_vlq` / `parse_vlq` — VLQ integer codec

### Why it exists

`windlass::Transport` and `windlass::TransportReceiver` are `pub(crate)`.
`windlass::encoding::encode_vlq_int` and `parse_vlq_int` are also `pub(crate)`.
These are the only two things preventing us from using windlass directly.

### What the patch does

`patches/windlass-expose-transport.patch` makes all five symbols public and
re-exports them at the crate root.  Applying it lets us:

1. **Delete `mcu_transport.rs` entirely** (except `fetch_dictionary`).
2. Replace `McuTransport` with `windlass::Transport` everywhere.
3. Replace `McuPayloadReceiver` with `windlass::TransportReceiver`.
4. Replace `encode_vlq` / `parse_vlq` with `windlass::encode_vlq_int` /
   `windlass::parse_vlq_int`.

The `fetch_dictionary` function itself stays (it contains real logic: the
`identify`/`identify_response` exchange loop), but it shrinks from ~70 lines
to ~30 because it no longer has to build `identify` payloads from scratch —
it uses `windlass::encode_vlq_int` directly.

---

## 2. Dictionary bytes vs. parsed Dictionary

### Current situation

`McuConnection::connect` (the high-level windlass API) fetches and **parses**
the MCU dictionary.  After it returns, the raw compressed bytes are gone —
only the parsed `Dictionary` struct survives.

The smart proxy needs the **raw compressed bytes** (to forward them over the
tunnel to the host so it can answer `identify` locally).  That is why we
call `McuTransport::connect` + `fetch_dictionary` instead of
`McuConnection::connect`.

### Optional future windlass improvement

Adding `McuConnection::raw_dictionary_bytes() -> &[u8]` would allow using
the high-level API for both the dictionary fetch and the relay phase.  The
method would cache the compressed bytes alongside the parsed `Dictionary`.
This is not strictly required (our thin `fetch_dictionary` wrapper works
fine), but it would give the high-level API a complete picture and remove
the need to use the low-level `Transport` at all from the exporter.

---

## 3. Anchor: `Config`, `Readable`, `Writable` behind `#[doc(hidden)]`

### Current situation

`smart_host.rs` currently uses:
```rust
use anchor::transport::Config;
use anchor::encoding::{ReadError, Readable};
use anchor::encoding::Writable as _;
```

These all work (the modules are `pub mod`, just `#[doc(hidden)]`), but the
import paths are non-obvious and invisible in rustdoc.

### What the patch does

`patches/anchor-export-config.patch` removes `#[doc(hidden)]` from the
public modules and adds `pub use` re-exports at the crate root:

```rust
use anchor::{Config, ReadError, Readable, Writable};
```

This is a documentation and ergonomics improvement only; no logic changes.

---

## 4. Already-working improvement applied in this commit

`smart_host.rs` previously imported `encode_vlq` from our local
`mcu_transport` module:

```rust
use crate::windlass::mcu_transport::encode_vlq;
// ...
encode_vlq(&mut resp, 0);      // cmd = 0
encode_vlq(&mut resp, offset);
```

`anchor::encoding::Writable` is already publicly accessible and `Vec<u8>`
already implements `anchor::OutputBuffer` (under the `std` feature).  So the
local copy is **completely unnecessary today** — the trait just needed to be
brought into scope.  This commit makes that change:

```rust
use anchor::encoding::Writable as _;
// ...
(0u32).write(&mut resp);    // cmd = 0
offset.write(&mut resp);
```

`encode_vlq` and `parse_vlq` in `mcu_transport.rs` are now private (`fn`,
not `pub(crate) fn`) and carry doc comments explaining that they are
temporary copies pending the windlass patch.

---

## 5. Summary of all improvements

| # | Status | Description |
|---|---|---|
| 1 | **Done in this commit** | `smart_host.rs` uses `anchor::Writable` instead of local `encode_vlq` copy |
| 2 | **Patch provided** | `windlass`: expose `Transport`, `TransportReceiver`, `encode_vlq_int`, `parse_vlq_int` |
| 3 | **Patch provided** | `anchor`: promote `Config`, `Readable`, `Writable`, `ReadError` to top-level exports |
| 4 | Future (optional) | `windlass`: add `McuConnection::raw_dictionary_bytes()` to avoid needing low-level Transport at all |
| 5 | Future (optional) | `anchor`: add `Config::dispatch_raw` hook for unrecognised commands to avoid partial VLQ parse in dispatch |
