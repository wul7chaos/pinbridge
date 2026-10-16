<#
.SYNOPSIS
  构建 native/ C++ 层（桥接 DLL + ABI 自检）并运行 CTest。

.DESCRIPTION
  这一层**不需要 Intel Pin**：它构建出 `pinbridge_bridge.dll`——导出 Hub 期望的
  tool ABI 并转发给真实 agent DLL 的桥接层。有了它，"Hub ↔ agent"这条链路
  在没有 Pin 的机器上也能被真实驱动与测试（见 docs/roadmap.md 的 M1）。

  本脚本把"手工摸索过一遍的工具链发现过程"固定下来：Visual Studio 的
  cl.exe/vcvars64、Ninja、CMake（缺失时用 uv 从 PyPI 拉一个）。
  它同时是 CI 与本地使用的**同一条路径**——两边用不同的构建方式，
  迟早会出现"本地过、CI 挂"。

.PARAMETER Configuration
  Release（默认）或 Debug。

.PARAMETER BuildDir
  构建目录，默认 native/build。

.PARAMETER SkipTests
  只构建，不跑 ctest。

.PARAMETER RequireAgent
  agent DLL 缺失时直接失败（CI 用）。默认只警告。

.PARAMETER WithPin
  同时构建依赖 Intel Pin 的 Pin tool（pinbridge.dll）。需要 third_party/pin/
  下已放好 Pin kit（用 scripts/fetch-pin.ps1 -Verify 检查真实布局）。

  默认**不开**：CI 拿不到 Pin（许可要求人接受），而"缺 Pin"必须是一眼能看懂的
  失败，不能变成整个仓库都构建不出来。

.EXAMPLE
  pwsh scripts/build-native.ps1
  pwsh scripts/build-native.ps1 -RequireAgent
#>
[CmdletBinding()]
param(
    [string]$Configuration = 'Release',
    [string]$BuildDir = 'native/build',
    [switch]$SkipTests,
    [switch]$RequireAgent,
    [switch]$WithPin
)

# 带 Pin 的构建用**独立的构建目录**，而且必须在 `$configArgs` 组装之前决定。
#
# 起因是一次真实的时间浪费：`verify.ps1`（不带 Pin）会重新配置 native/build 并把
# PINBRIDGE_WITH_PIN 置 OFF，于是随后"带 Pin 的构建"其实什么都没编译，跑起来
# 还是旧的 DLL——表现为旋钮"unknown option"。两个用途共用一个构建目录，
# 谁最后跑谁说了算；这种事必须靠目录隔离，不能靠记忆。
if ($WithPin -and -not $PSBoundParameters.ContainsKey('BuildDir')) {
    $BuildDir = 'native/build-pin'
}

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Push-Location $root

function Info { param([string]$m) Write-Host "  $m" }
function Ok { param([string]$m) Write-Host "  ✓ $m" -ForegroundColor Green }
function Die { param([string]$m) Write-Host "  ✗ $m" -ForegroundColor Red; throw $m }

