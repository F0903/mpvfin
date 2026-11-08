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

$output = & $resolvedMpv `
    --no-config `
    --terminal=yes `
    --msg-level=all=info `
    --vo=null `
    --ao=null `
    --length=0.1 `
    "--script=$pluginPath" `
    "av://lavfi:testsrc=size=16x16:rate=1" 2>&1
$exitCode = $LASTEXITCODE

$output | Write-Output

if ($exitCode -ne 0) {
    throw "mpv exited with code $exitCode"
}

if (($output -join [Environment]::NewLine) -notmatch "\[mpvfin\] native C-plugin loaded") {
    throw "mpv exited without the mpvfin load sentinel"
}

Write-Output "mpvfin C-plugin smoke test passed."
