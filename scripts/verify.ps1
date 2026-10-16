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

    # native 层（C++ 桥接 + ABI 自检）：装了 Visual Studio 就跑。
    # **刻意放在 cargo test 之前**：Rust 侧的加载器端到端测试需要这个 DLL，
    # 顺序反了它就会跳过——而"跳过的测试"正是本项目最不想要的假绿。
    # 这一步验证的是"Hub 加载真实 DLL 并驱动 agent"这条链路，
    # 单元测试**测不到**它：它断了的表现是"目标进程崩了"而不是"测试失败"。
    $vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
    if (Test-Path $vswhere) {
        Step 'native 层（CMake + CTest）' {
            pwsh -NoProfile -File (Join-Path $root 'scripts/build-native.ps1') -RequireAgent
        } '先看是配置、编译还是 ctest 失败；桥接层的失败信息会指出具体缺哪个导出'
    } else {
        Write-Host '· 未找到 Visual Studio，跳过 native 层（CI 的 native job 会跑）' -ForegroundColor Yellow
    }

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
            # 这三步刻意分开：pytest 管行为，ruff/mypy 管风格与类型。
            # 一次跑完的好处是"本地通过 = CI 通过"；不这样做的话，
            # ruff/mypy 的问题会等到 CI 才暴露（本仓库就发生过一次）。
            $hasUv = [bool](Get-Command uv -ErrorAction SilentlyContinue)

            function Invoke-PyTool {
                param([string]$Tool, [string[]]$ToolArgs)
                Push-Location (Join-Path $root 'python')
                try {
                    $env:PYTHONPATH = 'src'
                    if ($hasUv) {
                        uv run --no-project --with $Tool --python 3.12 $Tool @ToolArgs
                    } else {
                        & $Tool @ToolArgs
                    }
                } finally { Pop-Location }
            }

            Step 'Python: pytest' {
                Invoke-PyTool 'pytest' @('-q')
            } '跨语言一致性失败时，先看 test_rust_parity 是不是找不到 Rust 二进制'

            Step 'Python: ruff check' {
                Invoke-PyTool 'ruff' @('check', '.', '--output-format', 'concise')
            } '运行 `ruff check . --fix` 看哪些能自动修；剩下的按 docs/dev/coding-standards-python.md'

            Step 'Python: ruff format --check' {
                Invoke-PyTool 'ruff' @('format', '--check', '.')
            } '运行 `ruff format .` 统一格式（与 Rust 侧同为 100 列）'

            Step 'Python: mypy（严格模式）' {
                Invoke-PyTool 'mypy' @()
            } '类型标注问题按提示补；不要用 `# type: ignore` 关掉了事'
        } else {
            Write-Host "· 未找到 python，跳过 Python 侧（CI 会跑）" -ForegroundColor Yellow
        }
    }

    # ---------------------------------------------------------------- 桌面端
    # 只跑**不需要依赖**的那部分：NDJSON 客户端的逻辑是纯 ESM，用 Node 自带的
    # 测试运行器即可验证（`node --test`）。装配依赖（React/Tauri）需要 registry，
    # 那个由 CI 装 —— 这里跑不了的就不假装跑过。
    if (Get-Command node -ErrorAction SilentlyContinue) {
        Step '桌面端: node --test（NDJSON 客户端）' {
            Push-Location (Join-Path $root 'apps/desktop')
            try {
                # 不传路径：用 Node 的自动发现（不同版本对"目录参数"的处理不一样，
                # CI 上的 Node 22 会把目录当成模块名而报错）。
                node --test
                if ($LASTEXITCODE -ne 0) { throw "node --test 失败" }
            } finally {
                Pop-Location
            }
        } '分帧/关联/错误传播的用例失败时不要改断言，先看是不是真的写错了'
    } else {
        Write-Host "· 未找到 node，跳过桌面端用例（CI 会跑）" -ForegroundColor Yellow
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
