<#
.SYNOPSIS
  PinBridge 本地全量验证：与 CI 逐条对应的门禁。

.DESCRIPTION
  依次执行：格式 → clippy → 测试 → 端到端 demo → Python 侧测试。
  任何一步失败立刻停止（fail-fast），并打印"怎么修"。

  这不是"顺便跑一下"，而是提交 PR 前的**必做步骤**。
  失败时请修代码，不要改脚本。

.PARAMETER SkipPython
  跳过 Python 侧（例如本机没有 Python 环境时）。

.PARAMETER SkipClippy
  跳过 clippy（只建议在排查环境问题时用）。

.EXAMPLE
  pwsh scripts/verify.ps1
  pwsh scripts/verify.ps1 -SkipPython
#>
[CmdletBinding()]
param(
    [switch]$SkipPython,
    [switch]$SkipClippy
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Push-Location $root
try {
    $results = [ordered]@{}

    function Step {
        param([string]$Name, [scriptblock]$Body, [string]$Fix)
        Write-Host ""
        Write-Host "== $Name ==" -ForegroundColor Cyan
        $sw = [System.Diagnostics.Stopwatch]::StartNew()
        & $Body
        $code = $LASTEXITCODE
        $sw.Stop()
        if ($code -ne 0) {
            Write-Host "FAIL ($([int]$sw.Elapsed.TotalSeconds)s) —— $Fix" -ForegroundColor Red
            $script:results[$Name] = "FAIL"
            throw "步骤失败: $Name"
        }
        Write-Host "ok ($([int]$sw.Elapsed.TotalSeconds)s)" -ForegroundColor Green
        $script:results[$Name] = "ok"
    }

    Step 'cargo fmt --check' { cargo fmt --all -- --check } '运行 `cargo fmt --all`'

    if (-not $SkipClippy) {
        Step 'cargo clippy -D warnings' {
            cargo clippy --workspace --all-targets -- -D warnings
        } '按提示修代码；如需豁免，必须在 Cargo.toml 的 lint 表里写明理由'

        # Windows 上有一条 clippy 类别**本机抓不到**：只在非 Windows 平台暴露的问题
        # （典型例子：某个 `use` 只被 `#[cfg(windows)]` 的代码用到 → Linux 上 unused_imports）。
        # 这类问题会等到 CI 的 ubuntu 任务才炸。装了 linux 目标时顺手在这里拦掉。
        $linuxTarget = (rustup target list --installed 2>$null) -contains 'x86_64-unknown-linux-gnu'
        if ($linuxTarget) {
            Step 'cargo clippy --target x86_64-unknown-linux-gnu' {
                cargo clippy --workspace --all-targets --target x86_64-unknown-linux-gnu -- -D warnings
            } '检查只在非 Windows 平台暴露的 lint（cfg 分支里的未使用导入等）'
        } else {
            Write-Host "· 未安装 x86_64-unknown-linux-gnu 目标，跳过跨平台 lint" -ForegroundColor Yellow
            Write-Host "  建议执行：rustup target add x86_64-unknown-linux-gnu（CI 会跑，但本地拦更早）" -ForegroundColor Yellow
        }
    }

    Step 'cargo test --workspace' { cargo test --workspace } '先看第一个失败的用例，不要一次改多处'

    Step '端到端自检（pinbridge demo）' {
        cargo run --quiet -p pinbridge-cli -- demo --trace-dir (Join-Path $env:TEMP 'pinbridge-verify')
    } '检查 demo 的哪一步断了；这一步失败通常意味着跨模块契约被改坏'

    if (-not $SkipPython) {
        if (Get-Command python -ErrorAction SilentlyContinue) {
            Step 'Python 侧（pytest）' {
                Push-Location (Join-Path $root 'python')
                try {
                    $env:PYTHONPATH = 'src'
                    if (Get-Command uv -ErrorAction SilentlyContinue) {
                        uv run --no-project --with pytest --python 3.12 python -m pytest -q
                    } else {
                        python -m pytest -q
                    }
                } finally { Pop-Location }
            } '跨语言一致性失败时，先跑 test_golden_vectors 定位是哪一侧偏离了规范'
        } else {
            Write-Host "· 未找到 python，跳过 Python 侧（CI 会跑）" -ForegroundColor Yellow
        }
    }

    Write-Host ""
    Write-Host "全部通过：" -ForegroundColor Green
    $results.GetEnumerator() | ForEach-Object { "  {0,-32} {1}" -f $_.Key, $_.Value }
    Write-Host ""
    Write-Host "可以提 PR 了。别忘了更新 CHANGELOG.md。" -ForegroundColor Green
} catch {
    Write-Host ""
    Write-Host "验证未通过：$_" -ForegroundColor Red
    exit 1
} finally {
    Pop-Location
}
