# TokenForest

TokenForest 是一个 Windows 上的 Rust 终端可视化工具（TUI），
用于实时监控 Codex / Claude Code 相关进程的网络吞吐，并将估算的 token 速率映射成森林雨景动画，帮助你直观感知“模型输出负载”变化。

TokenForest is a Rust TUI for Windows that monitors network throughput of Codex/Claude Code related processes,
then maps estimated token rate to a live rainy-forest scene so you can see model workload changes at a glance.

## Highlights

- Animated forest + rain scene rendered with `ratatui`
- Top-left 3D-style `Token Forest` logo with subtle breathing effect
- Status panel for process count, TCP connections, RX/TX throughput, and token estimate
- Smoothing and clipping controls via `token_forest.toml`
- Windows packaging script for release artifacts

## Requirements

- Windows (network monitor implementation is Windows-only)
- Rust toolchain (MSVC target), tested with `x86_64-pc-windows-msvc`

## Run in dev mode

```powershell
cargo run
```

Controls:

- `q` or `Esc`: quit
- `s`: show/hide status panel

## Build release

```powershell
cargo build --release
```

Output binary:

- `target/release/token_forest.exe`

## Windows packaging

Packaging instructions are in `PACKAGING.md`.

Quick command:

```powershell
powershell -ExecutionPolicy Bypass -File .\scripts\package_windows.ps1
```

Typical output:

- `dist/token_forest-v<version>-x86_64-pc-windows-msvc.zip`
- `dist/token_forest-v<version>-<target>/token_forest.exe`

## Exe icon

- Put icon file at `assets/icon.ico` before building.
- On Windows, build embeds that icon into the exe automatically.
- If `assets/icon.ico` is missing, build still succeeds with default exe icon.

## Configuration

Edit `token_forest.toml` to tune rendering, sampling interval, and smoothing behavior.
If the file is missing, defaults are used.

## Compatibility notes

- Binary built for `x86_64-pc-windows-msvc` runs on 64-bit Windows.
- Add `i686-pc-windows-msvc` target if you need a 32-bit package.
- If TCP eStats are unavailable on a machine, the app keeps running and reports the error in status.
