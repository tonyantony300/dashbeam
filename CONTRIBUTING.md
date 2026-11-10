# Contributing to DashBeam

Thank you for your interest in contributing. This guide covers local setup and how to submit changes.

## Development setup

**Prerequisites:** Rust 1.91+, Node.js 20.19+ or 22.12+ (recommended: 24 — see `.nvmrc`), pnpm 10+

```bash
git clone https://github.com/tonyantony300/dashbeam.git
cd dashbeam
pnpm install
```

### Desktop

```bash
pnpm tauri dev
```

Production build (skips installer/bundle packaging):

```bash
pnpm tauri build --no-bundle
```

### Web app setup

Use the web target to run the app in a browser during development.

Start the web dev server:

```bash
pnpm build:wasm   
pnpm dev:web
```

Open `http://localhost:3000/`.

Production web build:

```bash
pnpm build:web
pnpm preview:web
```

### Android

```bash
pnpm android:dev
```

### iOS

A Mac is required to build the iOS app. You do not need to have a physical iOS device to build and test the project, however some features are not available on the simulator. 

First you should install Xcode 27. You can do this from the Mac App Store, though it is often a pain and I would recommend installing from https://developer.apple.com/. There is also a helpful xcode version management tool: https://github.com/XcodesOrg/xcodes.

Once you have xcode installed you should "select" it with "sudo xcode-select -s /Applications/Xcode.app/" or substitute the path you installed to. This basically sets the version of Xcode that is invoked by other tools.

I would recommend using Nix to install the rest of the project dependencies, specific instructions for iOS are in the section below.

### Nix / NixOS

A flake dev shell provides the pinned Rust toolchain (with the `wasm32-unknown-unknown` target), Node, pnpm, and the full WebKitGTK stack `tauri dev` needs — no system packages required.

```bash
nix develop      # or `direnv allow`, which uses the checked-in .envrc
pnpm install
pnpm tauri dev
```

The shell reads `rust-toolchain.toml`, so bumping the pin there is enough — the flake follows it.

Notes:

- **`programs.nix-ld.enable = true;`** is required on NixOS. pnpm downloads prebuilt binaries (Biome, Rollup, esbuild) that expect a standard dynamic loader; without nix-ld they fail with "no such file or directory" on an executable that plainly exists.
- **`wasm-bindgen-cli` must match `wasm-bindgen` in `wasm-bridge/Cargo.lock` exactly.** The shell installs the matching versioned nixpkgs package (e.g. `wasm-bindgen-cli_0_2_126`); if nixpkgs doesn't have that version, it warns on entry and prints the `cargo install` line that fixes it.
- **Android is not covered** by the shell — the SDK/NDK are still on you.
- **iOS (macOS only) uses a separate shell**, `nix develop .#ios`. It adds the iOS Rust targets, XcodeGen and CocoaPods, and builds with Xcode's toolchain rather than nix's, so Xcode must be installed. Run `pnpm tauri ios init` once to generate `src-tauri/gen/apple` from `src-tauri/ios/project.yml`, then `pnpm tauri ios dev --open` and Run from Xcode — Xcode's Rust build phase enters the same shell itself. Physical devices also need `APPLE_DEVELOPMENT_TEAM` set to your team ID.
- **Bundling** (`pnpm tauri build` with installers) is not supported on NixOS; use `--no-bundle`.
- The shell sets `WEBKIT_DISABLE_DMABUF_RENDERER=1`, which avoids a blank webview on NVIDIA and older Mesa. Drop it from `flake.nix` if your setup doesn't need it.

### Project layout

| Path | Purpose |
|------|---------|
| `frontend/` | React UI (Tauri + web targets) |
| `src-tauri/` | Tauri desktop/Android shell |
| `engine/` | Rust P2P transfer engine |
| `www/` | Public Next.js site (planned) |

## Testing

To exercise transfers on a single machine, install the [Sendme CLI](https://www.iroh.computer/sendme) and share files locally — traffic stays on your device.

Engine E2E tests:

```bash
cargo test --manifest-path engine/Cargo.toml
```

## Commit messages

Use [Conventional Commits](https://www.conventionalcommits.org/). Format:

```text
type(scope): short description
```

Examples:

```text
fix(android): invalid sharesheet info
feat(discovery): optional DNS origin support
chore(nix): add flake and .envrc
docs: update contributing guide
```

Common types: `feat`, `fix`, `docs`, `refactor`, `perf`, `test`, `build`, `ci`, `chore`. Scope is optional (`android`, `tls`, `ui`, …).

After `pnpm install`, Husky installs a `commit-msg` hook that runs [commitlint](https://commitlint.js.org/). Invalid messages are rejected locally. Prefer fixing the message over `--no-verify`.

**PR titles use the same format** and are enforced in CI (`Lint PR title`). PRs are squash-merged using the PR title as the commit on `main`, so the title is what lands in history and release notes.

## Pull requests

1. Search [existing issues](https://github.com/tonyantony300/dashbeam/issues) before opening a new one.
2. For bugs, use the [bug report template](.github/ISSUE_TEMPLATE/report-bug.md).
3. Use a conventional PR title (required — CI will fail otherwise), for example `fix(android): invalid sharesheet info`.
4. Run checks before opening a PR:

```bash
pnpm lint
pnpm format
```

1. Fill out the [pull request template](.github/PULL_REQUEST_TEMPLATE.md).
**Lockfiles:** Do not commit lockfile-only changes. CI rejects PRs that modify `pnpm-lock.yaml` or `Cargo.lock` without a corresponding manifest change.

```bash
git checkout origin/main -- pnpm-lock.yaml src-tauri/Cargo.lock engine/Cargo.lock
```

## Getting help

- [Discord](https://discord.gg/xwb7z22Eve)
- [GitHub Issues](https://github.com/tonyantony300/dashbeam/issues)

## Code of conduct

Participation is governed by our [Code of Conduct](CODE_OF_CONDUCT.md). By
contributing, you agree to uphold it.

Security vulnerabilities should be reported privately — see [SECURITY.md](SECURITY.md).
