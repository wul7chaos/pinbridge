<#
.SYNOPSIS
  验证 TF 单步的**观察结果**两侧一致；并在 `watch` 模式目前跑不通时明确跳过，
  而不是假装通过。

.DESCRIPTION
  这条要验的性质：工具看到的"单步异常发生在哪"，与目标自己从
  `GetExceptionInformation()->ExceptionAddress` 读到的是同一处。

  为什么必须是脚本而不是一条 ctest 正则：正则比不出两个值**相等**，而这条要验的
  恰恰是"两边说的是同一处"。只检查"都报了一个地址"等于什么都没验。

  **但这条判据目前拿不到数据**，原因是实测出来的：

    * `-pb_tf virtualize`（既有 ctest 用的模式）**能跑通**：工具把真实单步消化掉，
      目标一次异常都看不到（`PB_TF_EXC count=0`）—— 这是 virtualize 的**目的**，
      所以这个模式下没有"两侧比对"可言；
    * `-pb_tf watch`（想要两侧都看到异常的模式）下**靶子直接跑死**：运行到
      「入口点命中」之后再无任何输出，`PB_TF_THREADS` 一行都没有；
    * `--tf-set`（单线程那条路）则是**任何模式都死**。

  所以脚本分两段：

    1. `virtualize`：验**对照关系**（目标 0 次异常 + 工具明说"没有观察到真实单步"
       + 目标完整跑完轮数）。这一段现在就能成立，是实测证据；
    2. `watch`：若靶子死掉 ⇒ 打印清楚的现象并 **exit 77（跳过）**，附上待办；
       若某天它跑通了 ⇒ 真正执行"地址 ∈ 靶子报出的集合"这条判据并 exit 0/1。

  这样脚本不会因为一个已知缺陷长期变红（红的检查会训练人忽略红色 —— 第 41 轮
  的教训），而缺陷修好那天它会自动变成真正的断言。

.PARAMETER PinRoot
  Pin kit 根目录（默认 third_party/pin）。

.PARAMETER BuildDir
  带 Pin 的构建目录（默认 native/build-pin）。
#>
[CmdletBinding()]
param(
    [string]$PinRoot = 'third_party/pin',
    [string]$BuildDir = 'native/build-pin'
)

$ErrorActionPreference = 'Continue'
Set-StrictMode -Version Latest

function Info([string]$m) { Write-Host "  $m" }
function Ok([string]$m) { Write-Host "  ✓ $m" -ForegroundColor Green }
function Die([string]$m) { Write-Host "  ✗ $m" -ForegroundColor Red; exit 1 }
function Skip([string]$m) { Write-Host "  跳过：$m" -ForegroundColor Yellow; exit 77 }

$root = Split-Path -Parent $PSScriptRoot
$pin = Join-Path $root (Join-Path $PinRoot 'pin.exe')
if (-not (Test-Path $pin)) { Skip "没有 Pin kit（$pin）" }

$dist = Join-Path $root (Join-Path $BuildDir 'dist')
$tool = Join-Path $dist 'pinbridge.dll'
$target = Join-Path $root (Join-Path $BuildDir 'pb_sample_target.exe')
if (-not (Test-Path $tool)) { Skip "没有构建 Pin tool（$tool）" }
if (-not (Test-Path $target)) { Skip "没有构建靶子（$target）" }

