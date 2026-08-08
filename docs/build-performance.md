# Build Performance

## Prerequisites

Windows builds use LLVM LLD instead of the slower MSVC linker. Install it
once on the development machine:

```powershell
cargo install cargo-binutils
rust-lld.exe --version
```

The repository-level `.cargo/config.toml` is intentional. Cargo discovers
configuration from the current working directory, so keeping this file only
under `src-tauri` would not cover npm scripts that pass `--manifest-path`.

## Daily Commands

Use the debug smoke build for frontend and native runtime changes:

```powershell
npm run build:tauri:smoke
```

After the first dependency build, this path is normally about 15-20 seconds
when no files changed. It uses `tauri/custom-protocol`, so the executable does
not point at a localhost development server.

For Rust-only release validation without packaging:

```powershell
npm run build:tauri:rust
```

This is fast when the release cache is warm. A changed Rust application crate
still needs a release link, which is expected to take longer than debug.

## Packaging

The normal release profile disables LTO to keep package iteration practical.
Use the size command only for final artifact measurements:

```powershell
npm run build:tauri:app:size
```

ThinLTO, stripping, and the NSIS bundle are intentionally kept out of the
daily smoke loop because they trade build time for final size.
