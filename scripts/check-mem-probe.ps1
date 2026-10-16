<#
.SYNOPSIS
  验证内存探针的**地址范围过滤**确实生效（需要 Intel Pin 与已构建的 Pin tool）。

.DESCRIPTION
  为什么这条要单独脚本化，而不放进普通的 ctest 用例里：

  `-pb_mem_range` 是内存探针能不能用的前提（不过滤就会灌爆环形缓冲、
  丢掉一半事件）。而**缓冲区地址每次运行都不一样**（ASLR），所以判据必须
  分两步：先让靶子把地址打出来，再用这个地址去跑探针。

  ctest 的一条命令里做不到"先读输出再决定参数"，所以这里用一个脚本，
  它自己跑两步并给出三条判定：

    1. 探针捕获的写入数**等于**靶子实际执行的写入数（一个不多一个不少）；
    2. 环形缓冲**溢出丢弃为 0**（静默丢事件是禁止的）；
    3. 地址范围过滤确实起了作用（过滤计数 > 0）。

  这三条一起才有意义：只有第 1 条会被"根本没过过滤"骗到（不过滤也会捕到
  那些写入），只有第 3 条会被"什么都没捕到"骗到。

.PARAMETER PinRoot
  Pin kit 根目录（默认 third_party/pin）。

.PARAMETER BuildDir
  带 Pin 的构建目录（默认 native/build-pin）。

.PARAMETER Iterations
  靶子的写入次数（默认 2000）。

.EXAMPLE
  pwsh scripts/check-mem-probe.ps1
#>
[CmdletBinding()]
param(
    [string]$PinRoot = 'third_party/pin',
    [string]$BuildDir = 'native/build-pin',
    [int]$Iterations = 2000
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot

function Fail([string]$message) {
    Write-Host "  ✗ $message" -ForegroundColor Red
    exit 1
}

$pin = Join-Path $root (Join-Path $PinRoot 'pin.exe')
$tool = Join-Path $root (Join-Path $BuildDir 'dist/pinbridge.dll')
$target = Join-Path $root (Join-Path $BuildDir 'pb_sample_target.exe')

# 这几条不是"文件缺失"而是"这次没准备好"：装不上 Pin 的环境应当跳过，
# 而不是把整条门禁判失败。返回 77 是 ctest 约定的"跳过"。
if (-not (Test-Path $pin) -or -not (Test-Path $tool) -or -not (Test-Path $target)) {
    Write-Host "跳过：缺少 Pin 或构建产物（先跑 scripts/build-native.ps1 -WithPin）" -ForegroundColor Yellow
    exit 77
}

# ── 第一步：拿缓冲区地址（**不用 Pin**，所以这是靶子自己真实运行的结果）──
$probeLine = (& $target --print-buffer 2>&1 | Select-String 'PB_TOUCH_BUFFER' | Select-Object -First 1).Line
if (-not $probeLine) { Fail '靶子没有输出 PB_TOUCH_BUFFER' }
$hex = ($probeLine -replace '.*PB_TOUCH_BUFFER=0x([0-9a-fA-F]+).*', '$1')
$low = [Convert]::ToUInt64($hex, 16)
$high = $low + 0xFF
$range = '0x{0:x}-0x{1:x}' -f $low, $high
Write-Host "缓冲区 0x$hex，过滤范围 $range" -ForegroundColor Cyan

# ── 第二步：带范围过滤跑探针 ──
$trace = Join-Path $root (Join-Path $BuildDir 'mem-probe-check.pbt')
Remove-Item $trace -ErrorAction SilentlyContinue
Push-Location (Join-Path $root (Join-Path $BuildDir 'dist'))
try {
    $out = (& $pin -t $tool -pb_mem w -pb_mem_range $range -pb_mem_max 100000 `
            -pb_require_mem $Iterations -pb_events 2 -pb_trace $trace `
            -- $target --touch $Iterations 2>&1) -join "`n"
} finally {
    Pop-Location
}

Write-Host $out

# ── 三条判定 ──
if ($out -match 'Unhandled internal exception') { Fail 'Pin 内部故障（工具崩了）' }

$writes = if ($out -match '捕获 write=(\d+)') { [int]$matches[1] } else { Fail '收尾统计里没有"捕获 write="' }
if ($writes -ne $Iterations) {
    Fail "捕获的写入数 $writes 与靶子执行的 $Iterations 次不一致"
}

if ($out -match '溢出丢弃 (\d+) 条') {
    if ([int]$matches[1] -ne 0) { Fail "环形缓冲溢出了 $($matches[1]) 条（地址过滤没起作用或容量不够）" }
} else { Fail '收尾统计里没有"溢出丢弃"' }

if ($out -match '地址范围过滤掉 (\d+) 条') {
    if ([int]$matches[1] -le 0) { Fail '地址范围过滤一条都没过滤掉——说明过滤条件根本没有生效' }
} else { Fail '收尾统计里没有"地址范围过滤掉"' }

if ($out -notmatch '内存事件捕获数满足要求') { Fail '工具没有报"捕获数满足要求"' }

Write-Host "  ✓ 内存探针：范围过滤生效、写入数精确、零丢弃" -ForegroundColor Green
exit 0
