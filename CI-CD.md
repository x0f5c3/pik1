# CI/CD Pipeline Guide

This repository includes automated GitHub Actions workflows for building, testing, and releasing binaries for both K1 (mipsel) and Pi/CB1 (aarch64) architectures.

## Workflows

### 1. Build Test (`build-test.yml`)

**Triggers:**
- Push to `main` or `develop` branches
- Pull requests affecting `serialmux-rs/`, `Cross.toml`, or `.cargo/config.toml`

**What it does:**
- ✅ Checks code formatting (`cargo fmt`)
- ✅ Runs linter (`cargo clippy`)
- ✅ Builds both architectures (debug + release)
- ✅ Builds windlass-bridge feature
- ✅ Reports binary sizes

**Status badge:**
Add to README to show build status:
```markdown
[![Build Test](https://github.com/x0f5c3/pik1/actions/workflows/build-test.yml/badge.svg)](https://github.com/x0f5c3/pik1/actions/workflows/build-test.yml)
```

### 2. Release (`release.yml`)

**Triggers:**
- Push a git tag (e.g., `git tag v0.2.0 && git push --tags`)
- Manual workflow dispatch (via GitHub Actions UI)

**What it does:**
- 📦 Builds optimized release binaries for all architectures
- 🔗 Generates SHA256 checksums for each binary
- 📝 Auto-generates changelog from commits since last tag
- 📋 Creates installation guide (INSTALL.md)
- 🔖 Creates GitHub Release with all assets
- ✅ Packages service files and setup scripts

**Release artifacts include:**
```
serialmux.mipsel                    (K1 binary)
serialmux.mipsel.sha256             (checksum)
serialmux.aarch64                   (Pi/CB1 binary)
serialmux.aarch64.sha256            (checksum)
windlass-bridge.mipsel              (K1 alternative)
windlass-bridge.mipsel.sha256       (checksum)
windlass-bridge.aarch64             (Pi/CB1 alternative)
windlass-bridge.aarch64.sha256      (checksum)
CHECKSUMS.txt                       (manifest)
INSTALL.md                          (installation steps)
setup_pik1.sh                       (setup script)
pik1.service.in                     (systemd template)
S99pik1                             (K1 init script)
README.md                           (project readme)
```

## Creating a Release

### Method 1: Using Git Tags (Recommended)

Create a release by pushing a git tag:

```bash
# Create an annotated tag
git tag -a v0.2.0 -m "Release v0.2.0: Add windlass support"

# Push to trigger CI/CD
git push origin v0.2.0

# Or push all tags at once
git push --tags
```

The release will automatically build and publish within 5-10 minutes.

**Tag naming conventions:**
- `v0.2.0` — stable release
- `v0.2.0-rc1` — release candidate (marked as prerelease)
- `v0.2.0-beta1` — beta (marked as prerelease)
- `v0.2.0-alpha1` — alpha (marked as prerelease)

### Method 2: Manual Dispatch

Trigger a release manually via GitHub UI:

1. Go to **Actions** tab
2. Click **Release** workflow
3. Click **Run workflow**
4. Enter version (e.g., `v0.2.0`)
5. Click **Run workflow**

This is useful for:
- Quick hotfix releases
- Re-running failed releases
- Releasing without a git tag

## Release Notes Generation

The workflow automatically generates release notes by:

1. Finding the commit range since the last tag
2. Listing all commits in that range
3. Formatting as:
   ```
   Commit message 1
   Commit message 2
   Commit message 3
   ```

**To improve release notes:**
- Write clear, user-facing commit messages in the main branch
- Group related changes before tagging
- Use conventional commit format (optional):
  ```
  feat: add windlass-bridge support
  fix: correct baudrate handling
  docs: update installation guide
  ```

## Verifying Releases

### Check Release Status

1. Go to **Releases** page
2. Look for your version tag
3. Verify all assets are present (4 binaries + 4 checksums + docs)

### Download and Verify Locally

```bash
# Download release
gh release download v0.2.0

# Verify checksums
sha256sum -c CHECKSUMS.txt

# Expected output:
# serialmux.mipsel: OK
# serialmux.aarch64: OK
# windlass-bridge.mipsel: OK
# windlass-bridge.aarch64: OK
```

