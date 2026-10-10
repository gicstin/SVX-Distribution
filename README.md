<div align="center">

# SVX

**Install, update and launch SVX with the SVX Patcher.**

[![Latest patcher](https://img.shields.io/github/v/release/gicstin/SVX-Distribution?label=patcher&style=flat-square&color=4682f0)](https://github.com/gicstin/SVX-Distribution/releases/latest)
[![Downloads](https://img.shields.io/github/downloads/gicstin/SVX-Distribution/total?style=flat-square&color=2f3640)](https://github.com/gicstin/SVX-Distribution/releases)
![Platforms](https://img.shields.io/badge/platforms-Windows%20%7C%20Linux-2f3640?style=flat-square)

<br>

[![Download for Windows](https://img.shields.io/badge/Download-Windows%20x64-0078D6?style=for-the-badge&logo=data:image/svg%2Bxml;base64,PHN2ZyB4bWxucz0iaHR0cDovL3d3dy53My5vcmcvMjAwMC9zdmciIHZpZXdCb3g9IjAgMCAyNCAyNCI+PHBhdGggZmlsbD0id2hpdGUiIGQ9Ik0wIDBoMTEuNHYxMS40SDB6TTEyLjYgMEgyNHYxMS40SDEyLjZ6TTAgMTIuNmgxMS40VjI0SDB6TTEyLjYgMTIuNkgyNFYyNEgxMi42eiIvPjwvc3ZnPg==)](https://github.com/gicstin/SVX-Distribution/releases/latest/download/SVXPatcher.exe)
&nbsp;
[![Download for Linux](https://img.shields.io/badge/Download-Linux%20x64-E95420?style=for-the-badge&logo=linux&logoColor=white)](https://github.com/gicstin/SVX-Distribution/releases/latest/download/svx-patcher)

<sub>Single executable · no installer · no account · no admin rights</sub>

</div>

---

## Getting started

| | Windows | Linux |
|---|---|---|
| **Run** | Open `SVXPatcher.exe` | `chmod +x svx-patcher && ./svx-patcher` |
| **Install** | **Install and Play** | **Install and Play** |
| **Default folder** | `%USERPROFILE%\Games\SVX` | `~/Games/SVX` |

The patcher is not code-signed yet. On first launch, Windows SmartScreen may show *Windows protected your PC*. Choose **More info → Run anyway**.

## Features

- **Delta updates.** Only changed files are downloaded, and interrupted downloads resume.
- **Repair.** Detects and restores missing or damaged game files.
- **Version control.** Install any published version, or restore the previous one in one click.
- **Safe by design.** Every download is signature-checked before anything is replaced. A failed or interrupted update never leaves a broken install.
- **Your data stays yours.** `Content`, `User`, `Cache` and `logs` are never modified.
- **Existing installs.** Point the patcher at a portable SVX folder and it takes over without touching your library.

The game folder stays portable: `SVX.exe` / `SVX.x86_64` can still be launched directly.

## System requirements

| | Windows | Linux |
|---|---|---|
| OS | Windows 10 or 11, 64-bit | x86-64, glibc 2.28+ (Ubuntu 20.04, Debian 10, Fedora 29 or newer) |
| Graphics | OpenGL 2.1 or newer | X11 or Wayland, OpenGL 2.1 or newer |
| VR (optional) | OpenXR runtime: SteamVR, Meta/Oculus | OpenXR runtime: SteamVR, Monado, WiVRn |

## Troubleshooting

| Problem | Fix |
|---|---|
| Game does not start | Open the patcher and click **Repair**. |
| "SVX is running" while updating | Close the game; the update continues automatically. |
| Cannot write to the folder | Choose a folder inside your home directory. |
| "Release check failed" | Nothing was changed. Try again later; if it persists, report it. |
| Anything else | Open an issue and attach `logs/latest.log` from the game folder. |

---

<details>
<summary><b>For developers</b></summary>
<br>

| Crate | Purpose |
|---|---|
| `crates/svx-patcher` | Patcher GUI (egui) |
| `crates/svxdist` | Metadata, signatures, downloads, install transactions |
| `crates/svx-release` | Keys, manifests, packs, signed index |
| `crates/testkit` | Test server, fixture game, end-to-end tests |

Open `SVX-Distribution.sln` in Rider; shared run configurations cover every task below.

| Task | Command |
|---|---|
| Build | `tools/cargo.sh build --workspace` |
| Test | `tools/cargo.sh test --workspace` |
| Smoke test (Linux + Wine) | `tools/smoke.sh` |
| Test patchers | `tools/build.sh --test` |
| Release patchers | `tools/build.sh` |

Requires rustup, `x86_64-w64-mingw32-gcc`, zig 0.15 with `cargo-zigbuild`, and optionally Wine.

**Release flow**
1. Game repo: set `tools/package/version.txt`, run the *Package – … Payload* configurations.
2. `tools/build.sh` if the patcher changed.
3. `tools/prepare-release.sh --key ~/svx-keys/release.key --linux <payload> --windows <payload> [--patcher out/patcher/<version>]`
4. Run `publish-patcher.sh`, `publish-release.sh`, then `out/index/publish-index.sh`. Commit `releases/catalog.json`.

**Distribution model.** Game files ship as pack assets in `svx-<version>` releases, patchers in `patcher-<version>` (marked Latest), and the signed release list in the `index` release. The index and manifests are Ed25519-signed against the root in `trust/production/1.root.json`; every file is SHA-256 verified. Old releases must stay published because newer releases reuse their packs.

**Keys.** `tools/init-trust.sh` creates them in `~/svx-keys`, `tools/backup-keys.sh` writes a verified encrypted backup, and `tools/restore-keys.sh` restores it. Private keys never enter this repository.

</details>

<div align="center">
<sub>© 2026 gicstin. All rights reserved. See <a href="LICENSE">LICENSE</a>.</sub>
</div>
