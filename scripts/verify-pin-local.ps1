# T037 local verify: real TinyImage window via CDP DOM + Win32 TOPMOST (5 consecutive rounds).
param(
    [int]$Rounds = 5,
    [int]$StartupTimeoutSec = 90,
    [int]$CdpPort = 9333
)

$ErrorActionPreference = 'Stop'
$root = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$cdpScript = Join-Path $PSScriptRoot 'cdp-eval.mjs'

function U8([byte[]]$bytes) {
    return [System.Text.Encoding]::UTF8.GetString($bytes)
}

$TXT_READY = U8 0xE5,0xB0,0xB1,0xE7,0xBB,0xAA
$TXT_PIN_ON = U8 0xE5,0xB7,0xB2,0xE7,0xBD,0xAE,0xE9,0xA1,0xB6
$TXT_PIN_OFF = U8 0xE5,0xB7,0xB2,0xE5,0x8F,0x96,0xE6,0xB6,0x88,0xE7,0xBD,0xAE,0xE9,0xA1,0xB6
$TXT_PIN_LABEL = U8 0xE7,0xAA,0x97,0xE5,0x8F,0xA3,0xE7,0xBD,0xAE,0xE9,0xA1,0xB6
$TXT_PIN_LABEL_ON = U8 0xE5,0xB7,0xB2,0xE7,0xBD,0xAE,0xE9,0xA1,0xB6,0xEF,0xBC,0x88,0xE7,0x82,0xB9,0xE5,0x87,0xBB,0xE5,0x8F,0x96,0xE6,0xB6,0x88,0xEF,0xBC,0x89
$TXT_OLD_FOOTER = U8 0xE5,0x9B,0xBE,0xE9,0x92,0x89,0xE7,0xBD,0xAE,0xE9,0xA1,0xB6

Add-Type @"
using System;
using System.Runtime.InteropServices;
public class PinWin32 {
  [DllImport("user32.dll", EntryPoint = "GetWindowLongPtr")]
  public static extern IntPtr GetWindowLongPtr(IntPtr hWnd, int nIndex);
  [DllImport("user32.dll")]
  public static extern bool SetForegroundWindow(IntPtr hWnd);
}
"@

function Get-TinyImageHwnd {
    $proc = Get-Process -Name 'tinyimage' -ErrorAction SilentlyContinue |
        Where-Object { $_.MainWindowHandle -ne 0 -and $_.MainWindowTitle -eq 'TinyImage' } |
        Select-Object -First 1
    if ($proc) { return $proc.MainWindowHandle }
    return [IntPtr]::Zero
}

function Test-WindowTopMost([IntPtr]$hwnd) {
    $GWL_EXSTYLE = -20
    $WS_EX_TOPMOST = 0x00000008
    $ex = [PinWin32]::GetWindowLongPtr($hwnd, $GWL_EXSTYLE)
    return (($ex.ToInt64() -band $WS_EX_TOPMOST) -ne 0)
}

function Wait-TopMost([IntPtr]$hwnd, [bool]$expected, [int]$timeoutSec = 8) {
    $deadline = (Get-Date).AddSeconds($timeoutSec)
    while ((Get-Date) -lt $deadline) {
        if ((Test-WindowTopMost $hwnd) -eq $expected) { return $true }
        Start-Sleep -Milliseconds 150
    }
    return $false
}

function Invoke-PinOn([IntPtr]$hwnd) {
    Click-PinButton
    Wait-ForStatus $TXT_PIN_ON | Out-Null
    if (Wait-TopMost $hwnd $true) { return }
    Start-Sleep -Milliseconds 300
    Click-PinButton
    Wait-ForStatus $TXT_PIN_ON | Out-Null
    if (-not (Wait-TopMost $hwnd $true)) { throw 'PIN_ON_TOPMOST_FAIL' }
}

function Invoke-CdpEval([string]$expr) {
    $prevOut = [Console]::OutputEncoding
    $prevErr = [Console]::InputEncoding
    try {
        [Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false)
        $out = & node $cdpScript $CdpPort $expr 2>&1 | Out-String
        $out = $out.TrimEnd("`r", "`n")
    } finally {
        [Console]::OutputEncoding = $prevOut
    }
    if ($LASTEXITCODE -ne 0) {
        throw "CDP_FAIL $out"
    }
    return $out
}

function To-JsUnicode([string]$s) {
    if (-not $s) { return '' }
    $parts = foreach ($ch in $s.ToCharArray()) {
        '\u{0:x4}' -f [int][char]$ch
    }
    return -join $parts
}

