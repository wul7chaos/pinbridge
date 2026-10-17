<#
.SYNOPSIS
  免 Pin 的"附加"：把 pinbridge 的转发层（→ agent）注入**已在运行的进程**。

.DESCRIPTION
  为什么需要它：Pin 自带的 `-pid` 附加在 Windows 上会断言失败**并把目标杀死**
  （实测证据见 docs/dev/attach-capability-gaps.md 第 2 节），所以在"目标已经在跑、
  不能重启、不能冻结"的场景下，唯一的通路是进程内注入。

  拓扑（与 Pin 启动模式共用同一套 C ABI）：

      CreateRemoteThread(LoadLibraryW, <pinbridge_bridge.dll 路径>)
          └─ 桥接层按 [PINBRIDGE_AGENT_DLL → 同目录 → 系统路径] 加载 pinbridge_agent.dll
      CreateRemoteThread(<远端模块基址 + 导出 RVA>, 参数)
          └─ int32_t 返回值的导出，返回值即线程退出码

  边界（写清楚比"看起来能用"重要）：
  * 只做"装载 + 调用导出"；**不碰目标内存**（除非你调用的导出自己会碰）；
  * 调用任何导出前先核对远端函数前 8 字节与本地一致，不一致就不调用；
  * 这是**原型**：还不能把 agent 的回环链路接进 Hub（缺口 G3），
    因此 Hub 的工具（pb_memory_read / pb_modules / 脚本）暂时用不到这条链路。

.PARAMETER TargetPid
  目标进程 PID。

.PARAMETER Dll
  要注入的 DLL。默认取模块自动生成的桥接层；它必须与 pinbridge_agent.dll 同目录。

.PARAMETER Call
  注入后要调用的导出名（按顺序）。默认只读 + 状态标记：pb_abi_version、pb_attach。

.PARAMETER CallArg
  传给每个导出的单个参数（x64 下即 RCX）。省略时用 TargetPid。

.PARAMETER Unload
  调用完导出后 `FreeLibrary` 卸载注入的模块（"分离"）。默认不卸载。

.EXAMPLE
  # 对一次性靶子做附加自检（推荐先拿它验证环境）
  pwsh -NoProfile -File scripts/inject-agent.ps1 -TargetPid 1234

.EXAMPLE
  # 只验 ABI、不标记附加
  pwsh -NoProfile -File scripts/inject-agent.ps1 -TargetPid 1234 -Call pb_abi_version
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][int]$TargetPid,
    [string]$Dll = (Join-Path (Split-Path -Parent $PSScriptRoot) 'native\build\pinbridge_bridge.dll'),
    [string[]]$Call = @('pb_abi_version', 'pb_attach'),
    [int]$CallArg = -1,
    [int]$TimeoutSec = 20,
    [switch]$Unload
)

$ErrorActionPreference = 'Continue'
Set-StrictMode -Version Latest

Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;

public static class PBN {
    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
    public struct MODULEENTRY32 {
        public uint dwSize; public uint th32ModuleID; public uint th32ProcessID;
        public uint GlblcntUsage; public uint ProccntUsage;
        public IntPtr modBaseAddr; public uint modBaseSize; public IntPtr hModule;
        [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 256)] public string szModule;
        [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 260)] public string szExePath;
    }
    [DllImport("kernel32.dll", SetLastError = true)] public static extern IntPtr OpenProcess(uint a, bool inherit, int pid);
    [DllImport("kernel32.dll", SetLastError = true)] public static extern IntPtr VirtualAllocEx(IntPtr h, IntPtr addr, IntPtr size, uint type, uint protect);
    [DllImport("kernel32.dll", SetLastError = true)] public static extern bool VirtualFreeEx(IntPtr h, IntPtr addr, IntPtr size, uint type);
    [DllImport("kernel32.dll", SetLastError = true)] public static extern bool WriteProcessMemory(IntPtr h, IntPtr addr, byte[] buf, IntPtr size, out IntPtr written);
    [DllImport("kernel32.dll", SetLastError = true)] public static extern bool ReadProcessMemory(IntPtr h, IntPtr addr, byte[] buf, IntPtr size, out IntPtr read);
    [DllImport("kernel32.dll", SetLastError = true)] public static extern IntPtr CreateRemoteThread(IntPtr h, IntPtr attr, IntPtr stack, IntPtr start, IntPtr param, uint flags, out uint tid);
    [DllImport("kernel32.dll", SetLastError = true)] public static extern uint WaitForSingleObject(IntPtr h, uint ms);
    [DllImport("kernel32.dll", SetLastError = true)] public static extern bool GetExitCodeThread(IntPtr h, out IntPtr code);
    [DllImport("kernel32.dll", SetLastError = true)] public static extern bool CloseHandle(IntPtr h);
    [DllImport("kernel32.dll", SetLastError = true)] public static extern IntPtr GetModuleHandleA(string name);
    [DllImport("kernel32.dll", SetLastError = true)] public static extern IntPtr GetProcAddress(IntPtr mod, string name);
    [DllImport("kernel32.dll", SetLastError = true)] public static extern IntPtr CreateToolhelp32Snapshot(uint flags, uint pid);
    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)] public static extern bool Module32FirstW(IntPtr snap, ref MODULEENTRY32 me);
    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)] public static extern bool Module32NextW(IntPtr snap, ref MODULEENTRY32 me);

    /// 完整 64 位模块基址。**不能**用 CreateRemoteThread 的退出码代替：
    /// 那是 32 位值，高位被截断，拿它算 RVA 会把远程线程跳到错误地址（目标会死）。
    public static long FindModuleBase(int pid, string name) {
        IntPtr snap = CreateToolhelp32Snapshot(0x8 | 0x10, (uint)pid);   // MODULE | 32BIT 都接受
        if (snap == (IntPtr)(-1)) return 0;
        try {
            MODULEENTRY32 me = new MODULEENTRY32();
            me.dwSize = (uint)Marshal.SizeOf(typeof(MODULEENTRY32));
            if (!Module32FirstW(snap, ref me)) return 0;
            do {
                if (string.Equals(me.szModule, name, StringComparison.OrdinalIgnoreCase))
                    return me.modBaseAddr.ToInt64();
                me.dwSize = (uint)Marshal.SizeOf(typeof(MODULEENTRY32));
            } while (Module32NextW(snap, ref me));
        } finally { CloseHandle(snap); }
        return 0;
    }
}
'@

