# Generates smoothkvm.ico (a round blue badge with a white centre) next to this script.
Add-Type -AssemblyName System.Drawing
$size = 48
$bmp = New-Object System.Drawing.Bitmap $size, $size, ([System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
$g = [System.Drawing.Graphics]::FromImage($bmp)
$g.SmoothingMode = 'AntiAlias'
$g.Clear([System.Drawing.Color]::Transparent)
$g.FillEllipse((New-Object System.Drawing.SolidBrush ([System.Drawing.Color]::FromArgb(255, 0x3B, 0x6D, 0xF0))), 1, 1, $size - 2, $size - 2)
$g.FillEllipse([System.Drawing.Brushes]::White, 17, 17, 14, 14)
$g.Dispose()
$icon = [System.Drawing.Icon]::FromHandle($bmp.GetHicon())
$out = Join-Path $PSScriptRoot 'smoothkvm.ico'
$fs = [System.IO.File]::Create($out)
$icon.Save($fs)
$fs.Close()
Write-Host "wrote $out"