### Test Binaries Before Installing

For K1:
```bash
scp serialmux.mipsel root@<k1-ip>:/tmp/test_serialmux
ssh root@<k1-ip> /tmp/test_serialmux --help
```

For Pi/CB1:
```bash
scp serialmux.aarch64 pi@<cb1-ip>:/tmp/test_serialmux
ssh pi@<cb1-ip> /tmp/test_serialmux --help
```

## Troubleshooting

### Release build fails with "Docker not found"

**Cause:** GitHub runner doesn't have Docker for MIPS builds.

**Fix:** The workflow already handles this with `taiki-e/setup-cross-toolchain-action`. If it still fails:
1. Check the workflow logs for the specific error
2. Open an issue with the error message

### Binary size is larger than expected

**Normal after release:** Artifacts may include debug symbols. Check the actual stripped binary:
```bash
ssh root@<k1-ip> ls -lh /usr/data/pik1/serialmux
```

### Checksums don't match

**Cause:** Corrupted download or file was modified.

**Fix:**
1. Re-download from Release page
2. Verify again with `sha256sum -c`
3. If still failing, re-trigger the release workflow

## Advanced: Customizing Releases

### Add Custom Metadata

Edit the release workflow to add build info:

```yaml
- name: Create build info
  run: |
    cat > build-info.txt << EOF
    Build Date: $(date -u)
    Git SHA: ${{ github.sha }}
    Rust Version: $(rustc --version)
    EOF
```

### Sign Releases with GPG

To cryptographically sign releases:

```yaml
- name: Import GPG key
  run: |
    echo "${{ secrets.GPG_PRIVATE_KEY }}" | gpg --import

- name: Sign binaries
  run: |
    gpg --detach-sign serialmux.mipsel
    gpg --detach-sign serialmux.aarch64
```

(Requires GPG key stored in GitHub Secrets)

### Upload to Additional Platforms

To automatically upload releases to other platforms (e.g., artifact repository):

```yaml
- name: Upload to artifact server
  run: |
    curl -u user:pass -F "file=@serialmux.mipsel" https://artifacts.example.com/upload
```

## CI/CD Files Reference

| File | Purpose |
|------|---------|
| `.github/workflows/release.yml` | Main release workflow (tag or manual trigger) |
| `.github/workflows/build-test.yml` | PR and push testing (lint + build) |
| `Cross.toml` | cross-rs configuration (Docker images) |
| `.cargo/config.toml` | Cargo build settings (profiles, targets) |

## Integration with `Taskfile.yml`

The CI/CD pipelines use the same build strategy as the local Taskfile tasks:

```bash
# Local (using Task)
task rust

# CI/CD (using cross directly)
cross build --release --target mipsel-unknown-linux-musl
cross build --release --target aarch64-unknown-linux-musl
```

Both produce identical binaries due to matching profiles and flags.

## Environment Variables

The release workflow uses:

- `CARGO_TERM_COLOR` — Always colorize output
- Standard GitHub Actions variables (`github.ref_name`, `github.sha`, etc.)
- `GITHUB_TOKEN` — Automatically provided for releases

No additional secrets need to be configured.

## Example Workflow: Full Release

```bash
# 1. Make changes and test locally
task rust
./test_locally.sh

# 2. Commit and push to main
git add .
git commit -m "feat: add new protocol support"
git push origin main

# 3. Wait for build-test to pass (check Actions tab)

# 4. Create release tag
git tag -a v0.2.0 -m "Add protocol X support"
git push origin v0.2.0

# 5. GitHub Actions will automatically:
#    - Build all binaries
#    - Generate changelog
#    - Create GitHub Release
#    - Attach all artifacts

# 6. Verify release
gh release view v0.2.0
gh release download v0.2.0
sha256sum -c CHECKSUMS.txt

# 7. Deploy to K1 and Pi/CB1
#    (use INSTALL.md from release)
```

## Support

For issues with CI/CD:
1. Check workflow logs: **Actions** → **Release** → click run → view logs
2. Check build matrix: each architecture logs separately
3. Report issues with full log output

