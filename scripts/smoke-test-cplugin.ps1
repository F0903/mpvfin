param(
    [Parameter(Mandatory = $true)]
    [string] $MpvPath,

    [ValidateSet("debug", "release")]
    [string] $Configuration = "debug"
)

$ErrorActionPreference = "Stop"

$repoRoot = Split-Path -Parent $PSScriptRoot
$pluginPath = Join-Path $repoRoot "target\$Configuration\mpvfin.dll"
$resolvedMpv = (Resolve-Path -LiteralPath $MpvPath).Path

if (-not (Test-Path -LiteralPath $pluginPath -PathType Leaf)) {
    throw "Plugin not found at '$pluginPath'. Build it with: cargo build -p mpvfin-cplugin"
}

# mpv shows the single 1 fps frame as an image, for 5 s by default; with
# --image-display-duration=0 the 5 s limit measures the plugin's shutdown.
$stopwatch = [Diagnostics.Stopwatch]::StartNew()
$output = & $resolvedMpv `
    --no-config `
    --terminal=yes `
    --msg-level=all=info `
    --vo=null `
    --ao=null `
    --length=0.1 `
    --image-display-duration=0 `
    "--script=$pluginPath" `
    "av://lavfi:testsrc=size=16x16:rate=1" 2>&1
$exitCode = $LASTEXITCODE
$stopwatch.Stop()

$output | Write-Output

if ($exitCode -ne 0) {
    throw "mpv exited with code $exitCode"
}

$text = $output -join [Environment]::NewLine
foreach ($sentinel in "[mpvfin] loaded", "[mpvfin] no server configured") {
    if (-not $text.Contains($sentinel)) {
        throw "mpv exited without the sentinel '$sentinel'"
    }
}

$seconds = $stopwatch.Elapsed.TotalSeconds
Write-Output ("mpv ran for {0:N2} s" -f $seconds)
if ($seconds -gt 5) {
    throw "mpv took longer than 5 s to exit"
}

Write-Output "mpvfin C-plugin smoke test passed."
