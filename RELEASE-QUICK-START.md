# Quick Start: Making a Release

## Prerequisites

- Write access to the repository
- Git command-line tool
- `gh` CLI (optional, for downloading releases)

## Quick Release (5 minutes)

### Step 1: Ensure all tests pass

Push to main/develop and wait for the **Build Test** workflow to complete. Check the badge here:
```
https://github.com/x0f5c3/pik1/actions/workflows/build-test.yml
```

### Step 2: Create and push a tag

```bash
# Decide on version (semantic versioning: v0.2.0, v1.0.0-rc1, etc.)
VERSION="v0.2.0"

# Create annotated tag with description
git tag -a $VERSION -m "Release $VERSION: [your description here]"

# Push to GitHub (triggers Release workflow)
git push origin $VERSION
```

### Step 3: Wait for the release

- Go to **Actions** tab
- Watch the **Release** workflow run (usually 5-10 minutes)
- Status appears in **Releases** section when complete

### Step 4: Verify and deploy

```bash
# View release details
gh release view v0.2.0

# Download binaries
gh release download v0.2.0

# Verify checksums
sha256sum -c CHECKSUMS.txt

# Deploy to K1
scp serialmux.mipsel root@<k1-ip>:/usr/data/pik1/serialmux

# Deploy to Pi/CB1
scp serialmux.aarch64 pi@<cb1-ip>:/tmp/serialmux.aarch64
ssh pi@<cb1-ip> sudo install -m 755 /tmp/serialmux.aarch64 /opt/pik1/serialmux
```

## Common Version Patterns

| Pattern | Use Case | Example |
|---------|----------|---------|
| `v0.1.0` | Stable release | First release |
| `v0.2.0` | Patch/minor update | New feature or fix |
| `v1.0.0` | Major milestone | Production ready |
| `v0.2.0-rc1` | Release candidate | Pre-release testing |
| `v0.2.0-beta1` | Beta | Early testing (marked as prerelease) |

## What gets released

✅ **Two binaries per architecture:**
- `serialmux.{mipsel,aarch64}` — Main daemon
- `windlass-bridge.{mipsel,aarch64}` — Alternative (if you enable it)

✅ **Checksums for verification:**
- `serialmux.mipsel.sha256`
- `serialmux.aarch64.sha256`
- `windlass-bridge.*.sha256`
- `CHECKSUMS.txt` (manifest)

✅ **Installation & setup files:**
- `INSTALL.md` — Step-by-step install guide
- `setup_pik1.sh` — USB gadget configuration
- `pik1.service.in` — systemd service template
- `S99pik1` — K1 init script
- `README.md` — Project readme

✅ **Release notes:**
- Auto-generated from commits since last release
- Lists all changes with commit messages

## Troubleshooting Quick Fixes

### "Release failed with build error"

1. Click the **Release** workflow run in Actions
2. Scroll to the failed job
3. Check the error message
4. Fix the issue locally:
   ```bash
   task rust-mipsel   # or rust-aarch64
   ```
5. Re-push the tag:
   ```bash
   git tag -d v0.2.0           # delete local
   git push origin :refs/tags/v0.2.0  # delete remote
   git tag -a v0.2.0 -m "..."  # recreate
   git push origin v0.2.0      # re-push
   ```

### "Checksum verification failed"

The binary didn't download correctly. Re-download:
```bash
rm serialmux.*
gh release download v0.2.0
sha256sum -c CHECKSUMS.txt
```

### "Want to release without pushing a tag"

Use the manual dispatch option:

1. Go to **Actions** → **Release**
2. Click **Run workflow**
3. Enter version name (e.g., `v0.2.0`)
4. Click **Run workflow**

No git tag needed!

## Next Steps

- See [CI-CD.md](./CI-CD.md) for detailed workflow documentation
- See [BUILDING.md](./BUILDING.md) for local build instructions
- Check release page: https://github.com/x0f5c3/pik1/releases

## Tips

- Keep commit messages clear and user-focused
- Tag after a successful build-test run
- Test binaries before deploying to K1/Pi
- Keep releases incremental (v0.1.0 → v0.2.0 → v1.0.0)

