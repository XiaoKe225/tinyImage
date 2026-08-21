# TinyImage Pack：对齐 Denuvo Dev/Pack 体感；混淆层=前端 JS（非 UPX/Obfuscar）。
param(
    [ValidateSet('Dev', 'Pack')]
    [string]$Action = 'Dev',
    [switch]$Verify,
    [switch]$SkipObfuscation,
    [switch]$NoZip,
    [string]$OutputDir = 'release\protected'
)

$ErrorActionPreference = 'Stop'
$Root = Split-Path -Parent $PSScriptRoot
Set-Location $Root

function Get-ProductVersion {
    $pkg = Get-Content (Join-Path $Root 'package.json') -Raw -Encoding UTF8 | ConvertFrom-Json
    return [string]$pkg.version
}

function Stop-TinyImageProcesses {
    Get-Process -ErrorAction SilentlyContinue |
        Where-Object { $_.ProcessName -match '^(tinyimage|TinyImage)$' } |
        Stop-Process -Force -ErrorAction SilentlyContinue
    Start-Sleep -Milliseconds 400
}

function Invoke-Npm {
    param([string[]]$NpmArgs)
    & npm @NpmArgs
    if ($LASTEXITCODE -ne 0) { throw "npm $($NpmArgs -join ' ') failed ($LASTEXITCODE)" }
}

function Invoke-Dev {
    Write-Host '>> Dev: frontend build (no obfuscation)'
    Invoke-Npm @('run', 'build')
    Write-Host '>> Dev OK: dist\ (plain). Use npm run start for tauri dev.'
}

function Invoke-Pack {
    param([bool]$DoVerify, [bool]$SkipObf, [bool]$SkipZip)

    Stop-TinyImageProcesses
    $ver = Get-ProductVersion
    $outAbs = Join-Path $Root $OutputDir
    if (Test-Path $outAbs) { Remove-Item $outAbs -Recurse -Force }
    New-Item -ItemType Directory -Path $outAbs -Force | Out-Null

    Write-Host '>> Pack: frontend build'
    Invoke-Npm @('run', 'build')

    if (-not $SkipObf) {
        Write-Host '>> Pack: obfuscate dist JS (javascript-obfuscator)'
        Invoke-Npm @('run', 'obfuscate')
        $mark = Join-Path $Root 'dist\.obfuscated'
        if (-not (Test-Path $mark)) { throw 'Obfuscation marker dist\.obfuscated missing' }
    } else {
        Write-Host '>> Pack: SkipObfuscation (NOT for external release)'
    }

    $cfgPath = Join-Path $env:TEMP 'tinyimage-tauri-pack.json'
    Set-Content -LiteralPath $cfgPath -Value '{"build":{"beforeBuildCommand":""}}' -Encoding ASCII

    Write-Host '>> Pack: tauri build (skip frontend rebuild; use obfuscated dist)'
    & npx --yes tauri build --config $cfgPath
    if ($LASTEXITCODE -ne 0) { throw "tauri build failed ($LASTEXITCODE)" }

    $nsisDir = Join-Path $Root 'src-tauri\target\release\bundle\nsis'
    if (-not (Test-Path $nsisDir)) { throw "NSIS output missing: $nsisDir" }
    $setup = Get-ChildItem $nsisDir -Filter '*-setup.exe' -File | Sort-Object LastWriteTime -Descending | Select-Object -First 1
    if (-not $setup) { throw "No *-setup.exe under $nsisDir" }

    $destSetup = Join-Path $outAbs $setup.Name
    Copy-Item $setup.FullName $destSetup -Force

    $readme = @"
TinyImage $ver — protected release
- Installer: $($setup.Name)
- Pack includes frontend JS obfuscation (Dev builds do not).
- Not UPX/Themida/MPRESS; obfuscation raises reverse cost only — not unbreakable.
- Pure local compressor; no TinyPNG / cloud compress API.
"@
    Set-Content -LiteralPath (Join-Path $outAbs 'README.txt') -Value $readme -Encoding UTF8

    $sums = @()
    Get-ChildItem $outAbs -File | ForEach-Object {
        $hash = (Get-FileHash -LiteralPath $_.FullName -Algorithm SHA256).Hash.ToLowerInvariant()
        $sums += "$hash  $($_.Name)"
    }
    $sumsPath = Join-Path $outAbs 'SHA256SUMS.txt'
    Set-Content -LiteralPath $sumsPath -Value ($sums -join "`n") -Encoding ASCII

    $zipName = "TinyImage-$ver-win-x64-protected.zip"
    $zipPath = Join-Path $Root "release\$zipName"
    if (Test-Path $zipPath) { Remove-Item $zipPath -Force }
    if (-not $SkipZip) {
        Compress-Archive -Path (Join-Path $outAbs '*') -DestinationPath $zipPath -Force
        Write-Host ">> Zip: $zipPath"
    }

    if ($DoVerify) {
        Invoke-VerifyLayout -ProtectedDir $outAbs -ExpectObfuscation (-not $SkipObf) -ZipPath $(if ($SkipZip) { $null } else { $zipPath })
    }

    Write-Host ">> Pack OK: $outAbs"
}

function Invoke-VerifyLayout {
    param(
        [string]$ProtectedDir,
        [bool]$ExpectObfuscation,
        [string]$ZipPath
    )
    if (-not (Test-Path $ProtectedDir)) { throw "Missing: $ProtectedDir" }
    $setup = Get-ChildItem $ProtectedDir -Filter '*-setup.exe' -File | Select-Object -First 1
    if (-not $setup) { throw 'Verify fail: no *-setup.exe in protected\' }
    if (-not (Test-Path (Join-Path $ProtectedDir 'SHA256SUMS.txt'))) {
        throw 'Verify fail: SHA256SUMS.txt missing'
    }
    if (-not (Test-Path (Join-Path $ProtectedDir 'README.txt'))) {
        throw 'Verify fail: README.txt missing'
    }
    if ($ExpectObfuscation) {
        $mark = Join-Path $Root 'dist\.obfuscated'
        if (-not (Test-Path $mark)) { throw 'Verify fail: dist\.obfuscated missing (Pack must obfuscate)' }
    }
    Get-Content (Join-Path $ProtectedDir 'SHA256SUMS.txt') | ForEach-Object {
        if ($_ -match '^\s*$') { return }
        $parts = $_ -split '\s+', 2
        if ($parts.Count -lt 2) { throw "Verify fail: bad SUMS line: $_" }
        $expect = $parts[0].ToLowerInvariant()
        $name = $parts[1].Trim()
        $file = Join-Path $ProtectedDir $name
        if (-not (Test-Path $file)) { throw "Verify fail: listed file missing: $name" }
        $actual = (Get-FileHash -LiteralPath $file -Algorithm SHA256).Hash.ToLowerInvariant()
        if ($actual -ne $expect) { throw "Verify fail: hash mismatch $name" }
    }
    if ($ZipPath -and -not (Test-Path $ZipPath)) { throw "Verify fail: zip missing $ZipPath" }
    Write-Host "Verify OK: $ProtectedDir"
}

Write-Host "== TinyImage build.ps1 Action=$Action Verify=$Verify SkipObfuscation=$SkipObfuscation =="

switch ($Action) {
    'Dev' { Invoke-Dev }
    'Pack' {
        Invoke-Pack -DoVerify:$Verify -SkipObf:$SkipObfuscation -SkipZip:$NoZip
    }
}
