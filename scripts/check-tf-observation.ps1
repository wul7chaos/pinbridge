<#
.SYNOPSIS
  验证 TF 单步在三种模式下的**可观察契约**：
  `virtualize` 让目标看不到单步并及时跑完；`watch` 下目标按早已记录的预期死于
  未接住的单步异常（`STATUS_SINGLE_STEP`）。

.DESCRIPTION
  为什么这条要脚本而不是一条 ctest 正则：

    * 断言不止一条（目标 0 次异常 + 工具明说没有真实单步 + 目标跑完轮数 + 进程退出码），
      而 `PASS_REGULAR_EXPRESSION` 是"任一匹配即通过"，表达不了"且"；
    * 退出码是判据的一半（`watch` 那档的结论就在退出码上），正则看不到它。

  关于 `watch` 那一档 —— **它不是缺陷，是早已记录在案的行为**：
  `docs/roadmap.md` 的 TF 条目里写着"Pin 下不虚拟化则进程以 `0x80000004` 终止"。
  本轮把这条从"以为是新缺陷"改成**显式断言**（实测退出码 = `-2147483644` = `0x80000004`
  = `STATUS_SINGLE_STEP`），这样它从"未知失败"变成"已知预期"：

    * 预期还成立 ⇒ 通过（并且任何人改动 TF 隐藏逻辑时会被这条拦住）；
    * 预期改变了（例如将来能在 watch 下活下来）⇒ 这条会红，提示去更新文档与这里。

  **还没做到的那件事**（不在本脚本的断言里，见 `docs/dev/tf-location-in-progress.md`）：
  "工具看到的单步位置"与"靶子自己读到的 `ExceptionAddress`"**等值**这条判据。
  它需要目标能**活过**真实单步才能取到两侧数字 —— `watch` 下目标会死，
  所以要换一条路径（在靶子里装 VEH 而不是靠 `__try/__except`）。

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

# `STATUS_SINGLE_STEP` = 0x80000004。Pin 把它按有符号 32 位传出来，
# 于是外壳看到的退出码是 -2147483644 —— 两个都要认（不同外壳/平台的呈现可能不同）。
$SingleStepStatus = @(-2147483644, [int64]0x80000004)

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
        $text = (& $pin -t '.\pinbridge.dll' -pb_mem 0 -pb_tf $mode -pb_events 2 `
                -- $target --tf-threads 1 2>&1 | Out-String)
        return @{ Text = $text; Code = $LASTEXITCODE }
    } finally { Pop-Location }
}

# ── 一档：virtualize —— 目标看不到单步，但确实在"单步"（读回 TF 仍置位）──────
$virtual = Invoke-Tf 'virtualize'
if ($virtual.Text -match 'Unhandled internal exception') {
    Die 'Pin 内部异常：这次运行不能作为证据（"没看到"与"崩了"必须分开）'
}
if ($virtual.Code -ne 0) {
    Die ("virtualize 下退出码不是 0（{0}）：目标应当正常跑完" -f $virtual.Code)
}
$match = [regex]::Match($virtual.Text, 'PB_TF_EXC count=(\d+)')
if (-not $match.Success) {
    Die "virtualize 下靶子没有报出 PB_TF_EXC（探针或靶子行为变了）"
}
if ([int]$match.Groups[1].Value -ne 0) {
    Die ("virtualize 下靶子看到了 {0} 次真实单步：说明工具没有把真实 TF 剥干净" -f `
        $match.Groups[1].Value)
}
if ($virtual.Text -notmatch 'PB_TF_THREADS rounds=1 .*read_tf=[1-9]') {
    Die "virtualize 下靶子的「读回 TF 仍为置位」没有出现：虚拟化的假象不成立"
}
if ($virtual.Text -notmatch 'TF 异常位置：没有观察到真实单步') {
    Die "virtualize 下工具应当明说「没有观察到真实单步」（剥干净了就没有真实单步）"
}
Ok 'virtualize：目标 0 次真实单步、读回 TF 仍置位、工具明说没有真实单步、退出码 0'

# ── 二档：watch —— 目标按预期死于未接住的单步异常 ──────────────────────────
$watch = Invoke-Tf 'watch'
if ($watch.Text -match 'Unhandled internal exception') {
    Die 'Pin 内部异常（watch）：这次运行不能作为证据'
}
if ($SingleStepStatus -notcontains $watch.Code) {
    Die ("watch 下退出码是 {0}，而预期是 STATUS_SINGLE_STEP（0x80000004 = -2147483644）。" +
         "若这是有意变化，请同时更新 docs/roadmap.md 的 TF 条目与本脚本") -f $watch.Code
}
if ($watch.Text -match 'PB_TF_EXC count=') {
    Die 'watch 下靶子居然报出了异常计数：目标活过了真实单步，判据需要重写（这是好消息，别硬套预期）'
}
Ok ("watch：目标按预期死在未接住的单步异常上（退出码 {0} = 0x80000004）" -f $watch.Code)

Write-Host ''
Ok 'TF 分模式契约成立（地址等值那条判据仍需 VEH 路径，见 docs/dev/tf-location-in-progress.md）'
exit 0
