# GitHub CI/CD Setup - Verification Checklist

✅ All components have been created. Use this checklist to verify everything is in place before creating your first release.

## Files Created

### Workflows (2 files)
- ✅ `.github/workflows/release.yml` (11.4 KB)
  - Builds on git tags and manual dispatch
  - Creates binaries for mipsel + aarch64
  - Generates changelog and checksums
  - Publishes GitHub Release

- ✅ `.github/workflows/build-test.yml` (2.7 KB)
  - Runs on PR + pushes to main/develop
  - Checks formatting, linting, builds
  - Prevents broken builds from reaching release

### Configuration (2 files already present)
- ✅ `Cross.toml` (updated)
  - Docker images for mipsel and aarch64

- ✅ `.cargo/config.toml` (created)
  - Release profile optimization
  - Cargo workspace settings

### Documentation (4 files)
- ✅ `RELEASE-QUICK-START.md` (3.8 KB) — **START HERE**
  - 5-minute guide to creating releases
  - Common version patterns
  - Quick troubleshooting

- ✅ `CI-CD.md` (8.1 KB)
  - Complete workflow documentation
  - Advanced customization options
  - Full troubleshooting guide

- ✅ `CI-CD-SUMMARY.md` (8.5 KB)
  - Overview of what was created
  - Release workflow visualization
  - Integration points

- ✅ `BUILDING.md` (6.4 KB) — Already exists
  - Local development build guide
  - Feature builds documentation

---

## Pre-Release Setup

Before creating your first release, complete these steps:

### 1. ✅ Repository Setup

```bash
# Verify you're in the right repo
cd H:\Projects\x0f5c3\pik1
git remote -v  # Should show your GitHub repo

# Verify main branch is up to date
git status
git branch -v
```

### 2. ✅ Code Review

- [ ] All commits are on `main` branch
- [ ] `build-test` workflow passed (check Actions tab)
- [ ] No uncommitted changes
- [ ] Version bump (if applicable) is committed

### 3. ✅ Local Build Test

```bash
# Test local build before releasing
task rust

# Verify binaries exist
ls -lh build/serialmux.*
ls -lh build/windlass-bridge.*
```

### 4. ✅ Documentation Review

- [ ] Read [RELEASE-QUICK-START.md](./RELEASE-QUICK-START.md)
- [ ] Understand release version scheme
- [ ] Know your version number (e.g., v0.1.0)

---

## Creating Your First Release

### Step 1: Create Tag

```bash
# Set your version
$VERSION = "v0.1.0"  # or v0.2.0-rc1, etc.
$MSG = "Release $VERSION: [your description]"

# Create annotated tag
git tag -a $VERSION -m $MSG

# Verify tag
git tag -v $VERSION
```

### Step 2: Push Tag

```bash
# Push tag to GitHub (triggers Release workflow)
git push origin $VERSION

# Or push all tags at once
git push --tags
```

### Step 3: Monitor Build

1. Go to GitHub **Actions** tab
2. Click **Release** workflow
3. Watch progress (usually 5-15 minutes)
4. Check logs if any step fails

### Step 4: Verify Release

```bash
# Download and verify
gh release download $VERSION
sha256sum -c CHECKSUMS.txt

# Expected output: all OK
# serialmux.mipsel: OK
# serialmux.aarch64: OK
# windlass-bridge.mipsel: OK
# windlass-bridge.aarch64: OK
```

---

## What to Expect

### ✅ Build-Test Workflow (on push to main)
- **Duration:** 5-8 minutes
- **Runs:** Format check, lint, build all targets
- **Result:** Pass/Fail badge visible on commit
- **Next:** You can safely tag for release

### ✅ Release Workflow (on tag push)
- **Duration:** 10-15 minutes (MIPS needs Docker)
- **Builds:** 4 binaries + 4 checksums
- **Includes:** Changelog, installation guide, config files
- **Output:** GitHub Release page with all assets

---

## Quick Troubleshooting

