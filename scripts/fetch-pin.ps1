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
# 按**实测**的 kit 布局检查（早期版本查的是 source/tools/Pin/pin.H——
# 那是源码树内部的路径，kit 里根本不存在，于是脚本永远报"缺 Pin"）。
$pinHeader = Join-Path $Root 'source\include\pin\pin.H'
$pinLib = Join-Path $Root 'intel64\lib\pin.lib'
$makefileConfig = Join-Path $Root 'source\tools\Config\makefile.win.config'

Write-Host "检查 Pin: $Root" -ForegroundColor Cyan

$missing = @()
if (-not (Test-Path $pinExe)) { $missing += 'pin.exe' }
if (-not (Test-Path $pinHeader)) { $missing += 'source/include/pin/pin.H' }
if (-not (Test-Path $pinLib)) { $missing += 'intel64/lib/pin.lib' }

if ($missing.Count -eq 0) {
    $version = (& $pinExe -version 2>&1 | Select-Object -First 1)
    Write-Host "  ✓ 找到 Pin：$version" -ForegroundColor Green
    Write-Host "  ✓ 头文件：$pinHeader"
    Write-Host "  ✓ 导入库：$pinLib"
    if (Test-Path $makefileConfig) {
        Write-Host "  ✓ 构建参数权威：$makefileConfig（native/CMakeLists.txt 的 Pin 目标逐条对应它）"
    }
    Write-Host ""
    Write-Host "构建工具 DLL："
    Write-Host "  pwsh scripts/build-native.ps1 -WithPin"
    Write-Host "运行："
    Write-Host "  third_party\pin\pin.exe -t native\build\dist\pinbridge.dll -- TARGET.exe"
    exit 0
}

Write-Host "  ✗ 缺少：$($missing -join ', ')" -ForegroundColor Yellow
if ($Verify) { exit 1 }

Write-Host ""
Write-Host "获取方式（必须由人接受 Intel 许可）:" -ForegroundColor Cyan
Write-Host "  1. 从 Intel 官方页面下载 Pin 3.31 的 Windows 包"
Write-Host "     已记录过的包：pin-external-3.31-98869-gfa6f126a8-msvc-windows.zip"
Write-Host "     sha256: d6f492dcbb18ee5e8e0e23be4b07a8aac843e23c7d4bd2bc1283c91ac60f6632"
Write-Host "  2. 解压后把**内层目录的内容**放到：$Root"
Write-Host "     期望结构： pin.exe / intel64/lib/pin.lib / source/include/pin/pin.H"
Write-Host "  3. 用本脚本 -Verify 复查，并把版本与 sha256 填进 third_party/pin/README.md 的表"
Write-Host ""
Write-Host "注意：**没有 Pin 也能继续**。M0.5 的桥接层让 Hub ↔ agent 在无 Pin 时也能真实跑通；" -ForegroundColor DarkGray
Write-Host "      需要插装的动作会显式返回 not_supported，而不是假装成功。" -ForegroundColor DarkGray
exit 0
