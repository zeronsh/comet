# Pixel regression for the real pinned GPUI Windows renderer. Requires a desktop.
# Captures only this synthetic fixture's client HWND, never the entire screen.
param([string]$Exe = (Join-Path $PSScriptRoot '../target/release/examples/windows-render-fixture.exe'))
$ErrorActionPreference = 'Stop'
if ($env:OS -ne 'Windows_NT') { throw 'This probe requires Windows' }
$Exe = (Resolve-Path -LiteralPath $Exe).Path
$root = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot ('../target/windows-render-' + [guid]::NewGuid().ToString('N'))))
New-Item -ItemType Directory -Path $root | Out-Null
Add-Type -AssemblyName System.Drawing
$image = Join-Path $root 'quadrants.png'
$source = [Drawing.Bitmap]::new(240, 120)
$g = [Drawing.Graphics]::FromImage($source)
try {
    $colors = @('#F04040', '#40F080', '#4080F0', '#F0D040')
    for ($i = 0; $i -lt 4; $i++) {
        $brush = [Drawing.SolidBrush]::new([Drawing.ColorTranslator]::FromHtml($colors[$i]))
        try { $g.FillRectangle($brush, ($i % 2) * 120, [math]::Floor($i / 2) * 60, 120, 60) }
        finally { $brush.Dispose() }
    }
    $source.Save($image, [Drawing.Imaging.ImageFormat]::Png)
} finally { $g.Dispose(); $source.Dispose() }
$p = Start-Process -FilePath $Exe -ArgumentList ('"' + $image + '"') -PassThru -RedirectStandardOutput (Join-Path $root 'stdout.txt') -RedirectStandardError (Join-Path $root 'stderr.txt')
$bitmap = $null
try {
    $null = $p.Handle
    $deadline = [DateTime]::UtcNow.AddSeconds(20)
    do {
        $p.Refresh()
        if ($p.HasExited) { throw "Fixture exited early: $($p.ExitCode)" }
        if ($p.MainWindowHandle -ne 0) { break }
        Start-Sleep -Milliseconds 100
    } while ([DateTime]::UtcNow -lt $deadline)
    $hwnd = $p.MainWindowHandle
    if ($hwnd -eq 0) { throw 'Fixture did not open a window' }
    # Allow PNG decoding and a presented frame before client capture.
    Start-Sleep -Seconds 2
    $capturePath = Join-Path $root 'capture.png'
    $metadataPath = Join-Path $root 'capture.json'
    $helperPath = Join-Path $PSScriptRoot 'test-windows-capture-client.ps1'
    $shellPath = (Get-Process -Id $PID).Path
    $capture = Start-Process -FilePath $shellPath -PassThru -ArgumentList @(
        '-NoProfile', '-NonInteractive', '-File', ('"' + $helperPath + '"'),
        '-WindowHandle', $hwnd.ToInt64(), '-ExpectedProcessId', $p.Id,
        '-OutputFile', ('"' + $capturePath + '"'), '-MetadataFile', ('"' + $metadataPath + '"')
    ) -RedirectStandardOutput (Join-Path $root 'capture.stdout.txt') -RedirectStandardError (Join-Path $root 'capture.stderr.txt')
    try {
        $null = $capture.Handle
        if (-not $capture.WaitForExit(15000)) { throw 'Client capture timed out after 15s' }
        if ($capture.ExitCode -ne 0) { throw "Client capture failed: exit $($capture.ExitCode); see capture.stderr.txt" }
    } finally {
        $capture.Refresh()
        if (-not $capture.HasExited) { $capture.Kill(); $capture.WaitForExit() }
        $capture.Dispose()
    }
    $metadata = Get-Content -LiteralPath $metadataPath -Raw | ConvertFrom-Json
    $scale = $metadata.dpi / 96.0
    if ($scale -le 0) { throw 'Invalid window DPI' }
    $bitmap = [Drawing.Bitmap]::new($capturePath)
    $measurements = [ordered]@{ dpi = ($scale * 96); width = $bitmap.Width; height = $bitmap.Height }
    function Sample([string]$name, [double]$x, [double]$y) {
        $c = $bitmap.GetPixel([int][math]::Floor($x * $scale), [int][math]::Floor($y * $scale))
        $measurements[$name] = @([int]$c.R, [int]$c.G, [int]$c.B)
        return $c
    }
    function Assert-Color($actual, [int[]]$expected, [string]$name) {
        $rgb = @([int]$actual.R, [int]$actual.G, [int]$actual.B)
        for ($i = 0; $i -lt 3; $i++) {
            if ([math]::Abs($rgb[$i] - $expected[$i]) -gt 20) { throw "$name unexpected RGB: $rgb (expected $expected)" }
        }
    }
    try {
        Assert-Color (Sample 'background' 620 365) @(16,24,32) 'background'
        Assert-Color (Sample 'reference_quad' 480 130) @(255,64,32) 'reference quad'
        $center = Sample 'fade_quad_center' 160 130
        Assert-Color $center @(255,64,32) 'fade quad center'
        $edge = Sample 'fade_quad_edge' 44 130
        $middle = Sample 'fade_quad_middle' 60 130
        if (-not ($edge.R -lt $middle.R -and $middle.R -lt ($center.R - 20))) { throw 'Quad horizontal fade is missing or not progressive' }
        Assert-Color (Sample 'image_red' 100 255) @(240,64,64) 'atlas red quadrant'
        Assert-Color (Sample 'image_green' 220 255) @(64,240,128) 'atlas green quadrant'
        Assert-Color (Sample 'image_blue' 100 315) @(64,128,240) 'atlas blue quadrant'
        Assert-Color (Sample 'image_yellow' 220 315) @(240,208,64) 'atlas yellow quadrant'
        Assert-Color (Sample 'fade_image_center' 420 255) @(240,64,64) 'faded atlas center'
        $imageEdge = Sample 'fade_image_edge' 364 255
        if ($imageEdge.R -gt 100) { throw 'Image horizontal fade is missing' }
        Assert-Color (Sample 'panel' 590 430) @(34,51,68) 'opaque raised panel'
        Assert-Color (Sample 'vertical_quad_center' 160 540) @(32,128,240) 'vertical quad center'
        $topEdge = Sample 'vertical_quad_top_edge' 160 487
        $topMid = Sample 'vertical_quad_top_middle' 160 495
        $bottomEdge = Sample 'vertical_quad_bottom_edge' 160 603
        $bottomMid = Sample 'vertical_quad_bottom_middle' 160 585
        if (-not ($topEdge.B -lt $topMid.B -and $topMid.B -lt 220 -and $bottomEdge.B -lt $bottomMid.B -and $bottomMid.B -lt 220)) {
            throw 'Quad asymmetric top/bottom fades are missing or not progressive'
        }
        Assert-Color (Sample 'vertical_image_center' 420 540) @(240,64,64) 'vertical image center'
        $imageTop = Sample 'vertical_image_top_edge' 420 487
        $imageBottom = Sample 'vertical_image_bottom_edge' 420 603
        if ($imageTop.R -gt 100 -or $imageBottom.B -gt 100) { throw 'Image vertical fades are missing' }
        $bright = 0
        for ($y = 20; $y -lt 48; $y++) {
            for ($x = 40; $x -lt 580; $x++) {
                $c = $bitmap.GetPixel([int][math]::Floor($x * $scale), [int][math]::Floor($y * $scale))
                if ($c.R -gt 180 -and $c.G -gt 180 -and $c.B -gt 180) { $bright++ }
            }
        }
        $measurements['text_bright_pixels'] = $bright
        if ($bright -lt 50) { throw 'Text did not render in the expected region' }
    } finally { $measurements | ConvertTo-Json -Depth 4 | Set-Content -Encoding utf8 -LiteralPath (Join-Path $root 'measurements.json') }
    if (-not $p.CloseMainWindow() -or -not $p.WaitForExit(15000)) { throw 'Fixture did not close normally' }
    if ($p.ExitCode -ne 0) { throw "Fixture exited with $($p.ExitCode)" }
    Write-Host "PASS: native atlas, quad/image fades, text and panel pixels; evidence: $root"
} catch {
    Write-Host "FAIL: $($_.Exception.Message); evidence: $root"
    throw
} finally {
    if ($null -ne $bitmap) { $bitmap.Dispose() }
    $p.Refresh()
    if (-not $p.HasExited) { $p.Kill(); $p.WaitForExit() }
    $p.Dispose()
}
