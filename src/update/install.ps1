param([int]$ParentId, [string]$TargetExe)

$ErrorActionPreference = 'Stop'
$SetupExe = Join-Path $PSScriptRoot 'setup.exe'
$BackupExe = Join-Path $PSScriptRoot 'old.exe'
$Parent = Get-Process -Id $ParentId -ErrorAction SilentlyContinue
$KeepBackup = $false
$ParentExited = $false
$BackupReady = $false

function Start-Program([string]$Path, [string]$Arguments) {
    # 直接启动程序，路径中的空格和方括号按字面值传给 Windows。
    $StartInfo = New-Object System.Diagnostics.ProcessStartInfo
    $StartInfo.FileName = $Path
    $StartInfo.WorkingDirectory = [System.IO.Path]::GetDirectoryName($Path)
    $StartInfo.Arguments = $Arguments
    $StartInfo.UseShellExecute = $false
    [System.Diagnostics.Process]::Start($StartInfo)
}

try {
    # Windows 释放旧文件和单实例互斥后再运行安装包。
    if ($Parent -and -not $Parent.WaitForExit(30000)) { throw '旧版 WinLaunchpad 未能退出。' }
    $ParentExited = $true

    $UninstallKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\WinLaunchpad'
    $Installation = Get-ItemProperty -LiteralPath $UninstallKey -ErrorAction SilentlyContinue
    $InstallDir = [System.IO.Path]::GetDirectoryName($TargetExe)
    $Installed = $Installation.InstallLocation -and
        ([System.IO.Path]::GetFullPath((Join-Path $Installation.InstallLocation 'WinLaunchpad.exe')) -ieq $TargetExe)
    $SetupArgs = ''
    if ($Installed) {
        # 短时扫描和图标进程也可能占用原 EXE，只等待此安装路径下的进程。
        $Workers = Get-Process -Name 'WinLaunchpad' -ErrorAction SilentlyContinue |
            Where-Object { $_.Path -ieq $TargetExe }
        foreach ($Worker in $Workers) {
            if (-not $Worker.WaitForExit(15000)) { throw '旧版 WinLaunchpad 后台任务未能退出。' }
        }
        # 将旧 EXE 同卷迁移为备份，安装失败时恢复。/D 放在末尾并保持原始路径。
        [System.IO.File]::Move($TargetExe, $BackupExe)
        $BackupReady = $true
        $SetupArgs = "/S /D=$InstallDir"
    }

    $SetupProcess = Start-Program $SetupExe $SetupArgs
    $SetupProcess.WaitForExit()
    if (-not $Installed -and $SetupProcess.ExitCode -eq 1) {
        # 非安装版通过向导迁移。取消安装时重新打开原程序。
        [void](Start-Program $TargetExe '--settings')
        return
    }
    if ($SetupProcess.ExitCode -ne 0) { throw "安装程序退出码：$($SetupProcess.ExitCode)" }
    if ($Installed) {
        $Expected = (Get-Item -LiteralPath $SetupExe).VersionInfo.FileVersion
        if ((Get-Item -LiteralPath $TargetExe).VersionInfo.FileVersion -cne $Expected) {
            throw '安装完成后的应用版本与安装包不一致。'
        }
        [void](Start-Program $TargetExe '--settings')
    }
} catch {
    $Failure = $_.Exception.Message
    if ($BackupReady) {
        try {
            if (Test-Path -LiteralPath $TargetExe -PathType Leaf) {
                [System.IO.File]::Replace($BackupExe, $TargetExe, [NullString]::Value)
            } else {
                [System.IO.File]::Move($BackupExe, $TargetExe)
            }
            if ($Installation.DisplayVersion) {
                Set-ItemProperty -LiteralPath $UninstallKey -Name 'DisplayVersion' -Value $Installation.DisplayVersion -ErrorAction SilentlyContinue
            }
        } catch {
            $KeepBackup = $true
            $Failure = "$Failure`n$($_.Exception.Message)`n旧版文件位于 $BackupExe"
        }
    }
    Add-Type -AssemblyName System.Windows.Forms
    [void][System.Windows.Forms.MessageBox]::Show($Failure, 'WinLaunchpad 更新失败')
    if (-not $KeepBackup -and $ParentExited) { [void](Start-Program $TargetExe '--settings') }
} finally {
    Remove-Item -LiteralPath $SetupExe, $PSCommandPath -Force -ErrorAction SilentlyContinue
    if (-not $KeepBackup) {
        Remove-Item -LiteralPath $BackupExe -Force -ErrorAction SilentlyContinue
        Remove-Item -LiteralPath $PSScriptRoot -Force -ErrorAction SilentlyContinue
    }
}
