/// windlass-bridge: native Klipper transport relay (opt-in feature).
///
/// Enable with: `cargo build --features windlass`
///
/// This produces a second binary (`windlass-bridge`) that replaces the
/// `serialmux` daemon on both the K1 exporter and the Pi host.  It is
/// **not** compatible with the C/Python serialmux daemon — users who need
/// TCP channel tunnelling or grumpyscreen support must continue using the
/// standard `serialmux` binary.
#[cfg(feature = "windlass")]
pub mod windlass;

/// Structured logging initialisation shared by both binaries.
pub mod logging;
