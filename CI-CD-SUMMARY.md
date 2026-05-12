# GitHub Actions CI/CD Setup - Summary

This document outlines the automated build and release system set up for the serialmux Rust project.

## What Was Created

### 1. Workflow Files

#### `.github/workflows/release.yml`
**Purpose:** Build and publish release binaries

**Triggers:**
- Git tags (e.g., `git push origin v0.2.0`)
- Manual workflow dispatch (UI button)

**Builds:**
- `serialmux` (main daemon)
- `windlass-bridge` (alternative, feature-gated)
- Both architectures: mipsel (K1), aarch64 (Pi/CB1)

**Outputs:**
- 4 binaries + 4 SHA256 checksums
- CHECKSUMS.txt manifest
- INSTALL.md installation guide
- GitHub Release with all assets
- Auto-generated changelog from commits

**Build time:** ~10-15 minutes (Docker needed for MIPS)

---

#### `.github/workflows/build-test.yml`
**Purpose:** Validate code and builds on every push/PR

**Triggers:**
- Pull requests affecting `serialmux-rs/`, build config, or workflows
- Pushes to `main` or `develop` branches

**Checks:**
- Code formatting (`cargo fmt`)
- Linting (`cargo clippy -D warnings`)
- Debug builds (both architectures)
- Release builds (both architectures)
- Binary size reporting

**Pass/Fail:** Controls whether release workflow can proceed

**Build time:** ~5-8 minutes (no Docker needed, parallel builds)

---

### 2. Configuration Files (Updated)

#### `Cross.toml`
Specifies Docker images for cross-compilation:
- mipsel: `ghcr.io/cross-rs/mips:latest`
- aarch64: `ghcr.io/cross-rs/aarch64:latest`

#### `.cargo/config.toml`
Cargo workspace configuration:
- Release profile: `-O3`, thin LTO, strip
- Cross-compilation targets configured

---

### 3. Documentation Files

#### `RELEASE-QUICK-START.md` ⭐ START HERE
5-minute guide to creating a release:
- How to tag and push
- What gets released
- Quick troubleshooting

#### `CI-CD.md`
Comprehensive CI/CD documentation:
- Detailed workflow descriptions
- Release process explained
- Advanced customization options
- Troubleshooting guide
- Integration with local build tasks

#### `BUILDING.md` (Previously created)
Local development build guide:
- Prerequisites and setup
- Using Task for builds
- Manual cross compilation
- Feature builds (windlass, journald)

---

## Release Workflow (Step by Step)

```
Developer                  GitHub CI                    Release Page
─────────────────────────────────────────────────────────────────────

Code changes
    ↓
git commit
    ↓
git push origin main
    ↓ ──→ BUILD-TEST WORKFLOW ←──
        ✓ Format check
        ✓ Lint check
        ✓ Build all targets
        ✓ Report status
    ↓ (if pass, continue; if fail, stop)
git tag -a v0.2.0
git push origin v0.2.0
    ↓ ──→ RELEASE WORKFLOW ←──
        ✓ Build serialmux (mipsel)
        ✓ Build windlass-bridge (mipsel)
        ✓ Build serialmux (aarch64)
        ✓ Build windlass-bridge (aarch64)
        ✓ Generate SHA256 checksums
        ✓ Create changelog
        ✓ Package assets
        ✓ Create GitHub Release
    ↓ ────────────────────────→ Release page published
        Open https://github.com/ORG/pik1/releases/tag/v0.2.0
        Download binaries
        Verify checksums
        Deploy to K1/Pi
```

---

## Files Included in Each Release

```
📦 Release v0.2.0
├── 📄 README.md                    - Project overview
├── 📄 INSTALL.md                   - Deployment instructions
├── 📄 CHECKSUMS.txt                - Combined manifest + checksums
├── 🔧 setup_pik1.sh                - USB gadget setup (Pi/CB1)
├── 🔧 pik1.service.in              - systemd service template
├── 🔧 S99pik1                      - K1 init script
│
├── 🗜️ Binaries (4 total)
│  ├── serialmux.mipsel             - K1 main daemon
│  ├── serialmux.mipsel.sha256
│  ├── serialmux.aarch64            - Pi/CB1 main daemon
│  ├── serialmux.aarch64.sha256
│  ├── windlass-bridge.mipsel       - K1 alternative (faster)
│  ├── windlass-bridge.mipsel.sha256
│  ├── windlass-bridge.aarch64      - Pi/CB1 alternative (faster)
│  └── windlass-bridge.aarch64.sha256
```

