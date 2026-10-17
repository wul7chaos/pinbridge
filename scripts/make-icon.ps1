<#
.SYNOPSIS
  生成应用图标（多尺寸 `.ico` + 一张 512 PNG），**可复现**，不引入来路不明的二进制。

.DESCRIPTION
  为什么要有这个脚本：仓库里原来只有一个 32×32 的占位 `.ico`（够 `tauri dev` 用，
  但打包安装器需要 256×256）。直接塞一个二进制进仓库没人能复核它；而这个图标只是
  "一个桥 + 缩写"，用脚本画出来既够用又**可重放**（谁都能跑一遍拿到同样的东西）。

  ICO 结构按规范手写：ICONDIR + N×(ICONDIRENTRY 16 字节) + 各尺寸的 PNG 负载。
  256×256 的宽高字段写 0（规范如此，不是笔误）。

.PARAMETER OutputDir
  输出目录（默认 apps/desktop/src-tauri/icons）。
#>
[CmdletBinding()]
param(
    [string]$OutputDir = 'apps/desktop/src-tauri/icons'
)

$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing

$root = Split-Path -Parent $PSScriptRoot
$dir = Join-Path $root $OutputDir
if (-not (Test-Path $dir)) { New-Item -ItemType Directory -Path $dir -Force | Out-Null }

# 品牌色与背景取自 Tauri 工作台的暗色配色（别用纯黑：安装器里会糊成一团）。
function New-IconBitmap([int]$size) {
    $bmp = New-Object System.Drawing.Bitmap($size, $size)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::AntiAlias
    $g.TextRenderingHint = [System.Drawing.Text.TextRenderingHint]::AntiAlias
    $s = $size / 256.0

    # 背景：圆角深色方块
    $g.Clear([System.Drawing.Color]::Transparent)
    $bg = New-Object System.Drawing.SolidBrush([System.Drawing.Color]::FromArgb(255, 17, 24, 32))
    $r = 56 * $s
    $path = New-Object System.Drawing.Drawing2D.GraphicsPath
    $path.AddArc(0, 0, $r, $r, 180, 90)
    $path.AddArc($size - $r - 1, 0, $r, $r, 270, 90)
    $path.AddArc($size - $r - 1, $size - $r - 1, $r, $r, 0, 90)
    $path.AddArc(0, $size - $r - 1, $r, $r, 90, 90)
    $path.CloseFigure()
    $g.FillPath($bg, $path)

    # 「桥」：一条拱形主跨 + 两根桥墩 —— 比缩写更能在小尺寸下认出形状
    $pen = New-Object System.Drawing.Pen([System.Drawing.Color]::FromArgb(255, 88, 196, 221), (18 * $s))
    $pen.StartCap = [System.Drawing.Drawing2D.LineCap]::Round
    $pen.EndCap = [System.Drawing.Drawing2D.LineCap]::Round
    $g.DrawArc($pen, 40 * $s, 62 * $s, 176 * $s, 150 * $s, 190, 160)
    $g.DrawLine($pen, 62 * $s, 150 * $s, 62 * $s, 200 * $s)
    $g.DrawLine($pen, 194 * $s, 150 * $s, 194 * $s, 200 * $s)
    $g.DrawLine($pen, 40 * $s, 202 * $s, 216 * $s, 202 * $s)

    $g.Dispose()
    return $bmp
}

# ── 各尺寸 PNG 字节 ──────────────────────────────────────────────────────────
$sizes = @(256, 64, 48, 32, 16)
$pngs = @()
foreach ($size in $sizes) {
    $bmp = New-IconBitmap $size
    $ms = New-Object System.IO.MemoryStream
    $bmp.Save($ms, [System.Drawing.Imaging.ImageFormat]::Png)
    $pngs += , $ms.ToArray()
    if ($size -eq 256) { $bmp.Save((Join-Path $dir 'icon.png'), [System.Drawing.Imaging.ImageFormat]::Png) }
    $bmp.Dispose()
    $ms.Dispose()
}

# ── ICO 容器（手写，见文件头说明）────────────────────────────────────────────
$ico = New-Object System.IO.MemoryStream
$w = New-Object System.IO.BinaryWriter($ico)
$w.Write([UInt16]0)                 # reserved
$w.Write([UInt16]1)                 # type = icon
$w.Write([UInt16]$sizes.Count)
$offset = 6 + 16 * $sizes.Count
for ($i = 0; $i -lt $sizes.Count; $i++) {
    $size = $sizes[$i]
    # 256 在 ICO 里用 0 表示（规范如此）
    $w.Write([Byte]$(if ($size -ge 256) { 0 } else { $size }))
    $w.Write([Byte]$(if ($size -ge 256) { 0 } else { $size }))
    $w.Write([Byte]0)               # 调色板数
    $w.Write([Byte]0)               # reserved
    $w.Write([UInt16]1)             # planes
    $w.Write([UInt16]32)            # 位深
    $w.Write([UInt32]$pngs[$i].Length)
    $w.Write([UInt32]$offset)
    $offset += $pngs[$i].Length
}
foreach ($png in $pngs) { $w.Write($png) }
$w.Flush()
[System.IO.File]::WriteAllBytes((Join-Path $dir 'icon.ico'), $ico.ToArray())
$w.Dispose(); $ico.Dispose()

$icoPath = Join-Path $dir 'icon.ico'
Write-Host ("已生成：{0}（{1} 字节，尺寸 {2}）" -f $icoPath.Replace($root + '\', ''), (Get-Item $icoPath).Length, ($sizes -join '、'))
Write-Host ("已生成：{0}" -f (Join-Path $dir 'icon.png').Replace($root + '\', ''))