### Workflow doesn't trigger after tag push
- **Check:** Did the tag format match `v*` (e.g., `v0.1.0`)?
- **Check:** Are you pushing to the correct repo?
- **Fix:** Double-check tag exists: `git tag -l | grep v0.1.0`

### Build succeeded but no Release
- **Check:** Did GitHub Actions finish? (go to Actions tab)
- **Check:** Is Release workflow in the list of successful jobs?
- **Check:** Go to Releases tab and refresh

### Binaries are huge or missing
- **Size is normal:** ~100KB each (musl-linked, stripped)
- **Missing:** Check Release workflow logs for build errors
- **Re-run:** Delete tag and recreate if build failed

### Need to re-release (forgot something)
```bash
# Delete local tag
git tag -d v0.1.0

# Delete remote tag
git push origin :refs/tags/v0.1.0

# Recreate and push
git tag -a v0.1.0 -m "Release v0.1.0"
git push origin v0.1.0
```

---

## After First Release

### ✅ Verify Everything Works
- [ ] Download binaries from Release page
- [ ] Verify checksums match
- [ ] Test binary runs: `./serialmux.aarch64 --help`
- [ ] Deploy to test K1/Pi instance

### ✅ Document Release Process
- [ ] Save release notes (copy from GitHub)
- [ ] Note any issues encountered
- [ ] Update team on release

### ✅ Plan Next Release
- [ ] Commit regularly to main
- [ ] Run `build-test` workflow before tagging
- [ ] Use semantic versioning (v0.1.0 → v0.2.0 → v1.0.0)

---

## Tips for Success

1. **Test locally first**
   ```bash
   task rust  # Before creating tag
   ```

2. **Use clear commit messages**
   - "feat: add windlass support"
   - "fix: correct baudrate handling"
   - Makes changelog more readable

3. **Keep releases incremental**
   - v0.1.0 → v0.1.1 (patch)
   - v0.1.0 → v0.2.0 (minor)
   - v0.1.0 → v1.0.0 (major)

4. **Tag after CI passes**
   - Wait for build-test ✅ on main
   - Then create and push tag
   - Ensures good release

5. **Mark pre-releases**
   - Add `-rc1`, `-beta1`, `-alpha1` suffix
   - Workflow marks these as pre-releases automatically

---

## Command Reference

```bash
# Local testing
task rust                           # Build locally (uses cross)
task rust-mipsel                    # Build K1 only
task rust-aarch64                   # Build Pi/CB1 only (fast)

# Creating releases
git tag -a v0.2.0 -m "Release v0.2.0"  # Create tag
git push origin v0.2.0              # Push (triggers CI)
git tag -l                          # List all tags
git tag -d v0.2.0                   # Delete local tag
git push origin :refs/tags/v0.2.0   # Delete remote tag

# Downloading releases
gh release download v0.2.0          # Download binaries
gh release view v0.2.0              # View release details
gh release list                     # List all releases

# Verification
sha256sum -c CHECKSUMS.txt          # Verify checksums
./serialmux.aarch64 --help          # Test binary
```

---

## Support

### Documentation
- **Quick Start:** [RELEASE-QUICK-START.md](./RELEASE-QUICK-START.md)
- **Full Guide:** [CI-CD.md](./CI-CD.md)
- **Building:** [BUILDING.md](./BUILDING.md)

### Workflow Files
- `.github/workflows/release.yml` — Release workflow
- `.github/workflows/build-test.yml` — Testing workflow

### GitHub Resources
- Actions Tab: `https://github.com/x0f5c3/pik1/actions`
- Releases Page: `https://github.com/x0f5c3/pik1/releases`

---

## Ready to Release?

1. ✅ Read [RELEASE-QUICK-START.md](./RELEASE-QUICK-START.md)
2. ✅ Test locally: `task rust`
3. ✅ Create tag: `git tag -a v0.1.0 -m "..."`
4. ✅ Push tag: `git push origin v0.1.0`
5. ✅ Monitor: Go to Actions tab
6. ✅ Deploy: Download from Releases page

Good luck! 🚀

