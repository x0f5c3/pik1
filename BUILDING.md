# Building serialmux for K1 and Pi/CB1

This project compiles Rust code to two targets using cross-rs and Docker:
- **K1 (Creality)**: `mipsel-unknown-linux-musl` (32-bit MIPS, little-endian)
- **Pi/CB1**: `aarch64-unknown-linux-musl` (64-bit ARM)

## Prerequisites

1. **Rust toolchain** (stable)
   ```shell
   curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
   ```

2. **cross-rs** for Docker-based cross-compilation
   ```shell
   cargo install cross
   ```

3. **Docker** (required by cross for MIPS builds)
   - Windows: Docker Desktop
   - Linux: `docker` + `docker-compose`
   - macOS: Docker Desktop

## Project Structure

```
pik1/
├── Cross.toml           # cross-rs configuration (Docker image specs)
├── .cargo/config.toml   # Cargo configuration (build profiles)
├── Taskfile.yml         # Task automation (build + install)
├── serialmux-rs/        # Rust crate
│   ├── Cargo.toml
│   ├── src/
│   │   ├── main.rs      # CLI and daemon entry point
│   │   ├── daemon.rs    # Event loop
│   │   ├── channel.rs   # Multiplexing channels
│   │   ├── protocol.rs  # Wire protocol
│   │   ├── serial.rs    # Serial I/O
│   │   ├── logging.rs   # Structured logging
│   │   └── bin/windlass_bridge.rs  # Alternative binary
```

## Configuration Files

### Cross.toml
Specifies which Docker images cross uses for each target:
- `mipsel-unknown-linux-musl`: Uses `ghcr.io/cross-rs/mips:latest` (musl libc, static linking)
- `aarch64-unknown-linux-musl`: Uses `ghcr.io/cross-rs/aarch64:latest` (musl libc, static linking)

### .cargo/config.toml
Cargo build configuration:
- Sets release profile: `-O3`, thin LTO, stripped binaries
- Configures cross-compilation runner for both targets

### Taskfile.yml
Automation tasks:

| Task | Purpose |
|------|---------|
| `task rust` | Build both mipsel and aarch64 Rust binaries |
| `task rust-mipsel` | Build K1 binary → `build/serialmux.mipsel` |
| `task rust-aarch64` | Build Pi/CB1 binary → `build/serialmux.aarch64` |
| `task install-k1-rust` | Build + deploy to K1 SSH |
| `task install-pi-rust` | Build + deploy to Pi/CB1 SSH |

## Building

### Option 1: Using Task (Recommended)

Build both binaries:
```shell
task rust
```

Build individually:
```shell
task rust-mipsel   # K1 only (requires Docker)
task rust-aarch64  # Pi/CB1 only (much faster, no Docker)
```

### Option 2: Direct cargo / cross

For K1 (requires Docker on Windows):
```shell
cd serialmux-rs
cross build --release --target mipsel-unknown-linux-musl
ls target/mipsel-unknown-linux-musl/release/serialmux
```

For Pi/CB1:
```shell
cd serialmux-rs
cross build --release --target aarch64-unknown-linux-musl
ls target/aarch64-unknown-linux-musl/release/serialmux
```

## Output Binaries

After building, check:
```shell
ls -lh build/
```

Expected output:
```
-rw-r--r-- 1 user group  ~90K May 12 15:30 serialmux.aarch64
-rw-r--r-- 1 user group ~100K May 12 15:30 serialmux.mipsel
```

Both are statically linked, no dependencies required at runtime.

## Installing on Target Devices

### K1 (Creality)

Using Task (via SSH, assumes K1_DIR and K1_INIT_DIR env vars):
```shell
task install-k1-rust K1_DIR=/usr/data/pik1 K1_INIT_DIR=/etc/init.d
```

Manual deployment:
```bash
scp build/serialmux.mipsel root@<k1-ip>:/usr/data/pik1/serialmux
scp S99pik1 root@<k1-ip>:/etc/init.d/S99pik1
ssh root@<k1-ip> chmod +x /usr/data/pik1/serialmux /etc/init.d/S99pik1
ssh root@<k1-ip> /etc/init.d/S99pik1 start
```

### Pi/CB1 (Debian-based Armbian)

Using Task (requires sudo):
```shell
task install-pi-rust
```

This will:
1. Build the aarch64 binary
2. Install to `/opt/pik1/`
3. Copy and enable the systemd service
4. Reload systemd daemon

Manual deployment:
```bash
scp build/serialmux.aarch64 pi@<cb1-ip>:/tmp/serialmux.aarch64
ssh pi@<cb1-ip> sudo install -m 755 /tmp/serialmux.aarch64 /opt/pik1/serialmux
ssh pi@<cb1-ip> sudo systemctl enable pik1.service
ssh pi@<cb1-ip> sudo systemctl start pik1.service
ssh pi@<cb1-ip> sudo systemctl status pik1.service
```

## Troubleshooting

### Docker not found when building mipsel
**Error**: `docker: command not found`

**Fix**:
- Ensure Docker Desktop is running (Windows/macOS)
- Or install Docker Engine (Linux)
- cross requires Docker for non-native targets

### aarch64 build fails with missing libc
**Error**: `cannot find -lc` or `linking error: undefined reference`

**Fix**: This shouldn't happen as we use musl (static linking). Verify:
```shell
cd serialmux-rs
rustup target add aarch64-unknown-linux-musl
cross build --release --target aarch64-unknown-linux-musl --verbose
```

### Binary won't run on target (segfault or "no such file")
**Likely cause**: Architecture mismatch or glibc vs musl incompatibility

**Verify on target**:
```bash
# On K1
file /usr/data/pik1/serialmux  # Should say: ELF 32-bit LSB, MIPS, mips32

# On Pi/CB1
file /opt/pik1/serialmux       # Should say: ELF 64-bit LSB, ARM aarch64, GNU/Linux
```

## Features

Optional features can be enabled when building:

### Journald logging
```shell
cd serialmux-rs
cross build --release --target aarch64-unknown-linux-musl --features journald
```

### Windlass bridge (alternative protocol)
```shell
cd serialmux-rs
cross build --release --target aarch64-unknown-linux-musl --features windlass --bin windlass-bridge
```

(See `Cargo.toml` for more details.)

## Development Tips

- Use `task rust-aarch64` for fast iteration (no Docker)
- Use `task rust-mipsel` only when ready to test on K1 (slower due to Docker)
- Binaries are ~100K after stripping; check `build/` for latest output
- Run `task clean` to remove all build artifacts
- Run `task distclean` to remove build outputs and any downloaded toolchains

## References

- [cross-rs](https://github.com/cross-rs/cross) — Docker-based cross-compilation
- [Rust Platform Support](https://doc.rust-lang.org/nightly/rustc/platform-support.html)
- [musl libc](https://musl.libc.org/) — Static C library used
- Original [pik1 README](./readme.md) — Hardware and K1/Pi connectivity setup

