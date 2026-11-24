# 用法：powershell -File tools\shot.ps1 <out.png> [x y w h]
Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;
public static class DpiHelper {
    [DllImport("user32.dll")] public static extern bool SetProcessDpiAwarenessContext(IntPtr value);
    [DllImport("user32.dll")] public static extern int GetSystemMetrics(int nIndex);
}
"@
[DpiHelper]::SetProcessDpiAwarenessContext([IntPtr](-4)) | Out-Null
Add-Type -AssemblyName System.Drawing
$x = 0; $y = 0; $w = [DpiHelper]::GetSystemMetrics(0); $h = [DpiHelper]::GetSystemMetrics(1)
if ($args.Count -ge 5) { $x = [int]$args[1]; $y = [int]$args[2]; $w = [int]$args[3]; $h = [int]$args[4] }
$out = $args[0]
$bmp = New-Object System.Drawing.Bitmap $w, $h
$g = [System.Drawing.Graphics]::FromImage($bmp)
$g.CopyFromScreen($x, $y, 0, 0, (New-Object System.Drawing.Size $w, $h))
$g.Dispose()
$bmp.Save($out, [System.Drawing.Imaging.ImageFormat]::Png)
$bmp.Dispose()
"wrote $out ($w x $h)"
