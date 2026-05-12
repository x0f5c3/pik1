# 🚀 GitHub Actions CI/CD Setup - Complete

Your serialmux Rust project now has a fully automated GitHub Actions CI/CD pipeline. Here's what was created:

## What You Get

### Two Automated Workflows

#### 1️⃣ **Build Test** (`.github/workflows/build-test.yml`)
Runs on every PR and push to catch issues early:
- ✅ Code format check (`cargo fmt`)
- ✅ Linting (`cargo clippy`)
- ✅ Debug builds for mipsel + aarch64
- ✅ Release builds for mipsel + aarch64
- ✅ Binary size reporting

**When it runs:** Pull requests + pushes to main/develop
**Time:** ~5-8 minutes (no Docker needed)

#### 2️⃣ **Release** (`.github/workflows/release.yml`)
Builds and publishes release binaries when you tag:
- 📦 **4 binaries** (2 types × 2 architectures)
  - `serialmux.{mipsel,aarch64}` — Main daemon
  - `windlass-bridge.{mipsel,aarch64}` — Alternative (feature-gated)
- 🔐 **4 SHA256 checksums** for verification
- 📝 **Auto-generated changelog** from commits
- 📋 **INSTALL.md** with deployment steps
- 🔖 **GitHub Release** with all assets
- 📦 **Config files** (setup scripts, systemd service)

**When it runs:** Git tags (e.g., `v0.1.0`) or manual dispatch
**Time:** ~10-15 minutes (includes Docker for MIPS)

---

## Quick Start (5 Minutes)

### 1. Test Locally First
```powershell
task rust
```

### 2. Tag Your Release
```bash
git tag -a v0.1.0 -m "Release v0.1.0: Initial Rust port"
git push origin v0.1.0
```

### 3. Watch the Build
- Go to **Actions** tab on GitHub
- Watch **Release** workflow run
- Download from **Releases** page when complete

### 4. Verify & Deploy
```bash
gh release download v0.1.0
sha256sum -c CHECKSUMS.txt
# Deploy serialmux.mipsel to K1
# Deploy serialmux.aarch64 to Pi/CB1
```

---

## Documentation (Read in Order)

1. **[RELEASE-QUICK-START.md](./RELEASE-QUICK-START.md)** ⭐ START HERE
   - 5-minute release guide
   - Common version patterns
   - Quick troubleshooting

2. **[CI-CD-CHECKLIST.md](./CI-CD-CHECKLIST.md)**
   - Pre-release verification checklist
   - Step-by-step release process
   - First-time setup guide

3. **[CI-CD-SUMMARY.md](./CI-CD-SUMMARY.md)**
   - Overview of what was created
   - Release workflow visualization
   - File structure

4. **[CI-CD.md](./CI-CD.md)**
   - Complete workflow documentation
   - Advanced customization
   - Full troubleshooting

5. **[BUILDING.md](./BUILDING.md)**
   - Local development builds
   - Cross-compilation details
   - Feature builds (windlass, journald)

---

## Release Process Flowchart

```
┌─ Developer commits code
└─ git push origin main
   │
   └─ GitHub Actions: Build-Test workflow
      ├─ ✓ Format check
      ├─ ✓ Lint check
      ├─ ✓ Build all targets
      └─ ✓ Report status
         │
         └─ (if failed, stop here)
            (if passed, proceed)
            │
            └─ Developer creates tag
               git tag -a v0.1.0
               git push origin v0.1.0
               │
               └─ GitHub Actions: Release workflow
                  ├─ Build serialmux (mipsel + aarch64)
                  ├─ Build windlass-bridge (mipsel + aarch64)
                  ├─ Generate SHA256 checksums
                  ├─ Create changelog
                  ├─ Create GitHub Release
                  └─ Publish all assets
                     │
                     └─ User downloads & deploys
                        (see INSTALL.md)
```

---

## Release Contents

Each release includes:

```
📦 v0.1.0
├── 🔧 Binaries (4 total )
│  ├── serialmux.mipsel              (K1)
│  ├── serialmux.aarch64             (Pi/CB1)
│  ├── windlass-bridge.mipsel        (K1 alt.)
│  └── windlass-bridge.aarch64       (Pi/CB1 alt.)
│
├── 🔐 Checksums (4 total)
│  ├── serialmux.mipsel.sha256
│  ├── serialmux.aarch64.sha256
│  ├── windlass-bridge.mipsel.sha256
│  ├── windlass-bridge.aarch64.sha256
│  └── CHECKSUMS.txt                 (manifest)
│
├── 📚 Documentation
│  ├── INSTALL.md                    (steps-by-step deployment)
│  ├── README.md                     (project overview)
│  ├── pik1.service.in               (systemd service)
│  ├── S99pik1                       (K1 init script)
│  └── setup_pik1.sh                 (USB gadget config)
│
└── 📄 Release Notes
   ├── Changelog (auto-generated)
   └── Build info (date, version)
```

---

## File Structure

