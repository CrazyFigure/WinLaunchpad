# 焦点/键盘交互测试
Add-Type -TypeDefinition @"
using System; using System.Text; using System.Runtime.InteropServices;
public static class WG {
  [StructLayout(LayoutKind.Sequential)] public struct GUITHREADINFO { public int cbSize; public uint flags; public IntPtr hwndActive, hwndFocus, hwndCapture, hwndMenuOwner, hwndMoveSize, hwndCaret; public RECT rcCaret; }
  public struct RECT { public int L, T, R, B; }
  [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll")] public static extern bool GetGUIThreadInfo(uint tid, ref GUITHREADINFO info);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetClassName(IntPtr h, StringBuilder s, int n);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll")] public static extern bool SetProcessDpiAwarenessContext(IntPtr v);
  public static string Focus() { uint pid; var fg = GetForegroundWindow(); var t = GetWindowThreadProcessId(fg, out pid); var gi = new GUITHREADINFO(); gi.cbSize = Marshal.SizeOf(gi); GetGUIThreadInfo(t, ref gi); var sb = new StringBuilder(256); GetClassName(gi.hwndFocus, sb, 256); var fb = new StringBuilder(256); GetClassName(fg, fb, 256); return "fg=" + fb + " focus=" + sb + " fgVisible=" + IsWindowVisible(fg); }
}
"@
[WG]::SetProcessDpiAwarenessContext([IntPtr](-4)) | Out-Null
Add-Type -AssemblyName System.Windows.Forms
$exe = "C:\Software\WorkSpace\WinLaunchpad\target\release\winlaunchpad.exe"
Get-Process winlaunchpad -ErrorAction SilentlyContinue | Stop-Process -Force; Start-Sleep -Milliseconds 1500
Start-Process -FilePath $exe -ArgumentList "--background" -WindowStyle Hidden; Start-Sleep -Seconds 2
Start-Process -FilePath $exe -ArgumentList "--toggle" -WindowStyle Hidden; Start-Sleep -Milliseconds 1500
"after toggle: " + [WG]::Focus()
[System.Windows.Forms.SendKeys]::SendWait("note"); Start-Sleep -Milliseconds 800
"after typing: " + [WG]::Focus()
powershell -NoProfile -ExecutionPolicy Bypass -File "C:\Software\WorkSpace\WinLaunchpad\tools\shot.ps1" "C:\Software\WorkSpace\WinLaunchpad\.qoder-input-images\shot_search.png" 0 0 3072 1000
[System.Windows.Forms.SendKeys]::SendWait("{ESC}"); Start-Sleep -Milliseconds 800
"after Esc: " + [WG]::Focus()
