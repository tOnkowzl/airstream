param(
    [string]$SourceExe = (Join-Path $PSScriptRoot '..\dist\airstream.exe'),
    [string]$MediaToolsDirectory = '',
    [switch]$Launch
)
$ErrorActionPreference = 'Stop'
$source = (Resolve-Path -LiteralPath $SourceExe).Path
$install = Join-Path $env:LOCALAPPDATA 'Programs\Airstream'
$target = Join-Path $install 'airstream.exe'
$version = (Get-Item -LiteralPath $source).VersionInfo.ProductVersion; if (-not $version) { $version = (Get-Item -LiteralPath $source).VersionInfo.FileVersion }
if (-not $MediaToolsDirectory) {
    $MediaToolsDirectory = Split-Path (Get-Command ffmpeg.exe -ErrorAction Stop).Source
}
foreach ($name in @('ffmpeg.exe','ffprobe.exe')) {
    if (-not (Test-Path -LiteralPath (Join-Path $MediaToolsDirectory $name))) { throw "Missing $name" }
}
Get-CimInstance Win32_Process -Filter "name='airstream.exe'" | Where-Object { $_.ExecutablePath -eq $target -or $_.ExecutablePath -eq (Join-Path ([Environment]::GetFolderPath('Desktop')) 'airstream.exe') } | ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }
Get-Process -Name airstream -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq $target } | Wait-Process -Timeout 10 -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Path $install -Force | Out-Null
if ($source -ne $target) { Copy-Item -LiteralPath $source -Destination $target -Force }
foreach ($name in @('ffmpeg.exe','ffprobe.exe')) {
    $src = [IO.Path]::GetFullPath((Join-Path $MediaToolsDirectory $name))
    $dst = Join-Path $install $name
    if ($src -ne $dst) {
        Remove-Item -LiteralPath $dst -Force -ErrorAction SilentlyContinue
        # Same-volume NTFS hard links keep private runtimes without duplicating
        # hundreds of MB. Removing the original package does not remove this link.
        try { New-Item -ItemType HardLink -Path $dst -Value $src -ErrorAction Stop | Out-Null }
        catch { Copy-Item -LiteralPath $src -Destination $dst -Force }
    }
}
Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'uninstall.ps1') -Destination (Join-Path $install 'uninstall.ps1') -Force
$desktop = [Environment]::GetFolderPath('Desktop')
$programs = Join-Path ([Environment]::GetFolderPath('Programs')) 'Airstream'
New-Item -ItemType Directory -Path $programs -Force | Out-Null
$shell = New-Object -ComObject WScript.Shell
foreach ($shortcutPath in @((Join-Path $desktop 'Airstream.lnk'), (Join-Path $programs 'Airstream.lnk'))) {
    $shortcut = $shell.CreateShortcut($shortcutPath)
    $shortcut.TargetPath = $target
    $shortcut.WorkingDirectory = $install
    $shortcut.IconLocation = "$target,0"
    $shortcut.Description = 'Airstream - original-quality local media caster'
    $shortcut.Save()
}
$powerShell = Join-Path $env:SystemRoot 'System32\WindowsPowerShell\v1.0\powershell.exe'
$uninstallCommand = '"{0}" -NoProfile -File "{1}"' -f $powerShell, (Join-Path $install 'uninstall.ps1')
$key = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\Airstream'
New-Item -Path $key -Force | Out-Null
$values = @{ DisplayName='Airstream'; DisplayVersion=$version; Publisher='Airstream'; InstallLocation=$install; DisplayIcon="$target,0"; UninstallString=$uninstallCommand; QuietUninstallString="$uninstallCommand -Quiet"; URLInfoAbout='https://github.com/tOnkowzl/airstream' }
foreach ($name in $values.Keys) { New-ItemProperty -Path $key -Name $name -Value $values[$name] -PropertyType String -Force | Out-Null }
foreach ($name in @('NoModify','NoRepair')) { New-ItemProperty -Path $key -Name $name -Value 1 -PropertyType DWord -Force | Out-Null }
$size = [int][Math]::Ceiling((Get-ChildItem -LiteralPath $install -File | Measure-Object -Property Length -Sum).Sum / 1KB)
New-ItemProperty -Path $key -Name EstimatedSize -Value $size -PropertyType DWord -Force | Out-Null
$oldPortable = Join-Path $desktop 'airstream.exe'
if ((Test-Path -LiteralPath $oldPortable) -and $oldPortable -ne $target) {
    $backupDir = Join-Path $env:LOCALAPPDATA 'Airstream\backups'
    New-Item -ItemType Directory -Path $backupDir -Force | Out-Null
    $stamp = Get-Date -Format 'yyyyMMdd-HHmmss-fff'
    Move-Item -LiteralPath $oldPortable -Destination (Join-Path $backupDir "airstream-portable-$stamp.exe")
}
Write-Output "Installed Airstream $version at $install"
Write-Output "SHA256: $((Get-FileHash -LiteralPath $target -Algorithm SHA256).Hash)"
if ($Launch) { Start-Process -FilePath $target -WorkingDirectory $install }
