<#
.SYNOPSIS
  检查（或提示获取）Intel Pin SDK。

.DESCRIPTION
  Pin 不入库（许可 + 体积），但"缺 Pin"必须是**一眼能看懂**的状态。
  本脚本只做检查与指引，不自动下载：Pin 的下载需要接受 Intel 的许可条款，
  那个动作必须由人来做。

.PARAMETER Verify
  只检查，不打印获取指引（CI 用）。

.PARAMETER Root
  Pin 安装根目录，默认 third_party/pin。

.EXAMPLE
  pwsh scripts/fetch-pin.ps1 -Verify
#>
[CmdletBinding()]
param(
    [switch]$Verify,
    [string]$Root = (Join-Path (Split-Path -Parent $PSScriptRoot) 'third_party\pin')
)

$ErrorActionPreference = 'Stop'

$pinExe = Join-Path $Root 'pin.exe'
$pinHeader = Join-Path $Root 'source\tools\Pin\pin.H'

Write-Host "检查 Pin: $Root" -ForegroundColor Cyan

$missing = @()
if (-not (Test-Path $pinExe)) { $missing += 'pin.exe' }
if (-not (Test-Path $pinHeader)) { $missing += 'source/tools/Pin/pin.H' }

if ($missing.Count -eq 0) {
    $version = (& $pinExe -version 2>&1 | Select-Object -First 1)
    Write-Host "  ✓ 找到 Pin：$version" -ForegroundColor Green
    Write-Host "  ✓ 头文件：$pinHeader"
    Write-Host ""
    Write-Host "构建工具 DLL："
    Write-Host "  cmake -S native -B native/build -DPINBRIDGE_WITH_PIN=ON -DPIN_ROOT=`"$Root\source\tools\Pin`""
    Write-Host "  cmake --build native/build --config Release"
    exit 0
}

Write-Host "  ✗ 缺少：$($missing -join ', ')" -ForegroundColor Yellow
if ($Verify) { exit 0 }

Write-Host ""
Write-Host "获取方式（必须由人接受 Intel 许可）:" -ForegroundColor Cyan
Write-Host "  1. 从 Intel 官方页面下载 Pin 3.31 的 Windows 包"
Write-Host "  2. 解压到：$Root"
Write-Host "     期望结构： pin.exe / source/tools/Pin/pin.H / source/tools/ManualExamples/"
Write-Host "  3. 记录版本与 sha256，更新 third_party/pin/README.md 的表格"
Write-Host "  4. 重跑本脚本确认"
Write-Host ""
Write-Host "注意：**没有 Pin 也能继续**。M0 的骨架、测试、CLI、Python 侧都离线可跑；" -ForegroundColor DarkGray
Write-Host "      需要插装的动作会显式返回 not_supported，而不是假装成功。" -ForegroundColor DarkGray
exit 0