function Info([string]$m) { Write-Host "  $m" }
function Ok([string]$m) { Write-Host "  ✓ $m" -ForegroundColor Green }
function Bad([string]$m) { Write-Host "  ✗ $m" -ForegroundColor Red }
function Alive([int]$p) { return [bool](Get-Process -Id $p -ErrorAction SilentlyContinue) }
function Hex8([byte[]]$b) { return (($b | ForEach-Object { $_.ToString('X2') }) -join ' ') }

if (-not (Alive $TargetPid)) { Bad "pid=$TargetPid 不存在"; exit 1 }
if (-not (Test-Path $Dll)) { Bad "找不到 $Dll（先构建：pwsh scripts/build-native.ps1）"; exit 1 }
$leaf = Split-Path $Dll -Leaf

# 本地加载同一份文件：只需要「导出地址 − 本地基址 = RVA」，RVA 在远端同值
$local = [System.Runtime.InteropServices.NativeLibrary]::Load($Dll)
$localBase = [PBN]::GetModuleHandleA($leaf)
if ($localBase -eq [IntPtr]::Zero) { Bad "本地拿不到 $leaf 的基址"; exit 1 }
Info ("本地 $leaf 基址 = 0x{0:X}" -f $localBase.ToInt64())

$h = [PBN]::OpenProcess(0x1F0FFF, $false, $TargetPid)   # ALL_ACCESS；失败通常是权限（用 sudo/管理员）
if ($h -eq [IntPtr]::Zero) {
    Bad ("OpenProcess 失败（Win32 " + [Runtime.InteropServices.Marshal]::GetLastWin32Error() + "）——目标进程可能更特权")
    exit 1
}