---

## Key Features

### ✅ Automated Building
- Cross-compilation for both MIPS and ARM64
- Release optimizations applied automatically
- Binaries stripped and ready to deploy

### ✅ Checksums & Verification
- SHA256 checksums for every binary
- Combined CHECKSUMS.txt manifest
- Users can verify integrity before deploying

### ✅ Changelog Generation
- Automatically lists commits since last release
- Ready to edit and customize
- Included in GitHub Release body

### ✅ Multi-Binary Releases
- Two implementations per release (serialmux + windlass-bridge)
- Users choose which to deploy
- Both tested in same build pipeline

### ✅ Complete Package
- Binaries + checksums + docs + config files
- Users have everything needed for deployment
- Installation guide included

### ✅ Safety
- Build-Test on every PR/push catches issues early
- Release only triggers on explicit tag or manual dispatch
- No auto-releases on every commit

---

## Quick Commands

### Release a new version

```bash
# Push changes to main
git push origin main
# Wait for build-test to pass

# Create and push tag
git tag -a v0.2.0 -m "Release v0.2.0: [description]"
git push origin v0.2.0

# Monitor in GitHub Actions → Release workflow
# Download from GitHub Releases when complete
```

### Manual release (no tag)

1. Go to GitHub Actions → Release
2. Click "Run workflow"
3. Enter version name
4. Click "Run workflow"

### Download a release locally

```bash
gh release download v0.2.0
sha256sum -c CHECKSUMS.txt
```

---

## Customization

The workflows can be extended to:

### Add GPG signing
```yaml
- name: Sign binaries
  run: |
    gpg --detach-sign serialmux.mipsel
    gpg --detach-sign serialmux.aarch64
```

### Upload to alternative platforms
```yaml
- name: Upload to artifact server
  run: |
    curl -F "file=@serialmux.mipsel" https://artifacts.example.com
```

### Generate detailed changelogs
Replace the simple changelog with conventional commits parsing:
```bash
conventional-changelog -i CHANGELOG.md -s
```

See [CI-CD.md](./CI-CD.md) for more advanced options.

---

## Troubleshooting

| Issue | Solution |
|-------|----------|
| Build fails on MIPS | Docker required; check runner logs |
| Release doesn't trigger | Verify tag format: `v0.2.0`, `v1.0.0-rc1` |
| Checksum mismatch | Re-download; file may have corrupted |
| Need to re-run release | Delete tag, recreate, push again |
| Build Test is slow | Normal for cross-compilation; consider caching |

See [CI-CD.md](./CI-CD.md) **Troubleshooting** section for detailed solutions.

---

## Integration with Existing Tools

| Tool | Integration |
|------|-----------|
| `Task` (Taskfile.yml) | Same build commands; CI uses cross directly |
| `Cross` | CI/CD runs cross; local development uses cross too |
| GitHub Releases | Native; binaries published automatically |
| `gh` CLI | Can download releases locally with `gh release download` |

The CI/CD system uses the exact same build steps as local development, ensuring consistency.

---

## Support Files

- **RELEASE-QUICK-START.md** — Start here for release instructions
- **CI-CD.md** — Full workflow documentation
- **BUILDING.md** — Local build instructions
- **.github/workflows/release.yml** — Release workflow definition
- **.github/workflows/build-test.yml** — PR/push test workflow

---

## Next Steps

1. Read [RELEASE-QUICK-START.md](./RELEASE-QUICK-START.md) for creating your first release
2. Tag a version: `git tag -a v0.1.0 -m "Initial Rust port"`
3. Push tag: `git push origin v0.1.0`
4. Watch the Release workflow build binaries
5. Download from GitHub Releases
6. Deploy to K1 and Pi/CB1

---

## Questions?

- Check workflow logs in GitHub Actions tab
- Review CI-CD.md for detailed documentation
- Test locally with `task rust` before tagging

