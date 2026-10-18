param([switch]$Quiet)
$ErrorActionPreference = 'Stop'
$install = [IO.Path]::GetFullPath($PSScriptRoot)
$expected = Join-Path $env:LOCALAPPDATA 'Programs\Airstream'
if ($install -ne [IO.Path]::GetFullPath($expected)) { throw 'Unexpected install directory; refusing removal.' }
if (-not $Quiet) {
    Add-Type -AssemblyName PresentationFramework
    $answer = [System.Windows.MessageBox]::Show('Uninstall Airstream? Playlists, preferences and prepared media will be kept.', 'Airstream', 'YesNo', 'Question')
    if ($answer -ne 'Yes') { exit 0 }
}
Get-CimInstance Win32_Process -Filter "name='airstream.exe'" | Where-Object { $_.ExecutablePath -eq (Join-Path $install 'airstream.exe') } | ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }
$desktop = [Environment]::GetFolderPath('Desktop')
$programs = [Environment]::GetFolderPath('Programs')
Remove-Item -LiteralPath (Join-Path $desktop 'Airstream.lnk') -Force -ErrorAction SilentlyContinue
Remove-Item -LiteralPath (Join-Path $programs 'Airstream') -Recurse -Force -ErrorAction SilentlyContinue
Remove-Item -LiteralPath 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\Airstream' -Recurse -Force -ErrorAction SilentlyContinue
# This script is loaded by PowerShell; deleting it does not affect its execution.
Remove-Item -LiteralPath $install -Recurse -Force
Write-Output 'Airstream uninstalled. User data preserved.'
