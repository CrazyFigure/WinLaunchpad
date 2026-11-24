# 生成 assets/app.ico：渐变圆角方块 + 3x3 白色方块网格（启动台）。
# 用法：powershell -ExecutionPolicy Bypass -File tools\make_icon.ps1
Add-Type -AssemblyName System.Drawing

function New-IconPng([int]$size) {
    $base = 256
    $bmp = New-Object System.Drawing.Bitmap $base, $base
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.SmoothingMode = 'AntiAlias'
    $g.Clear([System.Drawing.Color]::Transparent)

    # 背景圆角方块
    $rect = New-Object System.Drawing.Rectangle 8, 8, ($base - 16), ($base - 16)
    $path = New-Object System.Drawing.Drawing2D.GraphicsPath
    $r = 58
    $path.AddArc($rect.X, $rect.Y, $r, $r, 180, 90)
    $path.AddArc($rect.Right - $r, $rect.Y, $r, $r, 270, 90)
    $path.AddArc($rect.Right - $r, $rect.Bottom - $r, $r, $r, 0, 90)
    $path.AddArc($rect.X, $rect.Bottom - $r, $r, $r, 90, 90)
    $path.CloseFigure()
    $c1 = [System.Drawing.Color]::FromArgb(255, 0x4F, 0x7C, 0xFF)
    $c2 = [System.Drawing.Color]::FromArgb(255, 0x7B, 0x3F, 0xE4)
    $brush = New-Object System.Drawing.Drawing2D.LinearGradientBrush $rect, $c1, $c2, 45.0
    $g.FillPath($brush, $path)

    # 3x3 网格
    $cell = 52
    $gap = 16
    $total = $cell * 3 + $gap * 2
    $off = ($base - $total) / 2
    $white = New-Object System.Drawing.SolidBrush ([System.Drawing.Color]::FromArgb(255, 255, 255, 255))
    for ($row = 0; $row -lt 3; $row++) {
        for ($col = 0; $col -lt 3; $col++) {
            $x = $off + $col * ($cell + $gap)
            $y = $off + $row * ($cell + $gap)
            $sp = New-Object System.Drawing.Drawing2D.GraphicsPath
            $sr = 12
            $sp.AddArc($x, $y, $sr, $sr, 180, 90)
            $sp.AddArc($x + $cell - $sr, $y, $sr, $sr, 270, 90)
            $sp.AddArc($x + $cell - $sr, $y + $cell - $sr, $sr, $sr, 0, 90)
            $sp.AddArc($x, $y + $cell - $sr, $sr, $sr, 90, 90)
            $sp.CloseFigure()
            $g.FillPath($white, $sp)
            $sp.Dispose()
        }
    }
    $g.Dispose()

    if ($size -ne $base) {
        $small = New-Object System.Drawing.Bitmap $size, $size
        $g2 = [System.Drawing.Graphics]::FromImage($small)
        $g2.InterpolationMode = 'HighQualityBicubic'
        $g2.SmoothingMode = 'AntiAlias'
        $g2.PixelOffsetMode = 'HighQuality'
        $g2.DrawImage($bmp, 0, 0, $size, $size)
        $g2.Dispose()
        $bmp.Dispose()
        $bmp = $small
    }
    $ms = New-Object System.IO.MemoryStream
    $bmp.Save($ms, [System.Drawing.Imaging.ImageFormat]::Png)
    $bmp.Dispose()
    ,$ms.ToArray()
}

$sizes = @(16, 24, 32, 48, 64, 128, 256)
$images = @()
foreach ($s in $sizes) { $png = New-IconPng $s; $images += ,@($s, $png) }

$out = Join-Path $PSScriptRoot '..\assets\app.ico'
$fs = [System.IO.File]::Create($out)
$bw = New-Object System.IO.BinaryWriter $fs
$bw.Write([uint16]0)
$bw.Write([uint16]1)
$bw.Write([uint16]$sizes.Count)
$offset = 6 + 16 * $sizes.Count
foreach ($img in $images) {
    $s = $img[0]; $data = $img[1]
    $dim = $s; if ($s -ge 256) { $dim = 0 }
    $bw.Write([byte]$dim)
    $bw.Write([byte]$dim)
    $bw.Write([byte]0)
    $bw.Write([byte]0)
    $bw.Write([uint16]1)
    $bw.Write([uint16]32)
    $bw.Write([uint32]$data.Length)
    $bw.Write([uint32]$offset)
    $offset += $data.Length
}
foreach ($img in $images) { $bw.Write($img[1]) }
$bw.Flush()
$fs.Dispose()
Write-Host "wrote $out"

