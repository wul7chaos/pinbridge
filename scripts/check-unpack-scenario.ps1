<#
.SYNOPSIS
  验证"脱壳场景"的两个关键时机能被观察到：**改内存属性**与**写盘**
  （需要 Intel Pin 与已构建的 Pin tool）。

.DESCRIPTION
  为什么这条要脚本化，而不写成一条普通 ctest 命令：

  判据是"某两个**系统调用号**必须出现过"，而**号随 Windows 版本变化**——
  写死在用例里会让它在别的机器上变成"工具坏了"这种假失败。
  系统调用号的唯一可靠来源是**运行中**的 ntdll 桩，而只有靶子自己能读它。
  于是判据必须分两步：

    1. 让靶子报出它要用到的两个号（`--print-ssns`，它自己读 ntdll 的桩）；
    2. 用这两个号去跑 Pin 工具，要求"**必须出现过**"。

  ctest 的一条命令做不到"先读输出再决定参数"，所以用脚本。

  给出四条判定，缺一条都会让结论虚：

    1. 正向：要求观察的两个号都出现了（脱壳的两个动作确实被看见）；
    2. 反向对照：要求一个**不存在**的号时必须报失败——
       否则"出现了"可能只是因为这条检查根本没在看；
    3. 靶子侧确实改了内存属性（`previous` 是原属性，说明调用成功）；
    4. 没有异常退出（Pin 内部故障会让"没看到"与"崩了"混在一起）。

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

# ── 第一步：问靶子要系统调用号 ────────────────────────────────────────────
$ssnLine = (& $target --print-ssns 2>&1 | Out-String).Trim()
Info "靶子报出的系统调用号：$ssnLine"
if ($ssnLine -notmatch 'PB_SSN protect=0x([0-9A-Fa-f]+) write=0x([0-9A-Fa-f]+)') {
    Skip "读不到系统调用号（ntdll 的桩形状与预期不符）：$ssnLine"
}
$protect = $Matches[1]
$write = $Matches[2]
if ([Convert]::ToUInt32($protect, 16) -eq 0 -or [Convert]::ToUInt32($write, 16) -eq 0) {
    Skip "系统调用号读成了 0（桩被 hook 或平台不符）"
}

# ── 第二步：用这两个号跑 Pin ──────────────────────────────────────────────
function Invoke-Pin([string]$requireSsn) {
    Push-Location $dist
    try {
        $out = & $pin -t '.\pinbridge.dll' -pb_syscalls 1 -pb_syscall_max 200000 `
            -pb_require_ssn $requireSsn -pb_events 2 `
            -- $target --unpack-scenario 2>&1 | Out-String
        return $out
    } finally { Pop-Location }
}

# 必须带 `0x` 前缀：不带前缀的纯数字会被工具按**十进制**解析，
# 而 `50` 与 `0x50` 是两个完全不同的系统调用号（第一版就是这么失败的）。
$positive = Invoke-Pin "0x$protect,0x$write"
Info ($positive -split "`n" | Where-Object { $_ -match '观察到系统调用号|要求观察的|PB_UNPACK|溢出丢弃' } | ForEach-Object { $_.Trim() } | Out-String).Trim()

if ($positive -match 'Unhandled internal exception') {
    Die 'Pin 内部异常：这次运行不能作为证据（"没看到"与"崩了"必须分开）'
}
if ($positive -notmatch 'PB_UNPACK ok') {
    Die '靶子没有完成脱壳场景（--unpack-scenario 失败）'
}
# 判定 3：previous 是**改之前**的属性。PAGE_READWRITE = 0x04。
if ($positive -notmatch 'PB_UNPACK ok region=0x[0-9A-Fa-f]+ previous=0x0*4\b') {
    Die '靶子的内存属性改动的 previous 不是 PAGE_READWRITE(0x4)：改属性这一步可能没真的发生'
}
if ($positive -match '失败：要求观察到系统调用号') {
    Die "要求观察的系统调用号没有全部出现（脱壳动作没被看见）：$protect,$write"
}
if ($positive -notmatch '✓ 要求观察的系统调用号全部出现') {
    Die '没有看到"全部出现"的确认行（判据本身没跑）'
}
Ok "改内存属性与写盘都被观察到（protect=0x$protect write=0x$write）"

# ── 第三步：反向对照 ─────────────────────────────────────────────────────
# 要求一个**肯定不存在**的号（0x999 在 Windows 上没有任何系统调用用它）。
# 这一条是整个脚本里最容易被省掉、也最不该省的一步：没有它，
# "正向通过"完全可能只是因为 `-pb_require_ssn` 根本没生效。
$negative = Invoke-Pin '0x999'
if ($negative -notmatch '失败：要求观察到系统调用号 0x999') {
    Die '反向对照失败：要求一个不存在的系统调用号竟然没有报失败（这条检查没有在看）'
}
Ok '反向对照成立（要求不存在的号会失败，说明检查确实在看）'

Write-Host ''
Ok '脱壳场景验收通过：改内存属性 + 写盘都能通过系统调用时机观察到'
exit 0
