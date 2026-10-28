param(
    [ValidateSet("debug", "release")]
    [string]$Profile = "release",
    [string]$Version = "",
    [switch]$NoClean
)

$ErrorActionPreference = "Stop"
$Root = Split-Path -Parent $PSScriptRoot
$Dist = Join-Path $Root "dist"
$CargoToml = Join-Path $Root "Cargo.toml"
$VersionFile = Join-Path $Root ".macscp-build-version"

if (-not $Version -and (Test-Path $VersionFile)) {
    $Version = (Get-Content $VersionFile -Raw).Trim()
}
if (-not $Version) {
    $Match = [regex]::Match((Get-Content $CargoToml -Raw), '(?m)^version\s*=\s*"([^"]+)"')
    if (-not $Match.Success) { throw "无法从 Cargo.toml 读取版本号" }
    $Version = $Match.Groups[1].Value
}
$Version = $Version.TrimStart("v")
if ($Version -notmatch '^[0-9]+\.[0-9]+\.[0-9]+([.-][0-9A-Za-z][0-9A-Za-z.-]*)?$') {
    throw "版本号无效：$Version（应类似 0.2.0）"
}

if (-not $NoClean -and (Test-Path $Dist)) {
    if ((Split-Path $Dist -Leaf) -ne "dist") { throw "拒绝清空非 dist 目录：$Dist" }
    Remove-Item $Dist -Recurse -Force
}
New-Item $Dist -ItemType Directory -Force | Out-Null

$CargoArgs = @("build", "--locked")
if ($Profile -eq "release") { $CargoArgs += "--release" }
if ($Profile -eq "release" -and -not $env:GPUI_FXC_PATH) {
    $WindowsKits = Join-Path ${env:ProgramFiles(x86)} "Windows Kits/10/bin"
    $Fxc = Get-ChildItem -Path $WindowsKits -Filter "fxc.exe" -Recurse -ErrorAction SilentlyContinue |
        Where-Object { $_.FullName -match '[\\/]x64[\\/]fxc\.exe$' } |
        Sort-Object FullName -Descending |
        Select-Object -First 1
    if (-not $Fxc) { throw "找不到 Windows SDK fxc.exe，无法编译 GPUI release 着色器" }
    $env:GPUI_FXC_PATH = $Fxc.FullName
}
Write-Host "→ cargo build $Profile"
& cargo @CargoArgs
if ($LASTEXITCODE -ne 0) { throw "cargo build 失败" }

$Arch = if ($env:PROCESSOR_ARCHITECTURE -eq "ARM64") { "arm64" } else { "x86_64" }
$Stage = Join-Path ([System.IO.Path]::GetTempPath()) ("blip-windows-" + [guid]::NewGuid())
$Archive = Join-Path $Dist "Blip-$Version-windows-$Arch.zip"
try {
    New-Item $Stage -ItemType Directory | Out-Null
    Copy-Item (Join-Path $Root "target/$Profile/blip.exe") (Join-Path $Stage "Blip.exe")
    Copy-Item (Join-Path $Root "README.md") (Join-Path $Stage "README.md")
    Compress-Archive -Path (Join-Path $Stage "*") -DestinationPath $Archive -Force
} finally {
    if (Test-Path $Stage) { Remove-Item $Stage -Recurse -Force }
}

Set-Content -Path $VersionFile -Value $Version
Write-Host "完成：$Archive"
