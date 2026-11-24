# 交互测试：显示 → 输入关键词 → 截图 → Esc 收起 → 验证隐藏
Add-Type -TypeDefinition @"
using System; using System.Text; using System.Runtime.InteropServices;
public static class WT {
  public delegate bool EnumProc(IntPtr h, IntPtr l);
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc p, IntPtr l);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetClassName(IntPtr h, StringBuilder s, int n);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll")] public static extern bool SetProcessDpiAwarenessContext(IntPtr v);
  public static bool MainVisible(uint pid) {
    bool vis = false;
    EnumWindows((h, l) => { uint p; GetWindowThreadProcessId(h, out p); if (p == pid) { var cb = new StringBuilder(256); GetClassName(h, cb, 256); if (cb.ToString() == "WinLaunchpadMain") vis = IsWindowVisible(h); } return true; }, IntPtr.Zero);
    return vis;
  }
}
"@
[WT]::SetProcessDpiAwarenessContext([IntPtr](-4)) | Out-Null
Add-Type -AssemblyName System.Windows.Forms
$exe = "C:\Software\WorkSpace\WinLaunchpad\target\release\winlaunchpad.exe"
$proc = Get-Process winlaunchpad | Select-Object -First 1
"initial visible=$([WT]::MainVisible([uint32]$proc.Id))"
Start-Process -FilePath $exe -ArgumentList "--toggle" -WindowStyle Hidden; Start-Sleep -Milliseconds 1200
"after toggle visible=$([WT]::MainVisible([uint32]$proc.Id))"
[System.Windows.Forms.SendKeys]::SendWait("note"); Start-Sleep -Milliseconds 800
powershell -NoProfile -ExecutionPolicy Bypass -File "C:\Software\WorkSpace\WinLaunchpad\tools\shot.ps1" "C:\Software\WorkSpace\WinLaunchpad\.qoder-input-images\shot_search.png" 0 0 3072 1000
[System.Windows.Forms.SendKeys]::SendWait("{ESC}"); Start-Sleep -Milliseconds 800
"after Esc visible=$([WT]::MainVisible([uint32]$proc.Id))"
"startup lnk exists: $(Test-Path "$env:APPDATA\Microsoft\Windows\Start Menu\Programs\Startup\WinLaunchpad.lnk")"
$proc2 = Get-Process -Id $proc.Id; "memory after hide: WS=$([math]::Round($proc2.WorkingSet64/1MB,1))MB Private=$([math]::Round($proc2.PrivateMemorySize64/1MB,1))MB"