try {
    # ── 1) 远程写路径 + LoadLibraryW
    $bytes = [Text.Encoding]::Unicode.GetBytes($Dll + [char]0)      # Unicode：被 LoadLibraryW 用
    $pathPtr = [PBN]::VirtualAllocEx($h, [IntPtr]::Zero, [IntPtr]$bytes.Length, 0x3000, 0x04)
    $written = [IntPtr]::Zero
    if (-not [PBN]::WriteProcessMemory($h, $pathPtr, $bytes, [IntPtr]$bytes.Length, [ref]$written)) {
        Bad ("WriteProcessMemory 失败（Win32 " + [Runtime.InteropServices.Marshal]::GetLastWin32Error() + "）"); exit 1
    }

    $tid = 1
    $sw = [Diagnostics.Stopwatch]::StartNew()
    $loadLib = [PBN]::GetProcAddress([PBN]::GetModuleHandleA('kernel32.dll'), 'LoadLibraryW')
    $t = [PBN]::CreateRemoteThread($h, [IntPtr]::Zero, [IntPtr]::Zero, $loadLib, $pathPtr, 0, [ref]$tid)
    if ($t -eq [IntPtr]::Zero) {
        Bad ("CreateRemoteThread(LoadLibraryW) 失败（Win32 " + [Runtime.InteropServices.Marshal]::GetLastWin32Error() + "）"); exit 1
    }
    $wait = [PBN]::WaitForSingleObject($t, $TimeoutSec * 1000)
    $code = [IntPtr]::Zero
    [void][PBN]::GetExitCodeThread($t, [ref]$code)
    [void][PBN]::CloseHandle($t)
    $sw.Stop()
    # **必须先看等待结果**：超时的时候 GetExitCodeThread 会给出 STILL_ACTIVE(259)，
    # 而 259 既不是 0 也不是模块基址 —— 以前直接往下走，会拿一个"还没加载完"的状态
    # 当成功，随后算出来的 RVA 指向错误地址。
    if ($wait -ne 0) {
        Bad ("等待远端 LoadLibraryW 超时（等待返回值 $wait，$TimeoutSec 秒）：不继续，先确认目标是否被挂起")
        exit 1
    }
    if ($code -eq [IntPtr]::Zero) { Bad '远端 LoadLibraryW 返回 0（加载失败：DLL 位数不符/依赖缺失/被拦截）'; exit 1 }
    Ok ("已注入 $leaf：LoadLibrary 态耗时 $($sw.ElapsedMilliseconds) ms，目标 $(if (Alive $TargetPid) { '存活' } else { '已死亡' })")

    # ── 2) 完整 64 位基址（Toolhelp，不能信退出码）
    $remoteBase = [PBN]::FindModuleBase($TargetPid, $leaf)
    if ($remoteBase -eq 0) { Bad 'Toolhelp 看不到远端模块（加载可能已回滚）'; exit 1 }
    Info ("远端 $leaf 基址 = 0x{0:X}" -f $remoteBase)

    # ── 3) 逐个导出：先核对远端序言，再调用
    foreach ($name in $Call) {
        $loc = [System.Runtime.InteropServices.NativeLibrary]::GetExport($local, $name)
        if ($loc -eq [IntPtr]::Zero) { Info "跳过 $name（本地 DLL 没有这个导出）"; continue }
        $rva = $loc.ToInt64() - $localBase.ToInt64()
        $rem = [IntPtr]($remoteBase + $rva)

        $l8 = New-Object byte[] 8; [Runtime.InteropServices.Marshal]::Copy($loc, $l8, 0, 8)
        $r8 = New-Object byte[] 8
        $got = [IntPtr]::Zero
        if (-not [PBN]::ReadProcessMemory($h, $rem, $r8, [IntPtr]8, [ref]$got)) {
            Bad ("$name : 远端 0x{0:X} 读不到内容，**不调用**" -f $rem.ToInt64()); continue
        }
        $same = $true
        for ($i = 0; $i -lt 8; $i++) { if ($l8[$i] -ne $r8[$i]) { $same = $false; break } }
        if (-not $same) {
            Bad ("$name : 远端序言与本地不一致（{0} vs {1}），**不调用**" -f (Hex8 $l8), (Hex8 $r8)); continue
        }

        $param = if ($CallArg -ge 0) { [IntPtr]$CallArg } else { [IntPtr]$TargetPid }
        $t2 = [PBN]::CreateRemoteThread($h, [IntPtr]::Zero, [IntPtr]::Zero, $rem, $param, 0, [ref]$tid)
        if ($t2 -eq [IntPtr]::Zero) {
            Bad ("${name}: CreateRemoteThread 失败（Win32 " + [Runtime.InteropServices.Marshal]::GetLastWin32Error() + "）"); continue
        }
        $wait2 = [PBN]::WaitForSingleObject($t2, $TimeoutSec * 1000)
        $c2 = [IntPtr]::Zero
        [void][PBN]::GetExitCodeThread($t2, [ref]$c2)
        [void][PBN]::CloseHandle($t2)
        # 同上的道理：超时时的 259 不是 ABI 返回值，别把它当成一个"状态码"报出去。
        if ($wait2 -ne 0) { Bad ("${name}: 调用超时（等待返回值 $wait2），返回值不可信"); continue }
        # 约定：0 = PB_STATUS_OK（见 docs/specs/c-abi.md）
        Ok ("$name(0x{0:X}) → 0x{1:X}  [rva 0x{2:X}]" -f $param.ToInt64(), $c2.ToInt64(), $rva)
        if (-not (Alive $TargetPid)) { Bad "$name 调用后目标已死亡"; exit 1 }
    }

    # ── 4) 可选：分离（FreeLibrary）。默认不做——卸载正在被别人用着的 DLL 本身就是风险
    if ($Unload) {
        $freeLib = [PBN]::GetProcAddress([PBN]::GetModuleHandleA('kernel32.dll'), 'FreeLibrary')
        $t3 = [PBN]::CreateRemoteThread($h, [IntPtr]::Zero, [IntPtr]::Zero, $freeLib, [IntPtr]$remoteBase, 0, [ref]$tid)
        if ($t3 -ne [IntPtr]::Zero) {
            [void][PBN]::WaitForSingleObject($t3, $TimeoutSec * 1000)
            $c3 = [IntPtr]::Zero
            [void][PBN]::GetExitCodeThread($t3, [ref]$c3)
            [void][PBN]::CloseHandle($t3)
            Info ("FreeLibrary → 0x{0:X}（非 0 表示卸载成功）" -f $c3.ToInt64())
        }
    }

    [void][PBN]::VirtualFreeEx($h, $pathPtr, [IntPtr]::Zero, 0x8000)
    Info "目标进程最终状态 = $(if (Alive $TargetPid) { '存活' } else { '已死亡' })"
}
finally { [void][PBN]::CloseHandle($h) }
