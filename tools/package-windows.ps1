param(
    [Parameter(Mandatory)][string]$Executable,
    [string]$OutputDirectory = (Join-Path $PSScriptRoot '../dist'),
    [string]$Makensis = 'makensis.exe'
)

$ErrorActionPreference = 'Stop'
$AppExe = Get-Item -LiteralPath $Executable
$VersionInfo = $AppExe.VersionInfo
$Version = $VersionInfo.FileVersion
if ($Version -cnotmatch '^\d+\.\d+\.\d+(?:-[A-Za-z0-9.-]+)?$') {
    throw 'WinLaunchpad EXE 缺少有效版本资源。'
}
$PeVersion = "$($VersionInfo.FileMajorPart).$($VersionInfo.FileMinorPart).$($VersionInfo.FileBuildPart).$($VersionInfo.FilePrivatePart)"

# 使用应用自身的版本命名安装包，避免安装向导与 Release 附件的版本分离。
$Output = New-Item -Path $OutputDirectory -ItemType Directory -Force
$SetupExe = Join-Path $Output.FullName "WinLaunchpad_${Version}_windows_x64_setup.exe"
$Script = Join-Path $PSScriptRoot '../installer/windows.nsi'
$Compiler = (Get-Command -Name $Makensis -CommandType Application).Source
& $Compiler /V2 /WX /INPUTCHARSET UTF8 "/DAPP_EXE=$($AppExe.FullName)" "/DSETUP_EXE=$SetupExe" "/DAPP_VERSION=$Version" "/DPE_VERSION=$PeVersion" $Script | Out-Host
if ($LASTEXITCODE -ne 0 -or -not (Test-Path -LiteralPath $SetupExe -PathType Leaf)) {
    throw 'NSIS 安装包构建失败。'
}
Get-Item -LiteralPath $SetupExe