```
pik1/
├── .github/
│  └── workflows/
│     ├── release.yml               ← Builds on tag push
│     └── build-test.yml            ← Tests on PR/push
│
├── CI-CD-CHECKLIST.md              ← Pre-release checklist
├── CI-CD-SUMMARY.md                ← Overview & flow
├── CI-CD.md                        ← Full documentation
├── RELEASE-QUICK-START.md          ← Quick guide
├── BUILDING.md                     ← Local dev guide
│
├── Cross.toml                      ← Compiler config
├── .cargo/config.toml              ← Cargo settings
├── Taskfile.yml                    ← Local build tasks
│
└── serialmux-rs/
   ├── Cargo.toml                   ← Rust project config
   └── src/
      ├── main.rs                   ← CLI + daemon
      ├── daemon.rs                 ← Event loop
      ├── channel.rs                ← Multiplexing
      └── ...
```

---

## Key Features

### ✅ Fully Automated
- No manual binary building needed
- One command to release: `git push origin v0.1.0`
- Workflow handles everything else

### ✅ Multi-Architecture
- Builds for both K1 (mipsel) and Pi/CB1 (aarch64)
- Each gets both serialmux and windlass-bridge
- All in a single release

### ✅ Quality Assurance
- Build-Test on every commit catches issues
- Checksums verify binary integrity
- Installation guide is ready-to-use

### ✅ Complete Package
- Binaries + checksums + docs + config files
- Users have everything needed
- No separate downloads required

### ✅ Safe Releases
- Explicit versioning (git tags)
- No auto-releases
- Full control over release timing

---

## Example Release Commands

### Create your first release
```bash
# Test locally
task rust

# Create annotated tag
git tag -a v0.1.0 -m "Release v0.1.0: Initial Rust port"

# Push to trigger CI
git push origin v0.1.0

# Wait 10-15 minutes for builds...

# Download and verify
gh release download v0.1.0
sha256sum -c CHECKSUMS.txt
```

### Create a pre-release
```bash
git tag -a v0.1.0-rc1 -m "Release candidate 1"
git push origin v0.1.0-rc1
# (Workflow marks as pre-release automatically)
```

### Manual release (no tag needed)
1. Go to GitHub **Actions** tab
2. Click **Release** workflow
3. Click **Run workflow**
4. Enter version name
5. Click **Run workflow**

---

## What to Do Now

### Immediate (5 min)
- [ ] Read [RELEASE-QUICK-START.md](./RELEASE-QUICK-START.md)
- [ ] Verify local build: `task rust`

### Soon (before first release)
- [ ] Review [CI-CD-CHECKLIST.md](./CI-CD-CHECKLIST.md)
- [ ] Make sure build-test passes
- [ ] Create first tag: `git tag -a v0.1.0 -m "..."`
- [ ] Push tag: `git push origin v0.1.0`

### Later (after first release)
- [ ] Download binaries and test
- [ ] Deploy to K1 and Pi/CB1
- [ ] Gather feedback
- [ ] Plan next release

---

## Troubleshooting Quick Reference

| Problem | Check |
|---------|-------|
| Workflow doesn't start | Tag format: `v*` (e.g., `v0.1.0`)? |
| Build fails | Check Actions logs for error |
| No Release page | Did workflow finish? Go to Actions tab |
| Checksum mismatch | Re-download; file may have corrupted |
| Re-run release | Delete tag, recreate, push again |

See **CI-CD.md** for detailed troubleshooting.

---

## Support Files

| File | Purpose |
|------|---------|
| **RELEASE-QUICK-START.md** | Quick release guide |
| **CI-CD-CHECKLIST.md** | Pre-release verification |
| **CI-CD-SUMMARY.md** | Setup overview |
| **CI-CD.md** | Complete documentation |
| **BUILDING.md** | Local build guide |

---

## Technical Details

### Docker Used
- **mipsel builds:** `ghcr.io/cross-rs/mips:latest`
- **aarch64 builds:** `ghcr.io/cross-rs/aarch64:latest`

### Build Optimization
- Release profile: `-O3`, thin LTO, stripped
- Result: ~100KB binaries (no deps)
- Static linking (musl libc)

### Workflow Tools
- **cross** — Cross-compilation with Docker
- **clap** — CLI argument parsing
- **softprops/action-gh-release** — GitHub Release creation
- **gh CLI** — release downloads (local)

---

## Next Release Checklist

Before every release:
- [ ] Commit all changes to main
- [ ] Build-Test workflow passed ✅
- [ ] Local test: `task rust` ✅
- [ ] Decide version number (e.g., v0.2.0)
- [ ] Create tag: `git tag -a vX.Y.Z -m "..."`
- [ ] Push tag: `git push origin vX.Y.Z`
- [ ] Monitor Actions tab
- [ ] Download and verify when complete

---

## You're All Set! 🎉

Start with [RELEASE-QUICK-START.md](./RELEASE-QUICK-START.md) and you'll be releasing binaries in minutes.

Questions? Check:
1. [RELEASE-QUICK-START.md](./RELEASE-QUICK-START.md) — Quick answers
2. [CI-CD-CHECKLIST.md](./CI-CD-CHECKLIST.md) — Step-by-step
3. [CI-CD.md](./CI-CD.md) — Deep dive
4. GitHub Actions tab — Workflow logs

Happy releasing! 🚀

