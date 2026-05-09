# Smart-proxy: remaining upstream follow-up patches

The previously documented visibility changes have landed in the upstream
`windlass` and `anchor` repositories. `serialmux-rs` now uses those public APIs
directly.

The two remaining upstream improvements tracked here are:

| Patch file | Crate | What it unlocks |
|---|---|---|
| `windlass-raw-dictionary-bytes.patch` | `x0f5c3/windlass` | Use `McuConnection::connect` on the smart exporter |
| `anchor-dispatch-raw.patch` | `x0f5c3/anchor` | Forward non-`identify` commands without re-encoding the command ID |

---

## 1. `windlass`: retain raw dictionary bytes

### Current situation

`serialmux-rs/src/windlass/smart_exporter.rs` already uses the public
`windlass::Transport` and VLQ helpers. The only remaining local helper is
`serialmux-rs/src/windlass/mcu_transport.rs::fetch_dictionary`, because
`McuConnection::connect` keeps only the parsed dictionary and discards the raw
compressed bytes.

The smart host needs those raw bytes so it can answer `identify` locally.

### What the patch does

`patches/windlass-raw-dictionary-bytes.patch` stores the compressed
`identify_response.data` bytes on `McuConnection` and adds:

```rust
pub fn raw_dictionary_bytes(&self) -> &[u8]
```

That lets the smart exporter switch to the high-level connection path and drop
the remaining dictionary bootstrap helper.

---

## 2. `anchor`: add `Config::dispatch_raw`

### Current situation

`serialmux-rs/src/windlass/smart_host.rs` already uses the root-level `anchor`
re-exports:
```rust
use anchor::{Config, ReadError, Readable, Writable as _};
```

However, proxying unknown commands still requires reading the command ID and
rebuilding it into a new payload before forwarding. That is only because
`anchor::Transport` currently calls `Config::dispatch(cmd, ...)` after decoding
the leading VLQ command ID internally.

### What the patch does

`patches/anchor-dispatch-raw.patch` adds:

```rust
fn dispatch_raw<'c>(frame: &mut &[u8], context: &mut Self::Context<'c>)
```

with a default implementation that preserves today's behavior by decoding the
command ID and delegating to `dispatch`.

With that hook available, the smart host can inspect `identify` locally and
forward every other command as raw bytes.

---

## 3. Local code status after the upstream visibility changes

The current `serialmux-rs` tree is already aligned to the upstream public APIs:

1. `smart_exporter.rs` uses `windlass::Transport`.
2. `mcu_transport.rs` is now only a thin dictionary bootstrap helper.
3. `smart_host.rs` uses the root-level `anchor::{Config, ReadError, Readable, Writable}` exports.

The patch files in this directory cover only the two remaining optional
upstream improvements above.