function Test-StatusContains([string]$needle) {
    $jsNeedle = To-JsUnicode $needle
    $result = Invoke-CdpEval "document.getElementById('status').textContent.includes('$jsNeedle')"
    return $result -eq 'true'
}

function Wait-ForStatus([string]$needle, [int]$timeoutSec = 8) {
    $deadline = (Get-Date).AddSeconds($timeoutSec)
    while ((Get-Date) -lt $deadline) {
        if (Test-StatusContains $needle) {
            return Get-StatusText
        }
        Start-Sleep -Milliseconds 200
    }
    throw "STATUS_TIMEOUT need=$needle got=$(Get-StatusText)"
}

function Wait-CdpReady([int]$timeoutSec) {
    $deadline = (Get-Date).AddSeconds($timeoutSec)
    while ((Get-Date) -lt $deadline) {
        try {
            $title = Invoke-CdpEval "document.title"
            if ($title -and $title.Contains('TinyImage')) { return }
        } catch {
            Start-Sleep -Milliseconds 400
        }
    }
    throw 'CDP_READY_TIMEOUT'
}

function Wait-ForTinyImageWindow([int]$timeoutSec, [System.Diagnostics.Process]$proc) {
    $deadline = (Get-Date).AddSeconds($timeoutSec)
    while ((Get-Date) -lt $deadline) {
        if ($proc.HasExited) {
            throw "APP_EXITED code=$($proc.ExitCode)"
        }
        $hwnd = Get-TinyImageHwnd
        if ($hwnd -ne [IntPtr]::Zero) {
            [void][PinWin32]::SetForegroundWindow($hwnd)
            Start-Sleep -Milliseconds 500
            return $hwnd
        }
        Start-Sleep -Milliseconds 300
    }
    throw 'APP_START_TIMEOUT'
}

function Get-StatusText {
    return Invoke-CdpEval "document.getElementById('status').textContent"
}

function Test-LabelEquals([string]$expected) {
    $jsExpected = To-JsUnicode $expected
    $result = Invoke-CdpEval "document.getElementById('btn-pin').getAttribute('aria-label') === '$jsExpected'"
    return $result -eq 'true'
}

function Test-LabelContains([string]$needle) {
    $jsNeedle = To-JsUnicode $needle
    $result = Invoke-CdpEval "document.getElementById('btn-pin').getAttribute('aria-label').includes('$jsNeedle')"
    return $result -eq 'true'
}

function Click-PinButton {
    Invoke-CdpEval "document.getElementById('btn-pin').click(); 'ok'" | Out-Null
}

function Get-PinLayout {
    $json = Invoke-CdpEval @"
(() => {
  const b = document.getElementById('btn-pin');
  const r = b.getBoundingClientRect();
  return JSON.stringify({
    pinW: r.width,
    pinH: r.height,
    x: r.x,
    y: r.y,
    winW: window.innerWidth,
    winH: window.innerHeight,
    label: b.getAttribute('aria-label') || '',
    hasSvg: !!b.querySelector('.pin-svg'),
    className: b.className
  });
})()
"@
    return $json | ConvertFrom-Json
}

function Test-PinLayoutFromDom($layout) {
    $rightGap = $layout.winW - ($layout.x + $layout.pinW)
    return @{
        IconSized = ($layout.pinW -le 48) -and ($layout.pinH -le 48) -and ($layout.pinW -ge 20)
        TopRight = ($rightGap -ge 0) -and ($rightGap -le 40) -and ($layout.y -ge 0) -and ($layout.y -le 40)
        NotFooter = $layout.y -lt ($layout.winH * 0.45)
        HasSvg = [bool]$layout.hasSvg
        PinClass = [string]$layout.className -match 'pin-icon'
        PinW = [math]::Round([double]$layout.pinW, 1)
        PinH = [math]::Round([double]$layout.pinH, 1)
        RightGap = [math]::Round([double]$rightGap, 1)
        TopGap = [math]::Round([double]$layout.y, 1)
    }
}

function Ensure-NotPinned([IntPtr]$hwnd) {
    if (-not (Test-WindowTopMost $hwnd)) { return }
    Click-PinButton
    Wait-ForStatus $TXT_PIN_OFF | Out-Null
    Start-Sleep -Milliseconds 350
    if (Test-WindowTopMost $hwnd) { throw 'UNPIN_FAILED' }
}

