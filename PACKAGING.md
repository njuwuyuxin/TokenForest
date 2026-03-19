# Windows packaging

## One-time setup

```powershell
rustup target add x86_64-pc-windows-msvc
rustup target add i686-pc-windows-msvc
```

## Build and package

```powershell
powershell -ExecutionPolicy Bypass -File .\scripts\package_windows.ps1
```

Outputs are written to `dist/` as target-specific folders and zip files.
By default, package output contains only `token_forest.exe` (no `token_forest.toml`).

## Useful options

```powershell
# Only x64
powershell -ExecutionPolicy Bypass -File .\scripts\package_windows.ps1 -SkipX86

# Keep folder output only, do not zip
powershell -ExecutionPolicy Bypass -File .\scripts\package_windows.ps1 -NoZip
```
