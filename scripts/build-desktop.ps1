# 构建工作台的 Tauri 壳（本地用）。
#
# 为什么单独一个脚本、而且**不进** `verify.ps1`：
# Tauri 需要系统 WebView 与一大堆依赖，构建一次要几分钟，而它对"核心是否正确"
# 没有任何贡献 —— 它验证的是"界面能不能拿到真实数据"。
# 所以：本地按需跑，CI 不跑（理由见 apps/desktop/README.md）。
#
# 用法：
#   pwsh scripts/build-desktop.ps1            # 构建 + 跑壳里的用例
#   pwsh scripts/build-desktop.ps1 -Release   # 发布构建（不打包安装包）

[CmdletBinding()]
param(
    [switch]$Release
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$shell = Join-Path $root 'apps/desktop/src-tauri'

if (-not (Test-Path (Join-Path $shell 'Cargo.toml'))) {
    Write-Host "找不到 src-tauri：" -ForegroundColor Red
    Write-Host "  期望路径：$shell" -ForegroundColor Red
    exit 1
}

Push-Location $shell
try {
    Write-Host "· 构建桌面壳（首次会拉 Tauri 依赖，可能几分钟）" -ForegroundColor Cyan
    $args = @('test')
    if ($Release) { $args += '--release' }
    & cargo @args
    if ($LASTEXITCODE -ne 0) {
        throw "cargo test 失败（退出码 $LASTEXITCODE）"
    }

    Write-Host ""
    Write-Host "壳里的用例验证三件事：" -ForegroundColor Green
    Write-Host "  1. view_trace 命令把**真实轨迹**变成界面能用的 JSON（含告警与时间线）；"
    Write-Host "  2. 读不到文件时**报错**而不是返回空载荷；"
    Write-Host "  3. 能力矩阵与覆盖缺口各自单独可查。"
    Write-Host ""
    Write-Host "界面侧需要 Node 依赖（pnpm install）才能跑起来；本仓库的 registry" -ForegroundColor Yellow
    Write-Host "不可达时不装依赖，改用 node --test 验证纯逻辑（见 verify.ps1 的第 11 步）。" -ForegroundColor Yellow
} finally {
    Pop-Location
}
