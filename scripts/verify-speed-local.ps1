# T038 local verify: cargo test clock + volume fingerprint (5 consecutive rounds).
param(
    [int]$Rounds = 5
)

$ErrorActionPreference = 'Stop'
$root = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path

Push-Location $root
try {
    $results = @()
    1..$Rounds | ForEach-Object {
        Write-Host "=== LOCAL SPEED VERIFY #$_ ==="
        $prevEap = $ErrorActionPreference
        $ErrorActionPreference = 'Continue'
        try {
            $env:TINYIMAGE_T038_TIMING = '1'
            $out = & cargo test --manifest-path src-tauri/Cargo.toml --lib t038_speed_volume_tool_measured -- --nocapture 2>&1 | Out-String
            $code = $LASTEXITCODE
        } finally {
            $ErrorActionPreference = $prevEap
        }
        Write-Host $out
        if ($code -ne 0) {
            throw "ROUND_FAIL $_ exit=$code"
        }
        $lines = $out -split "`n" | Where-Object { $_ -match '^T038 ' }
        $results += [pscustomobject]@{
            Round = $_
            PASS = $true
            Lines = $lines
        }
        if ($_ -lt $Rounds) { Start-Sleep -Seconds 1 }
    }
    $all = ($results | Where-Object { $_.PASS }).Count -eq $Rounds
    Write-Host "QUINTUPLE_PASS=$all"
    if (-not $all) { exit 1 }
}
finally {
    Pop-Location
}
