# Native GUI regression: close the real window, require a clean process exit,
# then reopen the same isolated profile. No real account/provider data is used.
param(
    [string]$Exe = (Join-Path $PSScriptRoot '../target/release/zeron.exe'),
    [int]$Runs = 2
)
$ErrorActionPreference = 'Stop'
if ($env:OS -ne 'Windows_NT') { throw 'This probe requires Windows and an interactive desktop' }
if ($Runs -lt 1) { throw 'Runs must be positive' }
$Exe = (Resolve-Path -LiteralPath $Exe).Path
$root = Join-Path $PSScriptRoot ('../target/windows-lifecycle-' + [guid]::NewGuid().ToString('N'))
$root = [IO.Path]::GetFullPath($root)
New-Item -ItemType Directory -Path $root | Out-Null

if (-not ('ZeronWindowProbe' -as [type])) {
Add-Type @'
using System;
using System.Text;
using System.Runtime.InteropServices;
public static class ZeronWindowProbe {
    [DllImport("user32.dll", CharSet = CharSet.Unicode)]
    public static extern int GetClassName(IntPtr hwnd, StringBuilder name, int count);
    [DllImport("user32.dll")]
    public static extern uint GetWindowThreadProcessId(IntPtr hwnd, out uint pid);
}
'@
}

$overrides = @{
    HOME = $root; USERPROFILE = $root
    LOCALAPPDATA = (Join-Path $root 'AppData/Local')
    APPDATA = (Join-Path $root 'AppData/Roaming')
    ZERON_DATA_DIR = (Join-Path $root 'Zeron')
    ZERON_EDGE_TOKEN = $null; ZERON_IPC_PORT = '0'
    ZERON_EDGE_URL = 'http://127.0.0.1:1'; ZERON_ORG_ID = $null
    ZERON_HARNESS = 'mock'; RUST_BACKTRACE = '1'
}
$saved = @{}
foreach ($key in $overrides.Keys) {
    $saved[$key] = [Environment]::GetEnvironmentVariable($key, 'Process')
    [Environment]::SetEnvironmentVariable($key, $overrides[$key], 'Process')
}
try {
    $firstDevice = $null
    foreach ($run in 1..$Runs) {
        $stdout = Join-Path $root "run-$run.stdout.txt"
        $stderr = Join-Path $root "run-$run.stderr.txt"
        $p = Start-Process -FilePath $Exe -PassThru -RedirectStandardOutput $stdout -RedirectStandardError $stderr
        try {
            # Retain the native process handle before Refresh/exit. Windows PowerShell
            # otherwise can report a null ExitCode for a Start-Process result.
            $null = $p.Handle
            $deadline = [DateTime]::UtcNow.AddSeconds(25)
            do {
                $p.Refresh()
                if ($p.HasExited) { throw "Run $run exited before opening: $($p.ExitCode)" }
                if ($p.MainWindowHandle -ne 0) { break }
                Start-Sleep -Milliseconds 100
            } while ([DateTime]::UtcNow -lt $deadline)
            if ($p.MainWindowHandle -eq 0) { throw "Run $run did not open a window within 25s" }
            $hwnd = $p.MainWindowHandle
            $class = [Text.StringBuilder]::new(256)
            [void][ZeronWindowProbe]::GetClassName($hwnd, $class, $class.Capacity)
            [uint32]$owner = 0
            [void][ZeronWindowProbe]::GetWindowThreadProcessId($hwnd, [ref]$owner)
            if ($owner -ne $p.Id -or $class.ToString() -eq 'ConsoleWindowClass') {
                throw "Probe selected a non-application window (class=$class owner=$owner)"
            }
            # Wait for real local engine boot, rather than closing the splash.
            $deadline = [DateTime]::UtcNow.AddSeconds(25)
            do {
                $p.Refresh()
                if ($p.HasExited) { throw "Run $run exited during engine startup" }
                if ((Get-Content -LiteralPath $stdout -Raw) -match 'engine core assembled') { break }
                Start-Sleep -Milliseconds 100
            } while ([DateTime]::UtcNow -lt $deadline)
            if (-not ((Get-Content -LiteralPath $stdout -Raw) -match 'engine core assembled')) {
                throw "Run $run did not assemble its local engine within 25s"
            }
            $device = (Get-Content -LiteralPath (Join-Path $root 'Zeron/device-id') -Raw).Trim()
            if ([string]::IsNullOrWhiteSpace($device)) { throw 'Engine did not persist its device identity' }
            if ($null -eq $firstDevice) { $firstDevice = $device }
            elseif ($device -ne $firstDevice) { throw 'Reopening changed the persisted device identity' }
            "Run $run window class=$class; owner=$owner; HWND=$hwnd" | Add-Content -Encoding utf8 -LiteralPath (Join-Path $root 'result.txt')
            if (-not $p.CloseMainWindow()) { throw "Run $run could not request normal close" }
            if (-not $p.WaitForExit(15000)) { throw "Run $run stayed alive 15s after window close" }
            if ($p.ExitCode -ne 0) { throw "Run $run closed with exit $($p.ExitCode)" }
            "Run $run normal close: exit 0" | Add-Content -Encoding utf8 -LiteralPath (Join-Path $root 'result.txt')
        } finally {
            $p.Refresh()
            if (-not $p.HasExited) {
                # Only the process created by this probe; never an existing engine.
                $p.Kill()
                $p.WaitForExit()
            }
            $p.Dispose()
        }
    }
    Write-Host "PASS: $Runs native GUI open/close cycles; evidence: $root"
} catch {
    Write-Host "FAIL: $($_.Exception.Message); evidence: $root"
    throw
} finally {
    foreach ($key in $saved.Keys) {
        [Environment]::SetEnvironmentVariable($key, $saved[$key], 'Process')
    }
}
