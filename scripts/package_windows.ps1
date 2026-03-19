param(
    [switch]$NoZip,
    [switch]$SkipX86
)

$ErrorActionPreference = "Stop"

$repoRoot = Split-Path -Parent $PSScriptRoot
Set-Location $repoRoot

$cargoPath = Join-Path $env:USERPROFILE ".cargo\bin\cargo.exe"
if (-not (Test-Path $cargoPath)) {
    $cargoPath = "cargo"
}

$rustupPath = Join-Path $env:USERPROFILE ".cargo\bin\rustup.exe"
$installedTargets = @{}
if (Test-Path $rustupPath) {
    $installed = & $rustupPath target list --installed
    foreach ($target in $installed) {
        $installedTargets[$target.Trim()] = $true
    }
}

$targets = @("x86_64-pc-windows-msvc")
if (-not $SkipX86) {
    $targets += "i686-pc-windows-msvc"
}

$versionLine = Select-String -Path "Cargo.toml" -Pattern '^version\s*=\s*"([^"]+)"' | Select-Object -First 1
if (-not $versionLine) {
    throw "Could not read version from Cargo.toml."
}
$version = $versionLine.Matches[0].Groups[1].Value

$builtTargets = @()
foreach ($target in $targets) {
    if ($installedTargets.Count -gt 0 -and -not $installedTargets.ContainsKey($target)) {
        Write-Warning "Skip $target (target not installed). Run: rustup target add $target"
        continue
    }

    Write-Host "Building release for $target ..."
    & $cargoPath build --release --target $target
    if ($LASTEXITCODE -ne 0) {
        throw "Build failed for $target."
    }
    $builtTargets += $target
}

if ($builtTargets.Count -eq 0) {
    throw "No build artifacts were produced."
}

$distRoot = Join-Path $repoRoot "dist"
New-Item -ItemType Directory -Path $distRoot -Force | Out-Null

foreach ($target in $builtTargets) {
    $pkgBaseName = "token_forest-v$version-$target"
    $pkgName = $pkgBaseName
    $pkgDir = Join-Path $distRoot $pkgName
    if (Test-Path $pkgDir) {
        try {
            Remove-Item -Path $pkgDir -Recurse -Force
        } catch {
            $suffix = Get-Date -Format "yyyyMMdd-HHmmss"
            $pkgName = "$pkgBaseName-$suffix"
            $pkgDir = Join-Path $distRoot $pkgName
            Write-Warning "Could not overwrite existing package folder. Using $pkgName instead."
        }
    }
    New-Item -ItemType Directory -Path $pkgDir -Force | Out-Null

    $exeSource = Join-Path $repoRoot "target\$target\release\token_forest.exe"
    if (-not (Test-Path $exeSource)) {
        throw "Missing artifact: $exeSource"
    }
    Copy-Item -Path $exeSource -Destination (Join-Path $pkgDir "token_forest.exe")

    if (-not $NoZip) {
        $zipPath = Join-Path $distRoot "$pkgName.zip"
        if (Test-Path $zipPath) {
            Remove-Item -Path $zipPath -Force
        }
        Compress-Archive -Path "$pkgDir\*" -DestinationPath $zipPath
        Write-Host "Created $zipPath"
    }
}

Write-Host "Done. Output directory: $distRoot"