try {
    # ------------------------------------------------------- 1. 找 C++ 工具链
    Write-Host "native 层构建" -ForegroundColor Cyan

    $vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
    if (-not (Test-Path $vswhere)) { Die '找不到 vswhere.exe：需要安装 Visual Studio（含 C++ 工具集）' }
    $vsRoot = (& $vswhere -products * -latest -property installationPath) | Select-Object -First 1
    if (-not $vsRoot) { Die 'vswhere 没有报告任何 Visual Studio 安装（需要“使用 C++ 的桌面开发”组件）' }
    Info "Visual Studio: $vsRoot"

    $vcvars = Join-Path $vsRoot 'VC\Auxiliary\Build\vcvars64.bat'
    if (-not (Test-Path $vcvars)) { Die "找不到 vcvars64.bat：$vcvars" }

    # Ninja 随 VS 一起安装；把它放到 PATH 上供 CMake 使用
    $ninjaDir = Join-Path $vsRoot 'Common7\IDE\CommonExtensions\Microsoft\CMake\Ninja'
    if (Test-Path (Join-Path $ninjaDir 'ninja.exe')) { $env:Path = "$ninjaDir;$env:Path"; Info "ninja: $ninjaDir" }
    else { Die "找不到 ninja.exe（$ninjaDir）" }

    # CMake：优先系统已有的；没有就用 uv 从 PyPI 拉一个（CI 机器上一般自带）
    $cmake = (Get-Command cmake -ErrorAction SilentlyContinue)?.Source
    $ctest = (Get-Command ctest -ErrorAction SilentlyContinue)?.Source
    if (-not $cmake) {
        $uvTool = "$env:USERPROFILE\.local\bin\cmake.exe"
        if (Test-Path $uvTool) { $cmake = $uvTool; $ctest = "$env:USERPROFILE\.local\bin\ctest.exe" }
    }
    if (-not $cmake -and (Get-Command uv -ErrorAction SilentlyContinue)) {
        Info '系统没有 cmake，用 uv 安装一个（uv tool install cmake）'
        uv tool install cmake 2>&1 | Out-Null
        $cmake = "$env:USERPROFILE\.local\bin\cmake.exe"
        $ctest = "$env:USERPROFILE\.local\bin\ctest.exe"
    }
    if (-not $cmake -or -not (Test-Path $cmake)) { Die '找不到 cmake：请安装 CMake，或安装 uv（本脚本会用 `uv tool install cmake`）' }
    Info "cmake: $cmake"

    # --------------------------------------------------------- 2. 注入 MSVC 环境
    # vcvars64.bat 只能在 cmd 里生效，于是把它的环境变量灌进当前进程。
    Info '载入 vcvars64 环境（INCLUDE/LIB/PATH）'
    $envLines = cmd /c "`"$vcvars`" >nul 2>&1 && set"
    foreach ($line in $envLines) {
        if ($line -match '^([^=]+)=(.*)$') {
            [System.Environment]::SetEnvironmentVariable($matches[1], $matches[2])
        }
    }
    if (-not (Get-Command cl.exe -ErrorAction SilentlyContinue)) { Die 'vcvars 之后仍然找不到 cl.exe' }
    Ok "cl.exe 就绪（MSVC $(cl 2>&1 | Select-Object -First 1))"

    # -------------------------------------------------------------- 3. agent DLL
    $agentDll = Get-ChildItem -Path (Join-Path $root 'target\debug\pinbridge_agent.dll'), (Join-Path $root 'target\release\pinbridge_agent.dll') -ErrorAction SilentlyContinue |
        Select-Object -First 1
    if (-not $agentDll) {
        Info '未找到 pinbridge_agent.dll，先运行 cargo build -p pinbridge-agent'
        cargo build -p pinbridge-agent 2>&1 | Select-Object -Last 2
        $agentDll = Get-ChildItem -Path (Join-Path $root 'target\debug\pinbridge_agent.dll') -ErrorAction SilentlyContinue | Select-Object -First 1
    }
    if (-not $agentDll) {
        if ($RequireAgent) { Die 'agent DLL 缺失，而本次要求必须有（-RequireAgent）' }
        Write-Host '  ! agent DLL 缺失：桥接层的正向用例会被标记为跳过' -ForegroundColor Yellow
    } else {
        Ok "agent DLL: $($agentDll.FullName)"
    }

    # ------------------------------------------------------------------ 4. 构建
    $configArgs = @('-S', 'native', '-B', $BuildDir, '-G', 'Ninja', "-DCMAKE_BUILD_TYPE=$Configuration")
    if ($agentDll) { $configArgs += "-DPINBRIDGE_AGENT_DLL=$($agentDll.FullName)" }

# Pin kit：只检查"存在且能报版本"，路径按第三方的**实测**布局
# （见 third_party/pin/README.md——早先按记忆写成 source/tools/Pin/pin.H，
# 结果脚本永远报"缺 Pin"，而实际是装好的）。
if ($WithPin) {
    $pinRoot = Join-Path $root 'third_party/pin'
    $pinExe = Join-Path $pinRoot 'pin.exe'
    $pinLib = Join-Path $pinRoot 'intel64/lib/pin.lib'
    if (-not (Test-Path $pinExe) -or -not (Test-Path $pinLib)) {
        Die "启用了 -WithPin 但没有 Pin kit：$pinRoot（获取方式见 third_party/pin/README.md）"
    }
    $pinVersion = (& $pinExe -version 2>&1 | Select-Object -First 1)
    Write-Host "Pin: $pinVersion" -ForegroundColor Cyan
    $configArgs += "-DPINBRIDGE_WITH_PIN=ON"
    $configArgs += "-DPIN_ROOT=$pinRoot"
} else {
    $configArgs += "-DPINBRIDGE_WITH_PIN=OFF"
}

# 缓存与本次请求不一致时**说出来**：CMake 会按 -D 重新配置，但"我明明加了
# -WithPin 怎么还是没编"这种困惑不该靠人自己发现。这里直接比对缓存值。
$cacheFile = Join-Path $BuildDir 'CMakeCache.txt'
if (Test-Path $cacheFile) {
    $cached = (Select-String -Path $cacheFile -Pattern '^PINBRIDGE_WITH_PIN:BOOL=(\w+)' |
        Select-Object -First 1).Matches[0].Groups[1].Value
    $want = if ($WithPin) { 'ON' } else { 'OFF' }
    if ($cached -and $cached -ne $want) {
        Write-Host "注意：$BuildDir 之前的缓存是 PINBRIDGE_WITH_PIN=$cached，本次按 $want 重新配置" -ForegroundColor Yellow
        Write-Host "      （带 Pin 与不带 Pin 用不同构建目录，避免互相覆盖）" -ForegroundColor DarkGray
    }
}
    Info "配置：cmake $($configArgs -join ' ')"
    & $cmake @configArgs 2>&1 | Select-String -Pattern 'agent DLL|WARNING|Error|error' | Select-Object -First 5

    Info '构建中…'
    & $cmake --build $BuildDir 2>&1 | Select-String -Pattern 'error|FAILED|Linking' | Select-Object -Last 6
    if ($LASTEXITCODE -ne 0) { Die "cmake --build 失败（exit=$LASTEXITCODE）" }
    Ok "产物目录：$BuildDir"

    # -------------------------------------------------------------------- 5. 测试
    if (-not $SkipTests) {
        if (-not $ctest -or -not (Test-Path $ctest)) { $ctest = (Get-Command ctest -ErrorAction SilentlyContinue)?.Source }
        if (-not $ctest) { Die '找不到 ctest' }
        Info 'ctest'
        & $ctest --test-dir $BuildDir --output-on-failure 2>&1 | Select-Object -Last 12
        if ($LASTEXITCODE -ne 0) { Die "ctest 失败（exit=$LASTEXITCODE）" }
        Ok 'native 层测试全过'
    }

    Write-Host ''
    Write-Host '提示：跑 Hub 侧加载器的端到端测试需要把桥接 DLL 的路径告诉它：' -ForegroundColor DarkGray
    Write-Host "  `$env:PINBRIDGE_BRIDGE_DLL = '$root\$BuildDir\pinbridge_bridge.dll'" -ForegroundColor DarkGray
    Write-Host '  $env:PINBRIDGE_REQUIRE_NATIVE = 1; cargo test -p pinbridge-sys' -ForegroundColor DarkGray
} finally {
    Pop-Location
}
