$ErrorActionPreference = 'Stop'
$binary = (Resolve-Path 'dist/bin/unified-hifi-win64.exe').Path
$version = & $binary --version
if ($LASTEXITCODE -ne 0 -or $version -ne "unified-hifi-control $env:UHC_VERSION ($env:UHC_GIT_SHA)") {
    throw "Windows executable/version check failed: $version"
}
# Deliberately invalid arguments exercise each helper's loader and Rust entrypoint
# without initiating pairing or external provider requests.
foreach ($name in @('uhc-hiphi-pair-win64.exe', 'uhc-music-details-win64.exe')) {
    $info = [System.Diagnostics.ProcessStartInfo]::new((Resolve-Path "dist/bin/$name").Path)
    $info.ArgumentList.Add('invalid'); $info.ArgumentList.Add('extra')
    $info.RedirectStandardOutput = $true; $info.RedirectStandardError = $true
    $process = [System.Diagnostics.Process]::Start($info)
    try {
        if (-not $process.WaitForExit(10000)) { throw "$name did not exit" }
        $stderr = $process.StandardError.ReadToEnd()
        if ($process.ExitCode -ne 1 -or $stderr -notmatch 'usage:') {
            throw "$name failed its loader/entrypoint check (exit $($process.ExitCode)): $stderr"
        }
    } finally {
        if (-not $process.HasExited) { $process.Kill() }
        $process.Dispose()
    }
}
$env:PORT = '18088'
$env:CONFIG_DIR = Join-Path $env:RUNNER_TEMP 'uhc-windows-smoke'
New-Item -ItemType Directory -Force $env:CONFIG_DIR | Out-Null
$stdout = Join-Path $env:RUNNER_TEMP 'uhc-windows.stdout.log'
$stderr = Join-Path $env:RUNNER_TEMP 'uhc-windows.stderr.log'
$server = Start-Process $binary -PassThru -RedirectStandardOutput $stdout -RedirectStandardError $stderr
try {
    $ready = $false
    for ($i = 0; $i -lt 60; $i++) {
        if ($server.HasExited) { throw 'Windows server exited before becoming ready' }
        try {
            $status = Invoke-RestMethod 'http://127.0.0.1:18088/status' -TimeoutSec 2
            $ready = $true; break
        } catch { Start-Sleep -Seconds 1 }
    }
    if (-not $ready) { throw 'Windows server readiness timed out' }
    if ($status.version -ne $env:UHC_VERSION -or $status.git_sha -ne $env:UHC_GIT_SHA) {
        throw "Windows server contains stale version/SHA: $($status | ConvertTo-Json -Compress)"
    }
    $page = Invoke-WebRequest 'http://127.0.0.1:18088/' -TimeoutSec 10
    if ($page.StatusCode -ne 200 -or $page.Content -notmatch '<title>') { throw 'Embedded HTML missing' }
    $assets = [regex]::Matches($page.Content, '/assets/[^"\s<>]+\.js')
    if ($assets.Count -eq 0) { throw 'Embedded JavaScript asset link missing' }
    $asset = Invoke-WebRequest ("http://127.0.0.1:18088" + $assets[0].Value) -TimeoutSec 10
    if ($asset.StatusCode -ne 200 -or $asset.RawContentLength -eq 0) { throw 'Embedded JavaScript asset missing' }
    Write-Host 'Native Windows loader, helper entrypoints, HTTP, version/SHA and embedded assets passed'
} catch {
    if (Test-Path $stdout) { Get-Content $stdout -Tail 50 }
    if (Test-Path $stderr) { Get-Content $stderr -Tail 50 }
    throw
} finally {
    if (-not $server.HasExited) { Stop-Process -Id $server.Id -Force }
}