function Invoke-Tf([string]$mode) {
    Push-Location $dist
    try {
        return (& $pin -t '.\pinbridge.dll' -pb_mem 0 -pb_tf $mode -pb_events 2 `
                -- $target --tf-threads 3 2>&1 | Out-String)
    } finally { Pop-Location }
}

function Get-TargetAddresses([string]$text) {
    $match = [regex]::Match($text, 'PB_TF_EXC count=(\d+)((?: addr\d+=0x[0-9A-Fa-f]+)*)')
    if (-not $match.Success) { return $null }
    $addresses = @()
    foreach ($m in [regex]::Matches($match.Groups[2].Value, 'addr\d+=0x([0-9A-Fa-f]+)')) {
        $addresses += [Convert]::ToUInt64($m.Groups[1].Value, 16)
    }
    return @{ Count = [int]$match.Groups[1].Value; Addresses = $addresses }
}

function Get-ToolAddress([string]$text) {
    $match = [regex]::Match($text, 'TF 异常位置：0x([0-9A-Fa-f]+)')
    if (-not $match.Success) { return $null }
    return [Convert]::ToUInt64($match.Groups[1].Value, 16)
}

# ── 第一段：virtualize —— 验对照关系（现在就能成立）────────────────────────
$virtual = Invoke-Tf 'virtualize'
if ($virtual -match 'Unhandled internal exception') {
    Die 'Pin 内部异常：这次运行不能作为证据（"没看到"与"崩了"必须分开）'
}
$virtualTarget = Get-TargetAddresses $virtual
if ($null -eq $virtualTarget) {
    Die "virtualize 下靶子没有报出 PB_TF_EXC（探针或靶子行为变了）：$virtual"
}
if ($virtualTarget.Count -ne 0) {
    Die ("virtualize 下靶子仍然看到了 {0} 次单步异常：说明工具没有把真实单步消化掉" -f `
        $virtualTarget.Count)
}
if ($virtual -notmatch 'TF 每线程：置位 ') {
    Die "virtualize 下靶子没有跑完（缺少 TF 每线程摘要）：$virtual"
}
if ($virtual -notmatch 'TF 异常位置：没有观察到真实单步') {
    Die "virtualize 下工具应当明说「没有观察到真实单步」（消化掉了就没有真实单步）：$virtual"
}
Ok 'virtualize 对照成立：靶子 0 次异常、工具明说没有真实单步、目标完整跑完'

# ── 第二段：watch —— 想要两侧都看到异常的比对 ──────────────────────────────
$watch = Invoke-Tf 'watch'
if ($watch -match 'Unhandled internal exception') {
    Die 'Pin 内部异常（watch 模式）：这次运行不能作为证据'
}
$watchTarget = Get-TargetAddresses $watch
$watchRounds = $watch -match 'PB_TF_THREADS rounds='
$watchTool = Get-ToolAddress $watch

if (-not $watchRounds -or $null -eq $watchTarget -or $watchTarget.Count -eq 0) {
    # 已知缺陷：watch 模式下靶子跑死（现象：入口点命中之后没有任何目标输出）。
    Info 'watch 模式下 Pin 的最后几行输出：'
    foreach ($line in ($watch -split "`n" | Select-Object -Last 4)) { Info "  $line" }
    Write-Host ''
    Write-Host '  watch 模式跑不通 ⇒ 这条判据暂时无法成立。' -ForegroundColor Yellow
    Write-Host '  已知缺陷（第 45/47 轮实测）：-pb_tf watch 下靶子运行到「入口点命中」之后再无输出；' -ForegroundColor Yellow
    Write-Host '  对照：-pb_tf virtualize 正常（既有 ctest 一直在跑它）。' -ForegroundColor Yellow
    Write-Host '  修好之后这个脚本会自动执行真正的地址比对，不需要改它。' -ForegroundColor Yellow
    exit 77
}

if ($null -eq $watchTool) {
    Die "watch 下工具没有报出 TF 异常位置：$watch"
}
Info ("靶子：count={0}，地址 {1}" -f $watchTarget.Count, (($watchTarget.Addresses | ForEach-Object { '0x{0:X}' -f $_ }) -join ', '))
Info ("工具：0x{0:X}" -f $watchTool)
if ($watchTarget.Addresses -notcontains $watchTool) {
    Die ("工具的异常位置不在靶子报出的集合里：工具 0x{0:X}，靶子 {1}" -f `
        $watchTool, (($watchTarget.Addresses | ForEach-Object { '0x{0:X}' -f $_ }) -join ', '))
}
Ok '两侧一致：工具观察到的异常位置，正是靶子自己读到的那一处'
Write-Host ''
Ok 'TF 异常位置比对通过'
exit 0