function Test-PinRound([IntPtr]$hwnd, [int]$n) {
    $hwnd = Get-TinyImageHwnd
    if ($hwnd -eq [IntPtr]::Zero) { throw 'HWND_LOST' }
    [void][PinWin32]::SetForegroundWindow($hwnd)
    Start-Sleep -Milliseconds 250

    $dom = Get-PinLayout
    $layout = Test-PinLayoutFromDom $dom
    $labelOk = (Test-LabelEquals $TXT_PIN_LABEL) -or (Test-LabelEquals $TXT_PIN_LABEL_ON)
    $notTextBtn = -not (Test-LabelContains $TXT_OLD_FOOTER)

    Ensure-NotPinned $hwnd

    Invoke-PinOn $hwnd
    $onStatus = Get-StatusText
    $topOn = $true
    $labelOn = Test-LabelEquals $TXT_PIN_LABEL_ON
    $statusOnOk = Test-StatusContains $TXT_PIN_ON

    Click-PinButton
    $offStatus = Wait-ForStatus $TXT_PIN_OFF
    $topOff = Wait-TopMost $hwnd $false
    $statusOffOk = Test-StatusContains $TXT_PIN_OFF

    $checks = @{
        PinFound = $true
        IconSized = $layout.IconSized
        TopRight = $layout.TopRight
        NotFooter = $layout.NotFooter
        HasSvg = $layout.HasSvg
        PinClass = $layout.PinClass
        LabelOk = $labelOk
        NotTextBtn = $notTextBtn
        StatusOn = $statusOnOk
        WinTopOn = $topOn
        LabelOn = $labelOn
        StatusOff = $statusOffOk
        WinTopOff = $topOff
    }
    $ok = ($checks.Values | Where-Object { $_ -eq $false }).Count -eq 0
    if (-not $ok) {
        $failed = $checks.GetEnumerator() | Where-Object { $_.Value -eq $false } | ForEach-Object { $_.Key }
        Write-Host "FAILED_CHECKS=$failed"
    }
    [pscustomobject]@{
        Round = $n
        PASS = $ok
        StatusOn = $onStatus
        StatusOff = $offStatus
        Layout = $layout
        Checks = $checks
    }
}

Write-Host '=== BUILD FRONTEND ==='
Push-Location $root
$appProc = $null
$prevCdp = $env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS
try {
    npm run build | Out-Host
    if ($LASTEXITCODE -ne 0) { throw 'BUILD_FAIL' }

    Write-Host '=== BUILD RUST ==='
    cargo build --manifest-path src-tauri/Cargo.toml --quiet
    if ($LASTEXITCODE -ne 0) { throw 'CARGO_BUILD_FAIL' }

    $exe = Join-Path $root 'src-tauri\target\debug\tinyimage.exe'
    if (-not (Test-Path $exe)) { throw "EXE_NOT_FOUND $exe" }

    Get-Process -Name 'tinyimage' -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
    Start-Sleep -Milliseconds 400

    $env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS = "--remote-debugging-port=$CdpPort"
    Write-Host "=== START APP (CDP port $CdpPort) ==="
    $appProc = Start-Process -FilePath $exe -WorkingDirectory $root -PassThru -WindowStyle Normal

    $hwnd = Wait-ForTinyImageWindow $StartupTimeoutSec $appProc
    Write-Host "APP_HWND=$hwnd"
    Wait-CdpReady 30
    Wait-ForStatus $TXT_READY | Out-Null

    $results = @()
    1..$Rounds | ForEach-Object {
        Write-Host "=== LOCAL PIN VERIFY #$_ ==="
        $r = Test-PinRound $hwnd $_
        $results += $r
        $r | Format-List
        if (-not $r.PASS) {
            throw "ROUND_FAIL $_"
        }
        if ($_ -lt $Rounds) { Start-Sleep -Seconds 1 }
    }

    $all = ($results | Where-Object { $_.PASS }).Count -eq $Rounds
    Write-Host "QUINTUPLE_PASS=$all"
    if (-not $all) { exit 1 }
}
finally {
    if ($null -ne $prevCdp) { $env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS = $prevCdp }
    else { Remove-Item Env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS -ErrorAction SilentlyContinue }
    Pop-Location
    Get-Process -Name 'tinyimage' -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
    if ($appProc -and -not $appProc.HasExited) {
        Stop-Process -Id $appProc.Id -Force -ErrorAction SilentlyContinue
    }
}
