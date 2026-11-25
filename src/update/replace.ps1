param([int]$ParentId, [string]$TargetExe)

$ErrorActionPreference = 'Stop'
$SourceExe = Join-Path $PSScriptRoot 'new.exe'
$BackupExe = Join-Path $PSScriptRoot 'old.exe'
$Parent = Get-Process -Id $ParentId -ErrorAction SilentlyContinue
$Replaced = $false

function Start-WinLaunchpad {
    # 文件路径可包含空格和方括号，使用原生进程 API 传入字面路径。
    $StartInfo = New-Object System.Diagnostics.ProcessStartInfo
    $StartInfo.FileName = $TargetExe
    $StartInfo.WorkingDirectory = [System.IO.Path]::GetDirectoryName($TargetExe)
    $StartInfo.Arguments = '--settings'
    $StartInfo.UseShellExecute = $false
    [void][System.Diagnostics.Process]::Start($StartInfo)
}

try {
    # 等待旧进程释放 EXE 和单实例互斥后才执行同卷原子替换。
    if ($Parent -and -not $Parent.WaitForExit(30000)) { throw '旧版 WinLaunchpad 未能退出。' }
    [System.IO.File]::Replace($SourceExe, $TargetExe, $BackupExe)
    $Replaced = $true
    Start-WinLaunchpad
    $Replaced = $false

    # 安装版仍通过已有的验签流程替换 EXE，仅刷新属于此目录的卸载记录。
    $UninstallKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\WinLaunchpad'
    $Installation = Get-ItemProperty -LiteralPath $UninstallKey -ErrorAction SilentlyContinue
    if ($Installation.InstallLocation -ieq [System.IO.Path]::GetDirectoryName($TargetExe)) {
        $Version = (Get-Item -LiteralPath $TargetExe -ErrorAction SilentlyContinue).VersionInfo.FileVersion
        if ($Version) { Set-ItemProperty -LiteralPath $UninstallKey -Name 'DisplayVersion' -Value $Version -ErrorAction SilentlyContinue }
    }
} catch {
    $Failure = $_.Exception.Message
    if ($Replaced) {
        try {
            [System.IO.File]::Replace($BackupExe, $TargetExe, [NullString]::Value)
            $Replaced = $false
        } catch {
            $Failure = "$Failure`n$($_.Exception.Message)`n旧版文件位于 $BackupExe"
        }
    } elseif (Test-Path -LiteralPath $BackupExe) {
        # 替换调用异常时保留可能已生成的备份，目标缺失时优先恢复原文件。
        $Replaced = $true
        if (-not (Test-Path -LiteralPath $TargetExe)) {
            try {
                [System.IO.File]::Move($BackupExe, $TargetExe)
                $Replaced = $false
            } catch {
                $Failure = "$Failure`n$($_.Exception.Message)"
            }
        }
        if ($Replaced) { $Failure = "$Failure`n旧版文件位于 $BackupExe" }
    }
    Add-Type -AssemblyName System.Windows.Forms
    [void][System.Windows.Forms.MessageBox]::Show($Failure, 'WinLaunchpad 更新失败')
    if (-not $Replaced -and (-not $Parent -or $Parent.HasExited)) { Start-WinLaunchpad }
} finally {
    if (-not $Replaced) { Remove-Item -LiteralPath $BackupExe -Force -ErrorAction SilentlyContinue }
    Remove-Item -LiteralPath $SourceExe, $PSCommandPath -Force -ErrorAction SilentlyContinue
    if (-not $Replaced) { Remove-Item -LiteralPath $PSScriptRoot -Force -ErrorAction SilentlyContinue }
}
