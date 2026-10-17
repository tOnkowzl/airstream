# build a static airstream.exe with the GNU toolchain (no Visual Studio needed)
$ErrorActionPreference = "Stop"

$mingw = Join-Path $env:LOCALAPPDATA "Microsoft\WinGet\Packages\BrechtSanders.WinLibs.POSIX.UCRT_Microsoft.Winget.Source_8wekyb3d8bbwe\mingw64\bin"
$shim  = ""
if (Test-Path $mingw) { $env:PATH = "$mingw;$env:PATH" }

rustup toolchain install stable-x86_64-pc-windows-gnu --profile minimal 2>$null
cargo +stable-x86_64-pc-windows-gnu build --release

New-Item -ItemType Directory -Force -Path dist | Out-Null
Copy-Item target\release\airstream.exe dist\airstream.exe -Force
Write-Host "built -> dist\airstream.exe"
