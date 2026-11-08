param([Parameter(Mandatory)][ValidateSet("up", "down", "reset")][string]$Action)
$ErrorActionPreference = "Stop"
$root = Split-Path $PSScriptRoot -Parent
$compose = Join-Path $root "tests/server/compose.yml"
$base = "http://127.0.0.1:8096"
function dc { docker compose -f $compose @args; if ($LASTEXITCODE) { throw "docker compose $args failed" } }

if ($Action -eq "down") { dc stop; return }
if ($Action -eq "reset") { dc down -v; return }

dc up -d
$ready = $false
for ($i = 0; $i -lt 90 -and -not $ready; $i++) {
    try { Invoke-RestMethod "$base/System/Info/Public" | Out-Null; $ready = $true } catch { Start-Sleep 2 }
}
if (-not $ready) { throw "server did not become ready" }

# Media: generated in-container once.
dc exec -T jellyfin mkdir -p /media/shows
$have = @(dc exec -T jellyfin find /media/shows -name "*.mkv").Count
if ($have -lt 3) {
    $subs = Join-Path $root "tests/server/subs"
    dc exec -T jellyfin mkdir -p /media/shows/subs "/media/shows/Test Show/Season 1"
    foreach ($l in "en", "de", "fr") { dc cp "$subs/$l.srt" "jellyfin:/media/shows/subs/$l.srt" }
    $dir = "/media/shows/Test Show/Season 1"
    foreach ($n in 1..3) {
        $out = "$dir/Test Show - S01E0$n.mkv"
        $ff = "-y","-loglevel","error","-f","lavfi","-i","testsrc2=size=320x180:rate=24","-f","lavfi","-i","sine=frequency=440","-f","lavfi","-i","sine=frequency=880",
            "-i","/media/shows/subs/en.srt","-i","/media/shows/subs/de.srt","-t","120",
            "-map","0:v","-map","1:a","-map","2:a","-map","3","-map","4","-c:v","libx264","-pix_fmt","yuv420p","-c:a","aac","-c:s","srt",
            "-metadata:s:a:0","language=eng","-metadata:s:a:1","language=ger","-metadata:s:s:0","language=eng","-metadata:s:s:1","language=ger",$out
        dc exec -T jellyfin /usr/lib/jellyfin-ffmpeg/ffmpeg @ff
    }
    dc cp "$subs/fr.srt" "jellyfin:$dir/Test Show - S01E01.fr.srt"
    dc exec -T jellyfin rm -r /media/shows/subs
}

# Startup wizard (throwaway local server, hardcoded admin).
if (-not (Invoke-RestMethod "$base/System/Info/Public").StartupWizardCompleted) {
    Invoke-RestMethod "$base/Startup/Configuration" -Method Post -ContentType "application/json" -Body '{"UICulture":"en-US","MetadataCountryCode":"US","PreferredMetadataLanguage":"en"}' | Out-Null
    Invoke-RestMethod "$base/Startup/User" | Out-Null # creates the default user record the next call renames
    Invoke-RestMethod "$base/Startup/User" -Method Post -ContentType "application/json" -Body '{"Name":"admin","Password":"mpvfin-test"}' | Out-Null
    Invoke-RestMethod "$base/Startup/RemoteAccess" -Method Post -ContentType "application/json" -Body '{"EnableRemoteAccess":true,"EnableAutomaticPortMapping":false}' | Out-Null
    Invoke-RestMethod "$base/Startup/Complete" -Method Post | Out-Null
}

$auth = 'MediaBrowser Client="mpvfin-test", Device="test-server", DeviceId="mpvfin-test-server", Version="1"'
$login = Invoke-RestMethod "$base/Users/AuthenticateByName" -Method Post -ContentType "application/json" `
    -Headers @{ "X-Emby-Authorization" = $auth } -Body '{"Username":"admin","Pw":"mpvfin-test"}'
$h = @{ "X-Emby-Authorization" = "$auth, Token=`"$($login.AccessToken)`"" }

$cfg = Invoke-RestMethod "$base/System/Configuration" -Headers $h
if (-not $cfg.QuickConnectAvailable -or $cfg.MinResumeDurationSeconds -ne 0) {
    $cfg.QuickConnectAvailable = $true
    $cfg.MinResumeDurationSeconds = 0 # the default 300 s never saves a resume position on 120 s episodes
    Invoke-RestMethod "$base/System/Configuration" -Method Post -Headers $h -ContentType "application/json" -Body ($cfg | ConvertTo-Json -Depth 20) | Out-Null
}

$folders = Invoke-RestMethod "$base/Library/VirtualFolders" -Headers $h
if (-not ($folders | Where-Object Name -eq "Shows")) {
    Invoke-RestMethod "$base/Library/VirtualFolders?name=Shows&collectionType=tvshows&refreshLibrary=true" -Method Post -Headers $h `
        -ContentType "application/json" -Body '{"LibraryOptions":{"PathInfos":[{"Path":"/media/shows"}]}}' | Out-Null
}

$count = 0
for ($i = 0; $i -lt 90 -and $count -lt 3; $i++) {
    $count = (Invoke-RestMethod "$base/Items?Recursive=true&IncludeItemTypes=Episode" -Headers $h).TotalRecordCount
    if ($count -lt 3) { Start-Sleep 2 }
}
if ($count -ne 3) { throw "expected 3 episodes, got $count" }
"Test server ready: $count episodes"
