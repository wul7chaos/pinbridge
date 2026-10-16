<#
.SYNOPSIS
  一次性把开发环境准备好。

.DESCRIPTION
  检查并（在需要时）安装/配置：
    1. Rust 工具链（按 rust-toolchain.toml 钉住的版本）
    2. Python（>= 3.10）与 python/ 下的开发依赖
    3. git 本地配置：提交模板、hooks 路径、默认分支、行尾策略
    4. 可选：CMake（只在要做 native/ 层时需要）

  设计原则：**幂等**。反复跑不会破坏已有环境，也不会重复安装。
  每一步都会打印"做了什么/为什么需要"——新人上手时这个脚本本身就是文档。

.PARAMETER InstallHooks
  安装 git hooks（commit-msg / pre-commit）。

.PARAMETER SkipPython
  跳过 Python 环境准备。

.EXAMPLE
  pwsh scripts/bootstrap.ps1 -InstallHooks
#>
[CmdletBinding()]
param(
    [switch]$InstallHooks,
    [switch]$SkipPython
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Push-Location $root

function Info { param([string]$m) Write-Host "  $m" }
function Ok { param([string]$m) Write-Host "  ✓ $m" -ForegroundColor Green }
function Warn { param([string]$m) Write-Host "  ! $m" -ForegroundColor Yellow }

try {
    Write-Host "PinBridge 环境引导" -ForegroundColor Cyan
    Write-Host "仓库: $root"

    # ------------------------------------------------------------ 1. Rust
    Write-Host "`n[1/4] Rust 工具链" -ForegroundColor Cyan
    if (Get-Command rustup -ErrorAction SilentlyContinue) {
        $pinned = (Select-String -Path (Join-Path $root 'rust-toolchain.toml') -Pattern 'channel\s*=\s*"([^"]+)"').Matches[0].Groups[1].Value
        Info "rust-toolchain.toml 钉住: $pinned"
        rustup toolchain install $pinned --component rustfmt clippy --no-self-update 2>&1 | Out-Null
        Ok "工具链就绪（进入仓库目录后 rustup 会自动使用它）"
    } else {
        Warn "未找到 rustup：请从 https://rustup.rs 安装后重跑本脚本"
    }

    # ----------------------------------------------------------- 2. Python
    Write-Host "`n[2/4] Python 环境" -ForegroundColor Cyan
    if ($SkipPython) {
        Warn "按要求跳过"
    } elseif (Get-Command python -ErrorAction SilentlyContinue) {
        $version = (& python -c "import sys;print('%d.%d' % sys.version_info[:2])")
        Info "python $version"
        $required = [version]'3.10'
        if ([version]$version -lt $required) {
            Warn "需要 >= 3.10，当前 $version"
        } else {
            Push-Location (Join-Path $root 'python')
            try {
                if (Get-Command uv -ErrorAction SilentlyContinue) {
                    Info "用 uv 准备隔离环境（不改动系统 Python）"
                    uv venv .venv --python $version 2>&1 | Out-Null
                    uv pip install --python (Join-Path (Get-Location) '.venv\Scripts\python.exe') -e '.[dev]' 2>&1 | Out-Null
                    Ok "python/.venv 就绪（用 .venv\Scripts\python.exe 或 uv run 调用）"
                } else {
                    Info "未找到 uv，用 pip 安装开发依赖"
                    python -m pip install -e '.[dev]' 2>&1 | Out-Null
                    Ok "已安装到当前解释器"
                }
            } finally { Pop-Location }
        }
    } else {
        Warn "未找到 python：Python 侧是可选的（Rust 侧完全独立），CI 会跑"
    }

    # -------------------------------------------------------------- 3. Git
    Write-Host "`n[3/4] git 本地配置" -ForegroundColor Cyan
    $template = Join-Path $root '.gitmessage'
    if (Test-Path $template) {
        git config commit.template $template
        Ok "提交模板 -> .gitmessage"
    }
    git config core.autocrlf false
    Ok "core.autocrlf=false（统一由 .gitattributes 决定行尾）"
    git config pull.rebase true
    Ok "pull.rebase=true（避免无意义的 merge 提交）"
    git config fetch.prune true
    Ok "fetch.prune=true"
    git config rerere.enabled true
    Ok "rerere.enabled=true（反复解同一处冲突时省事）"

    # ------------------------------------------------------------- 4. 钩子
    Write-Host "`n[4/4] git hooks" -ForegroundColor Cyan
    if ($InstallHooks) {
        git config core.hooksPath scripts/githooks
        Ok "core.hooksPath -> scripts/githooks（commit-msg / pre-commit）"
        Info "跳过钩子用 git commit --no-verify；但 CI 一样会拦"
    } else {
        Warn "未安装（加 -InstallHooks 参数安装）"
    }

    Write-Host "`n下一步：" -ForegroundColor Cyan
    Info "cargo test --workspace           # 139 个单测，离线可跑"
    Info "cargo run -p pinbridge-cli -- demo   # 端到端自检"
    Info "pwsh scripts/verify.ps1          # 提交前全量门禁"
    Info "docs/dev/getting-started.md      # 更完整的上手路径"
} finally {
    Pop-Location
}
