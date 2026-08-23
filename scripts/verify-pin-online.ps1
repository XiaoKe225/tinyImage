# T037 线上自检：从 jsDelivr 拉取已部署源码，验证图钉置顶已接线（连续 3 轮须全过）。
param(
    [string]$Ref = 'master',
    [int]$Rounds = 3
)

$ErrorActionPreference = 'Stop'
$base = "https://cdn.jsdelivr.net/gh/XiaoKe225/tinyImage@$Ref"
$ua = 'Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 Chrome/122.0.0.0 Safari/537.36'

function Fetch-Text([string]$rel) {
    $url = "$base/$rel"
    $tmp = Join-Path $env:TEMP ("pin-verify-" + [Guid]::NewGuid().ToString('n') + ".txt")
    $stat = curl.exe -sL -A $ua -o $tmp -w '%{http_code}|%{size_download}' --max-time 45 $url
    $parts = $stat -split '\|'
    if ($parts[0] -ne '200' -or [int]$parts[1] -lt 10) {
        throw "FETCH_FAIL $rel => $stat"
    }
    return [System.IO.File]::ReadAllText($tmp, [System.Text.UTF8Encoding]::new($false))
}

function Test-PinRound([int]$n) {
    $html = Fetch-Text 'index.html'
    $main = Fetch-Text 'src/main.ts'
    $cap = Fetch-Text 'src-tauri/capabilities/default.json'
    $readme = Fetch-Text 'README.md'

    $checks = @{
        HtmlBtnPin = $html -match 'id="btn-pin"'
        HtmlPinClass = $html -match 'class="btn pin"'
        MainSetAlways = $main.Contains('setAlwaysOnTop')
        MainPersist = $main.Contains('alwaysOnTop')
        MainToggle = $main.Contains('toggleAlwaysOnTop')
        CapPerm = $cap.Contains('allow-set-always-on-top')
        ReadmePin = $readme.Contains('2.1.3')
    }
    $ok = ($checks.Values | Where-Object { $_ -eq $false }).Count -eq 0
    [pscustomobject]@{
        Round = $n
        Ref = $Ref
        PASS = $ok
        Checks = $checks
    }
}

$results = @()
1..$Rounds | ForEach-Object {
    Write-Host "=== ONLINE PIN VERIFY #$_ ==="
    $r = Test-PinRound $_
    $results += $r
    $r | Format-List
    if (-not $r.PASS) { exit 1 }
    if ($_ -lt $Rounds) { Start-Sleep -Seconds 2 }
}

$all = ($results | Where-Object { $_.PASS }).Count -eq $Rounds
Write-Host "TRIPLE_PASS=$all"
if (-not $all) { exit 1 }
