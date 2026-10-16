// PinBridge 的 Pin tool：**探针层**。
//
// ───────────────────────────────────────────────────────────────────────────
// 它在整个架构里的位置
// ───────────────────────────────────────────────────────────────────────────
//   pin.exe
//     └─ pinbridge.dll（本文件：探针 + 上下文控制）
//           └─ pinbridge_bridge.dll（**复用**已经测过的转发层）
//                 └─ pinbridge_agent.dll（Rust：事件、脚本宿主、录制）
//
// 一个刻意的取舍：本文件**不重复**实现"加载 agent、校验 ABI、转发调用"那一套。
// 那些逻辑已经在 `agent_bridge.cpp` 里，并且被 CTest 与 Hub 侧集成测试覆盖过。
// Pin tool 唯一新增的东西是**探针**——把它做成"探针 + 现成转发层"，
// 意味着这里不可能出现"两条入口对同一条命令理解不同"的问题。
//
// ───────────────────────────────────────────────────────────────────────────
// M1 当前范围
// ───────────────────────────────────────────────────────────────────────────
// 已经真实可用：
//   * Pin 初始化、附加到目标、下发能力、开始/结束录制；
//   * **入口点探针**：目标主模块的入口被插桩，命中时向 agent 推一条真实事件。
//
// 还没做（后续提交，见 docs/roadmap.md 的 M2）：
//   * 系统调用 Hook（syscall_enter/exit）；
//   * 指令模板匹配（pattern_hit）；
//   * 内存读写观测（mem_read/mem_write）；
//   * TF 虚拟化（flags_virtualize.cpp）。
//
// 命令行旋钮：
//   -pb_trace <path>     录制轨迹到该路径（启动时开始，进程退出时收尾）
//   -pb_bridge <path>    指定 pinbridge_bridge.dll（默认：本 DLL 同目录）
//   -pb_caps <all|ro>    下发的能力集（默认 ro＝只读基线 257）
//   -pb_events <n>       退出时打印最后 n 条事件（默认 16，用于证明事件真的出来了）

#include "pin.H"

// ──────────────────────────────────────────────────────────────────────────
// 为什么**不**包含 <windows.h>
// ──────────────────────────────────────────────────────────────────────────
// Pin 自带一份部分重实现的 `minwindef.h`（`extras/crt/include/my_minwindef.h`
// 第 39 行也有 `typedef int BOOL;`），当它与真实 Windows 头出现在同一个编译
// 单元里时，MSVC 会报一串"BOOL：不明确的符号"（实测必现，且错误位置全落在
// 系统头里，看起来与自己的代码无关）。
//
// 我们只需要三个 Win32 入口，直接按名字声明比把两套头拉进来更省事：
// 声明即契约，依赖面一眼可见，也不用跟 Pin 的包含顺序较劲。
extern "C" {
__declspec(dllimport) void* __stdcall LoadLibraryA(const char* library_name);
__declspec(dllimport) void* __stdcall GetProcAddress(void* module, const char* procedure_name);
// 只比最小集合多一个：把模块**钉住**，让加载器在退出时也不卸载它
// （理由见 pin_loaded_modules 的注释——那是实测踩出来的崩溃）。
__declspec(dllimport) int __stdcall GetModuleHandleExA(unsigned long flags,
                                                       const char* module_name,
                                                       void** module);
__declspec(dllimport) void __stdcall Sleep(unsigned long milliseconds);
}

/// `GetModuleHandleExA` 的 flags：不引入 windows.h，所以按名字自己声明常量。
constexpr unsigned long kModuleHandlePin = 0x00000001;

using ModuleHandle = void*;

#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <string>


namespace {

// ══════════════════════════════════════════════════════ bridge 导出（转发层）
// 与 crates/pinbridge-sys/src/loader.rs 的表同名同型；签名见 include/pinbridge.h。
struct PbEventRaw {
  // 用 Pin 自己的类型名（`UINT64` 等）而不是 `uint64_t`/`std::uint64_t`：
  // Pin 工具链自带一份 CRT/STL，实测这个编译单元里既没有全局 `uint64_t`
  // 也没有 `std::uint64_t`（Pin 的 gen/types_foundation.PH 是从 C 的
  // `uint64_t` typedef 出来的，但在工具侧不一定可见）。宽度一致，直接用它的。
  UINT64 sequence;
  UINT8 kind;
  UINT8 reserved[3];
  UINT32 thread_id;
  UINT64 address;
  UINT64 args[8];
};

// 与 crates/pinbridge-sys/src/abi.rs 的 PbEventRaw 显式布局一致。
// 跨语言边界的结构体不校验布局，错误会表现为"能跑但数据是垃圾"——最难查的一类。
static_assert(sizeof(PbEventRaw) == 88, "pb_event_raw 必须是 88 字节（与 Rust 侧一致）");

constexpr INT32 PB_OK = 0;

struct BridgeApi {
  ModuleHandle module = nullptr;
  UINT32 (*abi_version)() = nullptr;
  INT32 (*attach)(UINT32) = nullptr;
  INT32 (*set_capabilities)(UINT32) = nullptr;
  INT32 (*allocate_sequence)() = nullptr;
  INT32 (*push_event)(const PbEventRaw*) = nullptr;
  /// 交线程的寄存器快照。**可选**符号：旧版转发层没有它只是拿不到寄存器，
  /// 不该让整个采集起不来（见 agent_bridge.cpp 里的同名说明）。
  INT32 (*set_thread_registers)(UINT32, const UINT64*, UINT32) = nullptr;
  INT32 (*drain_events)(PbEventRaw*, UINT32, UINT32*) = nullptr;
  INT32 (*start_trace)(const char*, UINT32) = nullptr;
  INT32 (*stop_trace)(UINT64*, UINT64*) = nullptr;
  const char* (*last_error)() = nullptr;
  UINT64 (*stats)(UINT32) = nullptr;


};

BridgeApi g_api;

// 命令行旋钮
KNOB<std::string> KnobTrace(KNOB_MODE_WRITEONCE, "pintool", "pb_trace", "",
                            "录制轨迹到该路径（默认不录制）");
KNOB<std::string> KnobBridge(KNOB_MODE_WRITEONCE, "pintool", "pb_bridge", "",
                             "pinbridge_bridge.dll 的路径（默认取本 DLL 同目录）");
KNOB<std::string> KnobCaps(KNOB_MODE_WRITEONCE, "pintool", "pb_caps", "ro",
                           "下发的能力集：all（全部）或 ro（只读基线）");
// 数值旋钮的默认值用字符串给：KNOB<UINT32> 的重载在 MSVC 下
// 会拒绝 `int` 字面量（"C2665 没有重载函数可以转换所有参数类型"）。
KNOB<UINT32> KnobEvents(KNOB_MODE_WRITEONCE, "pintool", "pb_events", "16",
                        "退出时打印最后 n 条事件");
// 现在可以默认打开：回调已改成"只入队"，跨边界由内部线程做，
// 之前 5/5 稳定崩溃的那条命令现在 5/5 正常（判据进了 ctest）。
KNOB<BOOL> KnobSyscalls(KNOB_MODE_WRITEONCE, "pintool", "pb_syscalls", "1",
                        "是否插装系统调用（1/0）");
KNOB<std::string> KnobSyscallMax(KNOB_MODE_WRITEONCE, "pintool", "pb_syscall_max", "4096",
                                 "捕获多少条系统调用事件后停止插装（0 表示不限制）");
// 自动化判据用：捕获数低于这个值时工具**自己以非零退出**。
// 为什么放在工具里而不是靠测试脚本数日志行：退出码是机器可判定的，
// 而"数日志里出现了几次"既脆弱又会被日志格式改动打断。
KNOB<std::string> KnobRequireSyscalls(KNOB_MODE_WRITEONCE, "pintool", "pb_require_syscalls", "0",
                                      "捕获到的系统调用**进入**事件少于该值时报告失败（0 表示不检查）");
KNOB<std::string> KnobRequireSyscallExits(KNOB_MODE_WRITEONCE, "pintool",
                                          "pb_require_syscall_exits", "0",
                                          "捕获到的系统调用**返回**事件少于该值时报告失败（0 表示不检查）");
// 内存探针：**默认关闭**。
//
// 为什么不像系统调用探针那样默认开：它是高频探针，实测在默认配置下
// （只采写入、只主模块）就能在启动阶段产生上千条事件并丢掉一半——
// 一个"打开了但丢一半数据"的探针比不打开更危险，因为使用者会以为
// 看到的是全部。要用就显式打开，并用 `-pb_mem_range`/`-pb_mem_max` 收紧。
KNOB<std::string> KnobMem(KNOB_MODE_WRITEONCE, "pintool", "pb_mem", "0",
                          "内存访问探针：0（关，默认）/ w（只写）/ r（只读）/ rw（都看）");
KNOB<std::string> KnobMemRange(KNOB_MODE_WRITEONCE, "pintool", "pb_mem_range", "",
                               "只看落在 lo-hi 内的地址（十六进制，形如 7ff6a000-7ff6a400；空表示不限）");
KNOB<BOOL> KnobMemAllModules(KNOB_MODE_WRITEONCE, "pintool", "pb_mem_all_modules", "0",
                             "内存探针是否覆盖所有模块（默认只主模块）");
KNOB<std::string> KnobMemMax(KNOB_MODE_WRITEONCE, "pintool", "pb_mem_max", "20000",
                             "捕获多少条内存事件后停止插桩（0 表示不限制）");
KNOB<std::string> KnobRequireMem(KNOB_MODE_WRITEONCE, "pintool", "pb_require_mem", "0",
                                 "捕获到的内存事件少于该值时报告失败（0 表示不检查）");
// 指令模板（字节签名）：只扫主模块，命中点会被插桩。
KNOB<std::string> KnobPattern(KNOB_MODE_WRITEONCE, "pintool", "pb_pattern", "",
                              "模板：id:name:hex（hex 两位一组、? 通配），多条用 ; 分隔");
KNOB<std::string> KnobPatternMax(KNOB_MODE_WRITEONCE, "pintool", "pb_pattern_max", "64",
                                 "每个模板最多登记多少个命中地址");
KNOB<std::string> KnobRequirePatternHits(KNOB_MODE_WRITEONCE, "pintool",
                                         "pb_require_pattern_hits", "0",
                                         "模板命中次数少于该值时报告失败（0 表示不检查）");
// TF 单步：默认 watch（只观察，不改目标行为；成本只在异常发生时才有）。
KNOB<std::string> KnobTf(KNOB_MODE_WRITEONCE, "pintool", "pb_tf", "watch",
                         "TF 单步：off（不注册）/ watch（只观察）/ virtualize（虚拟化，会改变目标行为）");
KNOB<std::string> KnobRequireTfSets(KNOB_MODE_WRITEONCE, "pintool", "pb_require_tf_sets", "0",
                                    "观察到的 TF 置位意图少于该值时报告失败（0 表示不检查）");

/// 能力位（与 pinbridge.h / capability.rs 一致；只用到这两个组合）。
constexpr UINT32 PB_CAP_MEM_READ = 1u << 0;
constexpr UINT32 PB_CAP_TRACE_RECORD = 1u << 8;
constexpr UINT32 PB_CAP_READ_ONLY_BASELINE = PB_CAP_MEM_READ | PB_CAP_TRACE_RECORD;
constexpr UINT32 PB_CAP_ALL = 0xFFFu;

void report(const char* format, ...) {
  va_list args;
  va_start(args, format);
  std::fputs("[pinbridge] ", stderr);
  std::vfprintf(stderr, format, args);
  std::fputc('\n', stderr);
  va_end(args);
}

/// 把最后一条错误文本取回来（跨语言边界唯一的"人话"来源）。
std::string last_error_text() {
  if (g_api.last_error == nullptr) {
    return "(bridge 没有导出 pb_last_error)";
  }
  const char* text = g_api.last_error();
  return text != nullptr ? std::string(text) : std::string();
}

/// 解析十六进制（`0x` 前缀可选）。用于 `-pb_mem_range`。
///
/// 与 `parse_u64` 分开而不是加个进制参数：出现非法字符时的**语义**不同
/// （这里是"这不是个地址"），混在一个函数里报错会含糊。
UINT64 parse_hex_u64(const char* text) {
  if (text == nullptr) {
    return 0;
  }
  UINT64 value = 0;
  for (const char* cursor = text; *cursor != 0; ++cursor) {
    const char c = *cursor;
    if (c == 'x' || c == 'X') {
      continue;  // 跳过 0x 前缀
    }
    UINT64 digit = 0;
    if (c >= '0' && c <= '9') {
      digit = static_cast<UINT64>(c - '0');
    } else if (c >= 'a' && c <= 'f') {
      digit = static_cast<UINT64>(c - 'a' + 10);
    } else if (c >= 'A' && c <= 'F') {
      digit = static_cast<UINT64>(c - 'A' + 10);
    } else {
      continue;  // 空白等无关字符直接跳过
    }
    value = value * 16 + digit;
  }
  return value;
}

/// 解析无符号十进制。
///
/// **自己写而不是用 `std::strtoull`**：Pin 自带的 CRT 不提供 `std::` 那套整数
/// 解析函数（实测报"strtoull 不是 std 的成员"）。为一个"把命令行里的十进制
/// 变成数字"的需求去和它的头文件较劲不值得；而且自己写还能顺手把**非法输入**
/// 处理成明确回退并报出来，而不是静默当成 0。
///
/// 放在 `report` 之后是因为失败时要借助它——这正是"先声明后使用"的正常约束，
/// 不值得为它再加一个前置声明。
UINT64 parse_u64(const char* text, UINT64 fallback) {
  if (text == nullptr || *text == '\0') {
    return fallback;
  }
  UINT64 value = 0;
  for (const char* cursor = text; *cursor != '\0'; ++cursor) {
    if (*cursor < '0' || *cursor > '9') {
      report("无法解析的数值 \"%s\"，按默认值 %llu 处理", text,
             static_cast<unsigned long long>(fallback));
      return fallback;
    }
    value = value * 10 + static_cast<UINT64>(*cursor - '0');
  }
  return value;
}

template <typename T>
T resolve(ModuleHandle module, const char* name) {
  return reinterpret_cast<T>(reinterpret_cast<void*>(::GetProcAddress(module, name)));
}

/// 加载并校验转发层。**先校验 ABI 版本，再解析其余符号**：
/// 反过来的话，版本不匹配的 DLL 里同名函数会被按错误的签名调用。
bool load_bridge() {
  // 默认按 DLL 名加载：Windows 的搜索顺序里包含**当前工作目录**，
  // 所以 `cd native\\build\\dist` 之后直接跑就能找到同目录的桥接层。
  // 拿不自己的模块路径是有意为之——为它引入 windows.h 会带来上面那个
  // "BOOL 不明确"的连锁问题，而收益只是省掉一次 cd。
  std::string path = KnobBridge.Value();
  if (path.empty()) {
    path = "pinbridge_bridge.dll";
  }
  ModuleHandle module = ::LoadLibraryA(path.c_str());
  if (module == nullptr) {
    report("无法加载转发层 %s", path.c_str());
    report("  → 请先 cd 到 dist 目录，或用 -pb_bridge <完整路径> 指定");
    return false;
  }
  g_api.module = module;
  g_api.abi_version = resolve<UINT32 (*)()>(module, "pb_abi_version");
  if (g_api.abi_version == nullptr) {
    report("转发层缺少 pb_abi_version，无法校验 ABI");
    return false;
  }
  const UINT32 version = g_api.abi_version();
  if (version != 1u) {
    report("ABI 版本不匹配：Pin tool 期望 1，转发层报 %u（拒绝继续）", version);
    return false;
  }
  g_api.attach = resolve<INT32 (*)(UINT32)>(module, "pb_attach");
  g_api.set_capabilities = resolve<INT32 (*)(UINT32)>(module, "pb_set_capabilities");
  g_api.allocate_sequence = resolve<INT32 (*)()>(module, "pb_allocate_sequence");
  g_api.push_event = resolve<INT32 (*)(const PbEventRaw*)>(module, "pb_push_event");
  // 可选符号：解析失败只是"拿不到寄存器"，不算导出面不全（不参与下面的检查）。
  g_api.set_thread_registers =
      resolve<INT32 (*)(UINT32, const UINT64*, UINT32)>(module, "pb_set_thread_registers");
  g_api.drain_events = resolve<INT32 (*)(PbEventRaw*, UINT32, UINT32*)>(module, "pb_drain_events");
  g_api.start_trace = resolve<INT32 (*)(const char*, UINT32)>(module, "pb_start_trace");
  g_api.stop_trace = resolve<INT32 (*)(UINT64*, UINT64*)>(module, "pb_stop_trace");
  g_api.last_error = resolve<const char* (*)()>(module, "pb_last_error");
  g_api.stats = resolve<UINT64 (*)(UINT32)>(module, "pb_stats");
  if (g_api.attach == nullptr || g_api.set_capabilities == nullptr || g_api.push_event == nullptr ||
      g_api.allocate_sequence == nullptr || g_api.drain_events == nullptr ||
      g_api.start_trace == nullptr || g_api.stop_trace == nullptr) {
    report("转发层导出面不完整（ABI 版本一致但缺符号）");
    return false;
  }
  report("转发层就绪：%s（ABI %u）", path.c_str(), version);
  return true;
}

/// 把转发层与 agent 的模块句柄**钉住**，让加载器在进程退出时也不卸载它们。
///
/// ── 为什么这是必需的（实测，不是防御性编程）──────────────────────────────
/// 目标退出时 `ExitProcess` 会逐个 `FreeLibrary` 已加载的 DLL，**包括我们自己的
/// 桥接层与 agent**。而系统调用探针此刻仍在命中（`NtTerminateProcess` 本身就是
/// 一次系统调用），于是回调走进"CRT 私有数据已被释放"的模块里，得到
/// `RECEIVED_ACCESS_FAULT, Access Address = 0x8`——故障 IP 落在桥接层映像内
/// （`+0x19fa`，位于导出表之前，即静态 CRT 区域），栈上毫无线索，
/// 而且是**竞态**：同样参数有时崩、有时不崩。
///
/// 我们拿不到更早的退出信号（Pin 的 Fini 在各 DLL 的 `DllMain(DETACH)` **之后**
/// 才跑；RTN 级钩子要 `PIN_StartProgramProbed()`，实测被拒），
/// 所以改成**消除窗口本身**：钉住模块 → 它们不会被卸载 → CRT 状态一直在。
///
/// 代价：这两个 DLL 驻留到进程结束。对一个插装工具这是合理的
/// （进程本来就要结束，而且我们本来就在它里面）。
void pin_loaded_modules() {
  const char* names[] = {"pinbridge_bridge.dll", "pinbridge_agent.dll"};
  for (const char* name : names) {
    void* module = nullptr;
    if (::GetModuleHandleExA(kModuleHandlePin, name, &module) != 0) {
      report("已钉住模块 %s（退出时不会被卸载，探针可安全走到最后）", name);
    } else {
      // 不静默：钉不住就意味着退出期仍可能踩到"CRT 已拆"的窗口。
      report("警告：无法钉住 %s —— 退出期探针可能崩溃（见本函数注释）", name);
    }
  }
}

// ══════════════════════════════════════════════════════════════════ 探针
volatile bool g_entry_hit = false;
ADDRINT g_entry_address = 0;

/// 最近一次抓到的寄存器快照，以及它属于哪条线程。
///
/// 为什么放在文件级的这一片（而不是 `ThreadStats` 里）：入口探针就在下面几十行，
/// 那时 `ThreadStats` 还是**不完整类型**（它的定义在几百行之后），只能声明指针、
/// 不能解引用成员——硬写会得到 `C2027: 使用了未定义类型`。
///
/// 消费者（`flush_ring`）只在**线程号对得上**时才用它：对不上就是别条线程的快照，
/// 张冠李戴比"没有"更糟。
UINT64 g_last_registers[8] = {0, 0, 0, 0, 0, 0, 0, 0};
UINT32 g_last_registers_thread = 0;
volatile bool g_last_registers_valid = false;


// 系统调用探针的开关、上限与观测计数（定义见下方"系统调用探针"一节）
bool pb_syscall_enabled = true;
UINT64 g_syscall_cap = 0;
/// 捕获数下限（见 KnobRequireSyscalls / KnobRequireMem）。
UINT64 g_required_syscalls = 0;
UINT64 g_required_syscall_exits = 0;
UINT64 g_required_mem = 0;
UINT64 g_required_pattern_hits = 0;
UINT64 g_required_tf_sets = 0;
volatile long g_syscall_sites = 0;
/// 返回点插装数（挂在 `syscall` 的下一条指令上）。
volatile long g_syscall_exit_sites = 0;
/// 记录到的系统调用返回地址数，以及其中被成功插装的数量。
///
/// `syscall` 是 Pin 里的**基本块终止指令**，所以 `INS_Next(syscall)` 无效
/// （实测 59/59 都"没有返回点"）。返回地址只能自己算：`syscall 地址 + 指令长度`，
/// 然后在插桩期按地址把它认出来。
UINT64 g_syscall_returns_recorded = 0;
UINT64 g_syscall_returns_matched = 0;
/// 没有配对进入的返回事件数（`int 2Eh` 回退路径与 syscall 路径共用同一个 ret）。
volatile UINT64 g_syscall_exit_without_enter = 0;

constexpr UINT32 kMaxSyscallReturns = 512;
ADDRINT g_syscall_returns[kMaxSyscallReturns];
UINT32 g_syscall_return_count = 0;

/// 登记一个"系统调用返回点"地址。
void record_syscall_return(ADDRINT address) {
  ++g_syscall_returns_recorded;
  for (UINT32 i = 0; i < g_syscall_return_count; ++i) {
    if (g_syscall_returns[i] == address) {
      return;  // 同一个桩只记一次
    }
  }
  if (g_syscall_return_count >= kMaxSyscallReturns) {
    return;  // 上限：命中数在收尾统计里对不上时能看出来
  }
  UINT32 position = g_syscall_return_count;
  while (position > 0 && g_syscall_returns[position - 1] > address) {
    g_syscall_returns[position] = g_syscall_returns[position - 1];
    --position;
  }
  g_syscall_returns[position] = address;
  ++g_syscall_return_count;
}

/// 该地址是否是已登记的系统调用返回点（二分查找）。
bool is_syscall_return(ADDRINT address) {
  UINT32 low = 0;
  UINT32 high = g_syscall_return_count;
  while (low < high) {
    const UINT32 middle = low + (high - low) / 2;
    if (g_syscall_returns[middle] < address) {
      low = middle + 1;
    } else {
      high = middle;
    }
  }
  return low < g_syscall_return_count && g_syscall_returns[low] == address;
}

/// 入口点命中：这是**真实事件**（不是模拟数据），它会经过
/// 转发层 → agent → 事件队列 → 轨迹文件 → Hub 侧加载器。
///
/// 参数与插桩点的 `IARG_` 顺序**必须逐一对齐**（见下面的 `INS_InsertCall`）。
///
/// 寄存器写在**文件级**的"最近一次快照"里，而不是直接写 `ThreadStats`：
/// `ThreadStats` 的定义在文件下面（线程槽位那一片），在这里它还是**不完整类型**
/// （只能声明指针，不能解引用成员）。硬要写它会得到 `C2027: 使用了未定义类型`。
void OnEntryPointHit(THREADID thread_id, ADDRINT address, ADDRINT rsp, ADDRINT rax,
                     ADDRINT rbx, ADDRINT rcx, ADDRINT rdx, ADDRINT rsi, ADDRINT rdi) {
  if (g_entry_hit) {
    return;
  }
  g_entry_hit = true;
  // 寄存器快照，顺序与 `pinbridge_trace::REGISTER_NAMES` 一致：
  // rip 在前（由 `IARG_INST_PTR` 取；`IARG_REG_VALUE` 不支持指令指针寄存器），
  // 其余七个由 `IARG_REG_VALUE` 取。
  //
  // **顺序错了不会报错，只会读到错位的寄存器**——上一轮就是这么踩的：插桩点
  // 已经按新顺序传了 9 个参数，回调签名还是旧的，于是 `address` 收到的是栈指针，
  // 「入口点命中 0x…」打出来的是一个栈地址（编译通过、运行不崩）。
  // 现在由两处钉住它：agent 侧的快照往返用例，以及这条端到端用例里
  // 「RIP 必须落在本进程主模块内」的断言。
  g_last_registers[0] = static_cast<UINT64>(address);
  g_last_registers[1] = static_cast<UINT64>(rsp);
  g_last_registers[2] = static_cast<UINT64>(rax);
  g_last_registers[3] = static_cast<UINT64>(rbx);
  g_last_registers[4] = static_cast<UINT64>(rcx);
  g_last_registers[5] = static_cast<UINT64>(rdx);
  g_last_registers[6] = static_cast<UINT64>(rsi);
  g_last_registers[7] = static_cast<UINT64>(rdi);
  g_last_registers_thread = static_cast<UINT32>(thread_id);
  // 顺序不能反：先写数组与线程号，最后才置"有效"——冲刷线程可能在任何一刻读它。
  g_last_registers_valid = true;
  PbEventRaw raw;
  std::memset(&raw, 0, sizeof(raw));
  raw.sequence = static_cast<UINT64>(g_api.allocate_sequence());
  raw.kind = 1;  // PB_EV_BREAKPOINT_HIT
  raw.thread_id = static_cast<UINT32>(thread_id);
  raw.address = address;
  raw.args[0] = address;
  const INT32 code = g_api.push_event(&raw);
  report("入口点命中 0x%llX（序号 %llu，线程 %u）→ push_event 返回 %d%s%s",
         static_cast<unsigned long long>(address),
         static_cast<unsigned long long>(raw.sequence), thread_id, code,
         code == PB_OK ? "" : " 错误: ", code == PB_OK ? "" : last_error_text().c_str());
}

/// 取模块名（不含路径、不看大小写）。
std::string module_name_of(const std::string& path) {
  const std::size_t slash = path.find_last_of("\\/");
  std::string name = slash == std::string::npos ? path : path.substr(slash + 1);
  for (std::size_t i = 0; i < name.size(); ++i) {
    if (name[i] >= 'A' && name[i] <= 'Z') {
      name[i] = static_cast<char>(name[i] - 'A' + 'a');
    }
  }
  return name;
}

/// 我们自己加载的模块，必须**排除在插桩之外**。
///
/// 为什么这是正确性问题而不是优化：桥接层与 agent 是用 `LoadLibraryA` 装进
/// **目标进程**的，从 Pin 的角度看它们就是目标的一部分，于是会被一起插桩。
/// 那意味着我们自己的系统调用（锁、分配、写轨迹文件）会**从我们自己的回调里**
/// 再次触发探针——递归，而且是在最不该出问题的地方（内核返回路径上）。
///
/// 入口探针时看不出这件事（只命中一次），系统调用探针一上来就暴露：
/// 表现为 `RECEIVED_ACCESS_FAULT: Access Address = 0x8` 这类"读空指针 + 偏移"，
/// 因为崩的是我们自己的 DLL 代码，而栈上没有任何线索。
bool is_own_module(const std::string& name) {
  return name == "pinbridge_bridge.dll" || name == "pinbridge_agent.dll" ||
         name == "pinbridge.dll";
}

/// 被排除的地址区间（自己模块的映像范围）。
struct AddressRange {
  ADDRINT low;
  ADDRINT high;
};

constexpr UINT32 kMaxExcludedRanges = 8;
AddressRange g_excluded[kMaxExcludedRanges];
UINT32 g_excluded_count = 0;

bool is_excluded(ADDRINT address) {
  for (UINT32 i = 0; i < g_excluded_count; ++i) {
    if (address >= g_excluded[i].low && address <= g_excluded[i].high) {
      return true;
    }
  }
  return false;
}

/// 前置声明：定义在下面的"系统调用探针"一节（那里才有软停止标志与计数）。
/// `OnImageLoad` 是**安全上下文**，所以它要负责把回调里攒下的摘插装请求执行掉。
void apply_pending_hard_stop();

/// 主模块的映像范围（内存探针默认只在这个范围内插桩）。
///
/// 放在这里而不是内存探针那一节：`OnImageLoad`（在前面）要填它，
/// 而 C++ 要求先声明后使用——踩过一次"未声明的标识符"，记下来。
ADDRINT g_main_image_low = 0;
ADDRINT g_main_image_high = 0;

/// 把十六进制写进缓冲区。
///
/// 自己写而不是用 `snprintf`：Pin 自带的 CRT 不提供 `std::snprintf`
/// （实测报"不是 std 的成员"），和 `std::strtoull` 是同一类问题。
/// 这里只需要一个固定宽度的十六进制，不值得去和它的头文件较劲。
void format_hex_into(char* buffer, UINT32 capacity, UINT64 value) {
  static const char digits[] = "0123456789ABCDEF";
  if (capacity == 0) {
    return;
  }
  char tmp[16];
  UINT32 count = 0;
  if (value == 0) {
    tmp[count++] = '0';
  }
  while (value != 0 && count < sizeof(tmp)) {
    tmp[count++] = digits[value & 0xF];
    value >>= 4;
  }
  UINT32 written = 0;
  buffer[written++] = '0';
  buffer[written++] = 'x';
  while (count > 0 && written + 1 < capacity) {
    buffer[written++] = tmp[--count];
  }
  buffer[written] = 0;
}

// ── 指令模板（字节签名）匹配 ────────────────────────────────────────────
//
// 为什么需要它：现实里的目标常常**没有符号**（发布版被 strip、被混淆、
// 逻辑内联到只剩一堆立即数）。用"一段字节 + 通配"定位函数，比依赖符号名
// 更接近实际可用的手段。命中的位置会被插桩，执行到就打一条 `pattern_hit`。
//
// 模式在命令行里给，不读文件：格式 `id:name:hex`（hex 两位一组，`?` 通配），
// 多条用 `;` 分隔：
//     -pb_pattern "1:demo:5042524944474531;2:prologue:48 89 5C 24 ??"
// 不读文件是为了避开"工具自带的 CRT 不一定有 fopen"这类坑——本项目已经踩过
// `std::strtoull` 与 `std::snprintf` 各一次。
struct Pattern {
  UINT32 id;
  char name[32];
  UINT8 bytes[32];
  UINT8 mask[32];  // 1 = 必须匹配，0 = 通配
  UINT32 length;
  UINT64 matches;  // 在这个映像里找到的地址数
};

constexpr UINT32 kMaxPatterns = 8;
constexpr UINT32 kMaxPatternBytes = 32;
constexpr UINT32 kMaxMatchSites = 512;

Pattern g_patterns[kMaxPatterns];
UINT32 g_pattern_count = 0;
UINT64 g_pattern_max_per_pattern = 64;
UINT64 g_pattern_hits = 0;
UINT64 g_pattern_sites = 0;
UINT64 g_pattern_limit_skipped = 0;
volatile bool g_pattern_enabled = false;

/// 命中地址表（按地址升序，供插桩期二分查找）。
struct MatchSite {
  ADDRINT address;
  UINT32 pattern_id;
};
MatchSite g_match_sites[kMaxMatchSites];
UINT32 g_match_count = 0;

void record_match(ADDRINT address, UINT32 pattern_id) {
  if (g_match_count >= kMaxMatchSites) {
    ++g_pattern_limit_skipped;
    return;
  }
  // 插入排序：命中数被上限压着（最多 512），代价可忽略，
  // 换来插桩期能二分查找而不是每处理一条指令就线性扫一遍。
  UINT32 position = g_match_count;
  while (position > 0 && g_match_sites[position - 1].address > address) {
    g_match_sites[position] = g_match_sites[position - 1];
    --position;
  }
  g_match_sites[position].address = address;
  g_match_sites[position].pattern_id = pattern_id;
  ++g_match_count;
  ++g_pattern_sites;
}

/// 该地址是否落在模板命中点**所在指令**的范围内。返回命中的模板 id，没有则 0。
///
/// 为什么按范围而不是按起始地址比对：字节模式匹配到的是**字节**，
/// 它常常落在指令中间——例如 `movabs rax, imm64`（`48 B8 <8 字节>`）里
/// 立即数的偏移是 +2。第一版按"指令起始地址 == 命中地址"比对，
/// 结果是扫描找到 1 个命中点、命中次数却是 0（实测踩过，写在这里省得再踩）。
UINT32 pattern_id_in_range(ADDRINT instruction, UINT32 size) {
  const ADDRINT end = instruction + size;
  UINT32 low = 0;
  UINT32 high = g_match_count;
  while (low < high) {
    const UINT32 middle = low + (high - low) / 2;
    if (g_match_sites[middle].address < instruction) {
      low = middle + 1;
    } else {
      high = middle;
    }
  }
  if (low < g_match_count && g_match_sites[low].address < end) {
    return g_match_sites[low].pattern_id;
  }
  return 0;
}

bool pattern_matches_at(const UINT8* data, const Pattern& pattern) {
  for (UINT32 i = 0; i < pattern.length; ++i) {
    if (pattern.mask[i] == 1 && data[i] != pattern.bytes[i]) {
      return false;
    }
  }
  return true;
}

/// 扫描映像的**可执行段**，登记命中点。
///
/// 必须在 `IMG_AddInstrumentFunction` 的回调里做完：Pin 明确说明
/// `SEC_Data()` 返回的指针在回调返回之后就失效。
void scan_image_for_patterns(IMG img) {
  if (!g_pattern_enabled || g_pattern_count == 0) {
    return;
  }
  for (UINT32 p = 0; p < g_pattern_count; ++p) {
    g_patterns[p].matches = 0;
  }
  for (SEC sec = IMG_SecHead(img); SEC_Valid(sec); sec = SEC_Next(sec)) {
    if (!SEC_IsExecutable(sec)) {
      continue;
    }
    const UINT8* data = static_cast<const UINT8*>(SEC_Data(sec));
    const USIZE size = SEC_Size(sec);
    const ADDRINT base = SEC_Address(sec);
    if (data == nullptr || size == 0) {
      continue;
    }
    for (UINT32 p = 0; p < g_pattern_count; ++p) {
      const Pattern& pattern = g_patterns[p];
      if (pattern.length == 0 || pattern.length > size) {
        continue;
      }
      for (USIZE offset = 0; offset + pattern.length <= size; ++offset) {
        if (!pattern_matches_at(data + offset, pattern)) {
          continue;
        }
        record_match(base + offset, pattern.id);
        if (++g_patterns[p].matches >= g_pattern_max_per_pattern) {
          // 上限截断**必须说出来**：不然"模板只命中这么几处"会被当成事实。
          report("模板 %u（%s）在 %s 命中数达到上限 %llu，后续地址未登记", pattern.id,
                 pattern.name, SEC_Name(sec).c_str(),
                 static_cast<unsigned long long>(g_pattern_max_per_pattern));
          ++g_pattern_limit_skipped;
          break;
        }
      }
    }
  }
  report("模板扫描完毕：登记命中点 %u 个（被上限截断 %llu 个）", g_match_count,
         static_cast<unsigned long long>(g_pattern_limit_skipped));
}

/// 解析 `id:name:hex`。返回 hex 里的字节数（0 表示失败）。
UINT32 parse_hex_bytes(const char* text, UINT8* bytes, UINT8* mask, UINT32 capacity) {
  const char* cursor = text;
  UINT32 index = 0;
  while (*cursor != 0) {
    if (*cursor == ' ' || *cursor == '-' || *cursor == ',') {
      ++cursor;
      continue;
    }
    if (index >= capacity) {
      return 0;  // 超出容量：明确失败，不截断
    }
    if (*cursor == '?') {
      bytes[index] = 0;
      mask[index] = 0;
      ++index;
      while (*cursor == '?') {
        ++cursor;
      }
      continue;
    }
    if (*(cursor + 1) == 0) {
      return 0;  // 半个字节
    }
    UINT32 value = 0;
    for (UINT32 i = 0; i < 2; ++i) {
      const char c = *(cursor + i);
      UINT32 digit = 0;
      if (c >= '0' && c <= '9') {
        digit = static_cast<UINT32>(c - '0');
      } else if (c >= 'a' && c <= 'f') {
        digit = static_cast<UINT32>(c - 'a' + 10);
      } else if (c >= 'A' && c <= 'F') {
        digit = static_cast<UINT32>(c - 'A' + 10);
      } else {
        return 0;
      }
      value = value * 16 + digit;
    }
    bytes[index] = static_cast<UINT8>(value);
    mask[index] = 1;
    ++index;
    cursor += 2;
  }
  return index;
}

bool parse_pattern_spec(const char* spec, Pattern* out) {
  const char* cursor = spec;
  UINT32 id = 0;
  bool saw_digit = false;
  while (*cursor >= '0' && *cursor <= '9') {
    id = id * 10 + static_cast<UINT32>(*cursor - '0');
    saw_digit = true;
    ++cursor;
  }
  if (!saw_digit || *cursor != ':') {
    return false;
  }
  ++cursor;  // 跳过第一个冒号

  UINT32 name_length = 0;
  while (*cursor != 0 && *cursor != ':' && name_length + 1 < sizeof(out->name)) {
    out->name[name_length++] = *cursor++;
  }
  out->name[name_length] = 0;
  if (*cursor != ':') {
    return false;
  }
  ++cursor;

  const UINT32 length = parse_hex_bytes(cursor, out->bytes, out->mask, kMaxPatternBytes);
  if (length == 0) {
    return false;
  }
  out->id = id;
  out->length = length;
  return true;
}

/// 解析整串（`;` 分隔）。任何一条解析失败就**整串失败**——不静默跳过，
/// 否则"某个模板没生效"会被当成"那个模板没命中"。
bool parse_patterns(const char* text) {
  if (text == nullptr || *text == 0) {
    return true;
  }
  const char* start = text;
  while (*start != 0 && g_pattern_count < kMaxPatterns) {
    const char* end = start;
    while (*end != 0 && *end != ';') {
      ++end;
    }
    char buffer[128];
    UINT32 length = 0;
    for (const char* cursor = start; cursor < end && length + 1 < sizeof(buffer); ++cursor) {
      buffer[length++] = *cursor;
    }
    buffer[length] = 0;
    if (length > 0) {
      if (!parse_pattern_spec(buffer, &g_patterns[g_pattern_count])) {
        report("无法解析模板 \"%s\"（期望 id:name:hex，例如 1:demo:48 89 5C 24 ??）", buffer);
        return false;
      }
      ++g_pattern_count;
    }
    start = (*end == ';') ? end + 1 : end;
  }
  return true;
}

void OnImageLoad(IMG img, VOID* v) {
  (void)v;

  // ── 模板扫描（在回调内部做完：SEC_Data 的指针出了回调就失效）──
  // 只扫主模块：模板是要找"我们自己关心的那段代码"，扫全进程会把
  // 系统 DLL 里的巧合匹配也拉进来。
  if (g_pattern_enabled && IMG_IsMainExecutable(img)) {
    scan_image_for_patterns(img);
  }
  const std::string name = module_name_of(IMG_Name(img));

  // ── 排除自己 ──
  if (is_own_module(name)) {
    if (g_excluded_count < kMaxExcludedRanges) {
      g_excluded[g_excluded_count].low = IMG_LowAddress(img);
      g_excluded[g_excluded_count].high = IMG_HighAddress(img);
      ++g_excluded_count;
    }
    // 注意：`IMG_HighAddress()` 给的是映像最后一个字节的地址，用它做上界是
    // **闭区间**判断；半开区间写法会漏掉恰好落在最后一个字节上的指令。
    report("排除自身模块 %s（0x%llX-0x%llX）——不得插桩我们自己的代码",
           name.c_str(), static_cast<unsigned long long>(IMG_LowAddress(img)),
           static_cast<unsigned long long>(IMG_HighAddress(img)));
    return;
  }

  // 安全上下文：把回调里攒下的"摘插装"请求真正执行掉
  apply_pending_hard_stop();

  if (!IMG_IsMainExecutable(img)) {
    return;
  }
  // `IMG_Entry()` 已被 Pin 标记为 DEPRECATED（在 PIE/ASLR 场景下返回值是错的），
  // 必须用 `IMG_EntryAddress()`。这两个名字很像，别写错。
  const ADDRINT entry = IMG_EntryAddress(img);
  if (entry == 0) {
    report("主模块 %s 没有入口点信息（或许是托管/加密镜像）", IMG_Name(img).c_str());
    return;
  }
  g_entry_address = entry;
  g_main_image_low = IMG_LowAddress(img);
  g_main_image_high = IMG_HighAddress(img);
  report("主模块 %s 范围 0x%llX-0x%llX，入口点 0x%llX，已布下入口探针",
         IMG_Name(img).c_str(), static_cast<unsigned long long>(g_main_image_low),
         static_cast<unsigned long long>(g_main_image_high),
         static_cast<unsigned long long>(entry));

  // ── 关于「退出信号」的现状（别凭直觉再试一遍）────────────────────────
  // 我们想要的是"main 返回 → 立刻停探针"。两条路都试过，都被 Pin 挡回来了：
  //   * `RTN_InsertCall(..., IPOINT_AFTER, ...)`
  //   * `RTN_FindByAddress(entry)` / `RTN_IsSafeForProbedInsertion`
  // 都报 "This functionality is usable only via PIN_StartProgramProbed()"。
  // 所以我们**不假装**有这个信号，退出期改由两件事兜住：
  //   1. `IMG_AddUnloadFunction`：自己模块/主模块开始卸载时停探针；
  //   2. **桥接层自身不再依赖会随卸载消失的东西**——那个
  //      `Access Address = 0x8` 的崩溃就出在它的 `thread_local` 上
  //      （静态 CRT 的线程私有数据在卸载时被释放），已改成普通静态量。
  // 将来若真要更早的信号，正确做法是换 `PIN_StartProgramProbed()` 并评估代价。
}

// ══════════════════════════════════════════════════════════════ 系统调用探针
//
// ── 为什么这个探针比入口探针危险得多 ──────────────────────────────────────
// 入口探针一辈子只命中一次。系统调用探针会命中**每一万次**，于是两个在入口
// 探针上根本不成立的问题冒出来了：
//
// 1. **重入**：回调里要调用进 agent；agent 内部可能自己发起系统调用
//    （分配、加锁、写轨迹文件）。那会再次触发本探针 → 递归。
//    解法：每个线程一个"正在回调中"的标志，标志置位时**直接跳过**。
// 2. **开销**：逐条过语言边界代价不小。这里先做**计数与上限**，
//    真正的批量缓冲留到有实测数据之后再加——现在优化等于是猜。
//
// ── 计数为什么必须报出来 ────────────────────────────────────────────────
// "丢了多少"与"看到多少"必须都能回答（项目规则：不允许静默降级）。
// 所以这里既报捕获数，也报**重入跳过数**与**推事件失败数**。

// ── 事件环形缓冲：回调只入队，由内部线程冲刷 ────────────────────────────
//
// 这是**实测逼出来的架构**，不是优化。上一版在回调里直接调用桥接层，
// 稳定崩溃（`RECEIVED_ACCESS_FAULT, Access Address = 0x8`，故障 IP 落在
// 桥接层映像的静态 CRT 区域内）；把那段改成"零外部调用"之后 3/3 全部正常，
// 恢复调用之后 2/2 崩溃。所以约束是硬的：
//
//   探针回调 = 一次比较 + 一次内存拷贝，**绝不调用外部代码**。
//   跨边界推事件交给 Pin 内部线程（非回调上下文、不被插桩）。
//
// 每个线程一个环：生产者只有本线程（无锁写 head），消费者只有冲刷线程
// （无锁写 tail）。溢出时丢**最新**一条并计数——在回调里做任何"补救"
// （写日志、分配、调用 DLL）都会把崩溃请回来。
struct EventEntry {
  UINT8 kind;
  UINT32 thread_id;
  UINT64 address;
  UINT64 args[8];
};

/// 每线程环的容量。
///
/// 实测数据决定这个值：启动阶段一次系统调用风暴就能在 10 ms 内产生 400+ 条事件，
/// 而 128 条的环会丢掉 39%（**会报出来**，但报出来不等于可以接受）。
/// 1024 条 × 64 字节 = 64 KiB/线程，64 个线程槽位 ≈ 4 MiB 静态内存——
/// 用内存换"不丢"，对采集工具是划算的。
// 2048 条：实测一个 2000 次的写循环会在一瞬间产生上千条事件，
// 1024 条的环在这种突发下会丢。槽位数相应减半，总量仍然可控
// （2048 × 80 字节 × 32 ≈ 5 MiB 静态内存）。
constexpr UINT32 kRingCapacity = 2048;

struct EventRing {
  EventEntry slots[kRingCapacity];
  volatile UINT32 head;     // 生产者推进
  volatile UINT32 tail;     // 消费者推进
  volatile UINT64 dropped;  // 溢出丢弃（必须报出来）
};

struct ThreadStats {
  UINT64 enters;
  UINT64 exits;
  UINT64 reentrant_skips;
  UINT64 push_failures;
  bool in_hook;
  /// 本线程的事件暂存环。回调只写这里（见 push_probe_event）。
  EventRing ring;
  /// 本线程**最近一次**探针命中时的寄存器快照，顺序与
  /// `pinbridge_trace::REGISTER_NAMES` 一致（rip/rsp/rax/rbx/rcx/rdx/rsi/rdi）。
  ///
  /// 为什么是"最近一次"而不是逐事件保存：这是**每线程**的字段，回调直接写
  /// （不分配、不加锁），冲刷线程读。语义与 agent 侧的存储完全一致。
  /// 全零表示"还没有过有效快照"——交付前会判 `registers[0] != 0`。
  UINT64 registers[8];
  /// 内存探针的计数。
  UINT64 mem_writes;
  UINT64 mem_reads;
  /// 模板命中计数。
  UINT64 pattern_hits;
  /// 最近一次系统调用进入时的调用号（返回点已经看不到它了）。
  UINT64 last_syscall_ssn;
  /// 是否有一次"已进入、尚未返回"的系统调用。
  ///
  /// 需要它是因为**返回事件可能没有配对的进入**：ntdll 的桩里
  /// `int 2Eh` 回退路径与 `syscall` 路径**共用同一个 `ret`**，
  /// 那条 `ret` 两条路径都会经过。此时拿上一次的调用号来填 args[0]
  /// 就是伪造数据——所以宁可记 0（未知）并计数。
  bool syscall_pending;
  /// 目标的虚拟 TF 状态（TF 虚拟化用）。
  bool virtual_tf;
  /// 写入的两段式实现用的待处理槽（BEFORE 记地址，AFTER 读值）。
  /// 每线程一份，同一线程内两条回调严格相邻，不需要任何同步。
  bool pending_write_valid;
  UINT64 pending_write_address;
  UINT32 pending_write_size;
  UINT64 pending_write_instruction;
};

/// 线程统计表。用固定槽位而不是每条线程都 new：探针回调在**任意线程**上
/// 运行，那里不能做可能失败/可能阻塞的分配（分配本身可能又触发系统调用）。
///
/// 槽位溢出时不假装成功：`g_slot_overflow` 会 +1 并在收尾时报出来。
// 线程槽位：每个槽位带一个 64 KiB 的环，所以不能随便放大（见 kRingCapacity）。
constexpr UINT32 kMaxThreadSlots = 32;
ThreadStats g_thread_slots[kMaxThreadSlots];
ThreadStats g_overflow_slot;   // 槽位用尽后的共享槽（只记不细分）
volatile long g_slot_overflow = 0;
volatile long g_syscall_captured = 0;
/// 已成功跨边界送出的事件数（由冲刷线程累加）。
volatile UINT64 g_flushed_total = 0;
/// 跨边界推事件失败的次数（必须报出来，不允许静默丢弃）。
volatile UINT64 g_push_failures = 0;
/// 成功交给 agent 的寄存器快照次数。
volatile UINT64 g_register_deliveries = 0;
/// 交寄存器快照失败的次数（旧版转发层没有该符号时不算失败）。
volatile UINT64 g_register_failures = 0;
/// 因为"这份快照无效"（全零）而被跳过的次数。
volatile UINT64 g_register_skipped = 0;
bool g_syscall_instrumentation_removed = false;

/// 达到上限后的**软停止**标志。
///
/// 为什么不在回调里直接 `PIN_RemoveInstrumentation()`：那是在**回调上下文**
/// 里改插装表，而回调可能正跑在一条系统调用指令上（内核返回路径）。
/// 更稳的做法是"回调只置位并立刻返回"（一次比较，代价可忽略），
/// 真正的摘除留给 `OnImageLoad`/`OnFini` 这种正常上下文。
volatile bool g_syscall_soft_stopped = false;
volatile bool g_syscall_hard_stop_pending = false;

/// 目标进入退出流程。
///
/// 这是**实测**逼出来的一个标志，不是防御性编程：目标在退出时会拆掉自己
/// 加载的 DLL（包括我们的桥接层与 agent），静态对象随之销毁。此时内核仍在
/// 执行系统调用（`NtTerminateProcess` 等），探针照旧命中——于是我们会在
/// "已经不存在"的模块里调用函数。
///
/// 症状极具误导性：Pin 报 `RECEIVED_ACCESS_FAULT, Access Address = 0x8`，
/// 崩溃 IP 落在桥接层映像里，而栈上没有任何线索；而且它只在**系统调用探针**
/// 下出现（入口探针一辈子只命中一次，且在早期）。
///
/// 所以：一旦进入退出流程，回调只返回，不再触碰任何外部代码。
volatile bool g_shutdown = false;

void request_syscall_stop() {
  if (!g_syscall_soft_stopped) {
    g_syscall_soft_stopped = true;
    g_syscall_hard_stop_pending = true;
  }
}

/// 在**安全上下文**里真正摘掉插装。
void apply_pending_hard_stop() {
  if (g_syscall_hard_stop_pending) {
    g_syscall_hard_stop_pending = false;
    PIN_RemoveInstrumentation();
    report("停止系统调用插装（达到 -pb_syscall_max 上限），已捕获 %ld 条",
           static_cast<long>(g_syscall_captured));
  }
}

/// 取本线程的统计槽。**不做分配**：第一次用到某个 tid 时零初始化即可。
ThreadStats* stats_for(THREADID tid) {
  const UINT32 index = static_cast<UINT32>(tid);
  if (index < kMaxThreadSlots) {
    return &g_thread_slots[index];
  }
  // 溢出只报一次（不然日志会被刷爆）
  if (g_slot_overflow == 0) {
    ++g_slot_overflow;
  }
  return &g_overflow_slot;
}


/// 推一条事件的统一出口：计数、失败可见。
///
/// ── 已知缺陷：这条路径在**高事件量**下会崩溃 ────────────────────────────
/// 实测（`--syscalls 300`）稳定崩：Pin 报
/// `RECEIVED_ACCESS_FAULT, Access Address = 0x8`，故障 IP 落在桥接层映像里。
/// 定位结论（可复现）：把本函数改成"零外部调用"之后 3/3 全部正常，
/// 恢复调用我们的 DLL 之后 2/2 崩溃——**问题出在"在系统调用回调上下文里
/// 调用外部 DLL"，不是事件量本身**。低事件量（1/50 次系统调用）时不崩。
///
/// 正因如此，系统调用探针**默认关闭**（`-pb_syscalls` 默认 0），入口探针不受影响。
/// 正确修法已经明确：回调只做内存入队 + 由 Pin 内部线程冲刷（见
/// docs/roadmap.md 的 M1 与 native/README.md 的"已知缺陷"一节）。
/// 推一条事件：**只入队**。回调路径上除了下标的算术与内存拷贝，什么都不做。
void push_probe_event(ThreadStats* stats, UINT8 kind, THREADID tid, ADDRINT address,
                      const UINT64* args, UINT32 arg_count) {
  EventRing* ring = &stats->ring;
  if (static_cast<UINT32>(ring->head - ring->tail) >= kRingCapacity) {
    ++ring->dropped;
    return;
  }
  EventEntry& slot = ring->slots[ring->head % kRingCapacity];
  slot.kind = kind;
  slot.thread_id = static_cast<UINT32>(tid);
  slot.address = static_cast<UINT64>(address);
  for (UINT32 i = 0; i < 8; ++i) {
    slot.args[i] = i < arg_count ? args[i] : 0;
  }
  ring->head = ring->head + 1;
}

/// 冲刷一个环。**只在内部线程或 Fini 里调用**（这两处可以安全跨边界）。
///
/// `stats` 是本环所属线程的统计块（寄存器快照就在里面）。溢出槽也走同一条路，
/// 但它的 `registers` 永远是全零——见下面的交付判定。
UINT64 flush_ring(EventRing* ring, const ThreadStats* stats) {
  UINT64 flushed = 0;
  while (ring->tail != ring->head) {
    const EventEntry& slot = ring->slots[ring->tail % kRingCapacity];
    PbEventRaw raw;
    std::memset(&raw, 0, sizeof(raw));
    // 序号**在这里**分配：只有冲刷路径会调 `pb_allocate_sequence`，
    // 因此序号全局单调。代价是顺序 = 冲刷顺序而不是发生顺序（10 ms 粒度），
    // 这点写在文档里，不假装是精确时序。
    raw.sequence = static_cast<UINT64>(g_api.allocate_sequence());
    raw.kind = slot.kind;
    raw.thread_id = slot.thread_id;
    raw.address = slot.address;
    for (UINT32 i = 0; i < 8; ++i) {
      raw.args[i] = slot.args[i];
    }
    if (g_api.push_event(&raw) != PB_OK) {
      // 失败必须计数（不允许静默丢弃）。这里在内部线程上下文，
      // 报日志是安全的，但为了不刷屏只累加，收尾统一报。
      ++g_push_failures;
    }
    // 寄存器快照：交的是**该线程最近一次**命中的快照（回调直接写进 ThreadStats）。
    //
    // 只在 `registers[0] != 0` 时交付——**全零不是快照，是"没有"**：
    // agent 侧是"每线程最近一次"的**覆盖式**存储，把零值交上去会把同一线程
    // 先前那份好快照冲掉。实测过一次：工具报"交付 3419 次"，agent 里 5 条线程
    // 全有快照，却全是 0。"有快照"与"快照有用"是两件事。
    //
    // RIP（顺序里的第一个）为 0 在真实执行流里不可能出现，拿它当有效判据。
    //
    // 快照来源：优先本线程 `stats->registers`；为空时退到文件级的"最近一次"，
    // 但**必须线程号对得上**——对不上就是别条线程的快照，张冠李戴比没有更糟。
    const UINT64* snapshot = nullptr;
    if (stats->registers[0] != 0) {
      snapshot = stats->registers;
    } else if (g_last_registers_valid && g_last_registers_thread == slot.thread_id &&
               g_last_registers[0] != 0) {
      snapshot = g_last_registers;
    }
    if (g_api.set_thread_registers != nullptr && snapshot != nullptr) {
      if (g_api.set_thread_registers(slot.thread_id, snapshot, 8) != PB_OK) {
        // 失败也计数：静默丢弃会让调用方以为"从来没命中过"。
        ++g_register_failures;
      } else {
        ++g_register_deliveries;
      }
    } else if (g_api.set_thread_registers != nullptr) {
      ++g_register_skipped;
    }
    ring->tail = ring->tail + 1;
    ++flushed;
  }
  return flushed;
}

/// 冲刷所有线程的环。返回本次送出的事件数。
UINT64 flush_all_rings() {
  UINT64 flushed = 0;
  for (UINT32 i = 0; i < kMaxThreadSlots; ++i) {
    flushed += flush_ring(&g_thread_slots[i].ring, &g_thread_slots[i]);
  }
  flushed += flush_ring(&g_overflow_slot.ring, &g_overflow_slot);
  return flushed;
}

/// 冲刷线程主体（Pin 内部线程）。
///
/// 用 10 ms 轮询而不是事件驱动：目标是"低开销采集"，不是"实时"。
/// 环满时生产者会丢弃并计数（丢最新），因此延迟不会变成丢失的借口。
VOID FlusherMain(VOID* arg) {
  (void)arg;
  while (!g_shutdown) {
    const UINT64 flushed = flush_all_rings();
    g_flushed_total += flushed;
    // **有活就不睡**：10 ms 固定间隔在访存这种密集探针下会成为瓶颈——
    // 实测内存写入探针 1 秒内就能把环灌满两次，而固定的 10 ms 间隔意味着
    // 大部分时间花在睡眠上。改成"送完了才睡 1 ms"：
    // 空闲时几乎不占 CPU（1 ms 轮询），繁忙时尽快把环清空。
    if (flushed == 0) {
      ::Sleep(1);
    }
  }
  // 退出前把剩下的送走：Fini 也会再冲刷一次，这里只是尽早。
  g_flushed_total += flush_all_rings();
  PIN_ExitThread(0);
}

/// `syscall` 指令：x64 的调用约定是 RAX=SSN，前四个参数在 R10/RDX/R8/R9
/// （注意第一个参数是 **R10** 而不是 RCX——RCX 被 syscall 指令自己用来存返回地址）。
void OnSyscallEnter(THREADID tid, ADDRINT address, ADDRINT ssn, ADDRINT arg0,
                    ADDRINT arg1, ADDRINT arg2, ADDRINT arg3) {
  if (g_shutdown || g_syscall_soft_stopped) {
    return;
  }
  ThreadStats* stats = stats_for(tid);
  if (stats->in_hook) {
    // 我们自己正在回调里（agent 内部的系统调用又走到这里）——必须跳过，
    // 否则递归。跳过要计数：这是"看到的事件变少"的一个正当理由。
    ++stats->reentrant_skips;
    return;
  }
  stats->in_hook = true;
  ++stats->enters;
  ++g_syscall_captured;
  // 记住调用号：返回点已经看不到它了（那时 RAX 是 NTSTATUS）。
  // 事件模型里 `syscall_exit` 的 args[0] 是**调用号**、args[1] 是返回值，
  // 所以这个记忆不是可选优化，而是让事件符合约定的一部分。
  stats->last_syscall_ssn = static_cast<UINT64>(ssn);
  stats->syscall_pending = true;
  const UINT64 args[5] = {static_cast<UINT64>(ssn), static_cast<UINT64>(arg0),
                          static_cast<UINT64>(arg1), static_cast<UINT64>(arg2),
                          static_cast<UINT64>(arg3)};
  push_probe_event(stats, 3 /* PB_EV_SYSCALL_ENTER */, tid, address, args, 5);
  stats->in_hook = false;

  // 软停止：只置位（一次比较），摘插装留给安全上下文。
  if (g_syscall_cap != 0 && static_cast<UINT64>(g_syscall_captured) >= g_syscall_cap) {
    request_syscall_stop();
  }
}

/// 系统调用的**返回点**：`syscall` 之后的那条指令。
///
/// ## 为什么不是 `sysret`
///
/// 第一版挂在 `INS_IsSysret` 上，实测在当前 Windows 上**一次都没命中**
/// （收尾统计里 `exit=0`）。原因不是 Pin 的问题：内核返回用户态用的 `sysret`
/// 是**内核态指令**，Pin 插装的是用户态代码。用户态能看到的返回路径就是
/// ntdll 系统调用桩里 `syscall` 指令的**下一条指令**（通常是 `ret`）。
///
/// 所以这里在插桩期取 `INS_Next(syscall_ins)`，把回调挂在那个点上：
/// 此时内核已经返回、RAX 里是 NTSTATUS。
void OnSyscallExit(THREADID tid, ADDRINT address, ADDRINT status) {
  if (g_shutdown || g_syscall_soft_stopped) {
    return;
  }
  ThreadStats* stats = stats_for(tid);
  if (stats->in_hook) {
    ++stats->reentrant_skips;
    return;
  }
  stats->in_hook = true;
  ++stats->exits;
  ++g_syscall_captured;
  // 顺序与 `syscall_exit` 的约定一致：args[0] = 调用号，args[1] = 返回值。
  UINT64 ssn = 0;
  if (stats->syscall_pending) {
    ssn = stats->last_syscall_ssn;
    stats->syscall_pending = false;
  } else {
    // 没有配对的进入：调用号**未知**。记 0 并计数，而不是拿上一次的号冒充——
    // 轨迹里出现一个错的调用号比出现一个 0 危险得多。
    ++g_syscall_exit_without_enter;
  }
  const UINT64 args[2] = {ssn, static_cast<UINT64>(status)};
  push_probe_event(stats, 4 /* PB_EV_SYSCALL_EXIT */, tid, address, args, 2);
  stats->in_hook = false;
  if (g_syscall_cap != 0 && static_cast<UINT64>(g_syscall_captured) >= g_syscall_cap) {
    request_syscall_stop();
  }
}

/// 映像卸载回调：**这是我们能拿到的最早的退出信号**。
///
/// 为什么需要它：目标的退出流程是
/// `ExitProcess → 各 DLL 的 DllMain(DETACH) → …`，而 Pin 的
/// `PIN_AddFiniFunction` **晚于**这个过程。系统调用探针在退出期仍会命中
/// （`NtTerminateProcess` 本身就是一次系统调用），于是回调会在"桥接层/agent
/// 已经卸载、其静态对象已析构"之后继续调用它们——实测表现为
/// `RECEIVED_ACCESS_FAULT, Access Address = 0x8`，故障 IP 固定在桥接层里。
///
/// 一旦看到**我们自己的模块**或主模块开始卸载，立刻停掉所有探针：此后回调
/// 只返回，不再触碰任何外部代码。
VOID OnImageUnload(IMG img, VOID* v) {
  (void)v;
  const std::string name = module_name_of(IMG_Name(img));
  if (is_own_module(name) || IMG_IsMainExecutable(img)) {
    if (!g_shutdown) {
      g_shutdown = true;
      report("检测到 %s 开始卸载 → 立即停止全部探针（退出期不再回调外部代码）",
             name.c_str());
    }
  }
}

// ══════════════════════════════════════════════════════════════ 内存访问探针
//
// ── 为什么过滤是**必需**的，不是优化 ────────────────────────────────────
// 几乎每条指令都会访存。不加过滤的话产出的是无法使用的噪声，而且开销会
// 掩盖被分析程序本身的行为。所以三层过滤都能收紧：
//
//   1. 模块："谁在访存"——默认只关心主模块；
//   2. 地址范围："访到哪里"——`-pb_mem_range lo-hi`（十六进制）；
//   3. 类型：读/写——默认**只写**。
//
// 第 3 条的默认值与事件模型保持一致：`EventKind::MemRead::default_subscribed()`
// 就是 false（读的体量比写大一到两个数量级）。两处各定各的，会让人先被
// "订阅了却收不到"困惑一次。
//
// ── 写入后的值怎么拿 ──────────────────────────────────────────────────
// `IARG_MEMORYWRITE_EA` 只在 `IPOINT_BEFORE` 有效，而 BEFORE 读到的还是
// **写入前**的值。所以写入事件分两段：
//   BEFORE：把 (EA, 长度, 指令地址) 记进本线程的待处理槽（纯内存写）
//   AFTER ：取出地址，用 `PIN_SafeCopy` 读**写入后**的值再入队
// 不支持 AFTER 点的写入指令**跳过并计数**——宁可少采一条，也不送出一条
// 语义错误的"新值"。
struct MemPolicy {
  bool enabled = false;
  bool reads = false;
  bool writes = true;
  bool main_image_only = true;
  UINT64 low = 0;
  UINT64 high = 0;        // low==0 && high==0 表示不限
  UINT64 max_events = 0;  // 0 表示不限
};

MemPolicy g_mem;
volatile long g_mem_sites = 0;
volatile UINT64 g_mem_captured = 0;
volatile UINT64 g_mem_skipped_no_after = 0;
volatile UINT64 g_mem_unreadable = 0;
/// 被地址范围过滤掉的条数（"事件为什么少"的可回答性靠它）。
volatile UINT64 g_mem_filtered = 0;
volatile bool g_mem_soft_stopped = false;
// `g_main_image_low/high` 的声明在前面（`OnImageLoad` 要填它，见那里的注释）。

bool mem_address_in_scope(UINT64 address) {
  if (g_mem.low == 0 && g_mem.high == 0) {
    return true;
  }
  return address >= g_mem.low && address <= g_mem.high;
}

bool mem_instruction_in_scope(ADDRINT address) {
  if (!g_mem.main_image_only) {
    return true;
  }
  return g_main_image_high > g_main_image_low && address >= g_main_image_low &&
         address <= g_main_image_high;
}

/// 写入的第一段：只记地址（`IARG_MEMORYWRITE_EA` 在 BEFORE 才有效）。
void OnMemWriteBefore(THREADID tid, ADDRINT ins_address, ADDRINT effective_address,
                      UINT32 size) {
  if (g_shutdown || g_mem_soft_stopped) {
    return;
  }
  // 地址过滤**必须在这里**做：插桩期只知道"哪条指令"，不知道它会访哪个地址。
  // 少了这一步，`-pb_mem_range` 就只是个摆设——实测过：范围外的事件照样灌进来，
  // 环形缓冲被冲爆、丢掉一半。被过滤掉的条数也要计数，否则"事件为什么这么少"
  // 同样无从回答。
  if (!mem_address_in_scope(static_cast<UINT64>(effective_address))) {
    ++g_mem_filtered;
    return;
  }
  ThreadStats* stats = stats_for(tid);
  stats->pending_write_valid = true;
  stats->pending_write_address = static_cast<UINT64>(effective_address);
  stats->pending_write_size = size;
  stats->pending_write_instruction = static_cast<UINT64>(ins_address);
}

/// 写入的第二段：此时内存里已经是**写入后**的值。
void OnMemWriteAfter(THREADID tid) {
  if (g_shutdown || g_mem_soft_stopped) {
    return;
  }
  ThreadStats* stats = stats_for(tid);
  if (!stats->pending_write_valid) {
    return;  // 第一段被软停止挡掉了
  }
  stats->pending_write_valid = false;
  if (stats->in_hook) {
    ++stats->reentrant_skips;
    return;
  }
  stats->in_hook = true;
  UINT64 value = 0;
  const UINT32 size = stats->pending_write_size;
  if (size <= 8) {
    // `PIN_SafeCopy` 是 Pin 自己的安全读：读不到就给 0 并计数，不抛异常。
    // 它**不违反**"回调不许调用外部 DLL"那条约束（那是我们自己的代码与 CRT）。
    const size_t got = PIN_SafeCopy(
        &value, reinterpret_cast<const VOID*>(static_cast<ADDRINT>(stats->pending_write_address)),
        size);
    if (got != size) {
      value = 0;
      ++g_mem_unreadable;
    }
  }
  ++stats->mem_writes;
  ++g_mem_captured;
  const UINT64 args[3] = {stats->pending_write_address, size, value};
  push_probe_event(stats, 6 /* PB_EV_MEM_WRITE */, tid,
                   static_cast<ADDRINT>(stats->pending_write_instruction), args, 3);
  stats->in_hook = false;
  if (g_mem.max_events != 0 && g_mem_captured >= g_mem.max_events) {
    g_mem_soft_stopped = true;
  }
}

/// 读取：BEFORE 就能拿到有效地址与长度，顺带把读到的值也带上。
void OnMemRead(THREADID tid, ADDRINT ins_address, ADDRINT effective_address, UINT32 size) {
  if (g_shutdown || g_mem_soft_stopped) {
    return;
  }
  if (!mem_address_in_scope(static_cast<UINT64>(effective_address))) {
    ++g_mem_filtered;
    return;
  }
  ThreadStats* stats = stats_for(tid);
  if (stats->in_hook) {
    ++stats->reentrant_skips;
    return;
  }
  stats->in_hook = true;
  UINT64 value = 0;
  if (size <= 8) {
    const size_t got = PIN_SafeCopy(
        &value, reinterpret_cast<const VOID*>(effective_address), size);
    if (got != size) {
      value = 0;
      ++g_mem_unreadable;
    }
  }
  ++stats->mem_reads;
  ++g_mem_captured;
  const UINT64 args[3] = {static_cast<UINT64>(effective_address), size, value};
  push_probe_event(stats, 7 /* PB_EV_MEM_READ */, tid, ins_address, args, 3);
  stats->in_hook = false;
  if (g_mem.max_events != 0 && g_mem_captured >= g_mem.max_events) {
    g_mem_soft_stopped = true;
  }
}

// 前置声明：定义在文件后面的"模板命中"与"TF 单步"两节
//（它们要用到那里的全局量，而 C++ 要求先声明后使用）。
void on_instruction_patterns(INS ins, ADDRINT address);
void OnPatternHit(THREADID tid, ADDRINT ins_address, ADDRINT pattern_id);
void instrument_tf(INS ins);

/// 指令级插桩：入口点 + 系统调用 + 内存访问 + 模板命中。
void OnInstruction(INS ins, VOID* v) {
  (void)v;

  const ADDRINT address = static_cast<ADDRINT>(INS_Address(ins));

  // 自己模块里的指令一律不插桩（理由见 is_own_module）。
  if (is_excluded(address)) {
    return;
  }

  // ── 入口点探针（只关心主模块入口那一条指令）──
  if (!g_entry_hit && g_entry_address != 0 && address == g_entry_address) {
    // 参数顺序与 `OnEntryPointHit` 的签名逐一对齐：tid、rip（`IARG_INST_PTR`）、
    // 然后 rsp/rax/rbx/rcx/rdx/rsi/rdi（与 `REGISTER_NAMES` 同序）。
    // 不用 `IARG_CONTEXT` + `PIN_GetContextReg`：那条路实测会 `RECEIVED_ACCESS_FAULT`。
    INS_InsertCall(ins, IPOINT_BEFORE, reinterpret_cast<AFUNPTR>(OnEntryPointHit),
                   IARG_THREAD_ID, IARG_INST_PTR, IARG_REG_VALUE, LEVEL_BASE::REG_STACK_PTR,
                   IARG_REG_VALUE, LEVEL_BASE::REG_GAX, IARG_REG_VALUE, LEVEL_BASE::REG_GBX,
                   IARG_REG_VALUE, LEVEL_BASE::REG_GCX, IARG_REG_VALUE, LEVEL_BASE::REG_GDX,
                   IARG_REG_VALUE, LEVEL_BASE::REG_GSI, IARG_REG_VALUE, LEVEL_BASE::REG_GDI,
                   IARG_END);
  }

  // ── 退出信号：入口函数的返回指令 ──
  // 注意：这里**永远不会触发**——`g_entry_rtn` 只能通过 `RTN_FindByAddress`
  // 拿到，而那个 API 需要 `PIN_StartProgramProbed()`（实测被拒）。
  // 保留它只会制造"有个机制但其实不生效"的错觉，所以直接删掉，
  // 现状写在 OnImageLoad 的注释里。

  // ── 系统调用探针 ──
  if (!pb_syscall_enabled || g_syscall_instrumentation_removed ||
      g_syscall_soft_stopped) {
    return;
  }
  // 先认返回点：它在自己所在的那个块里，与含 `syscall` 的块不是同一个。
  if (is_syscall_return(address)) {
    INS_InsertCall(ins, IPOINT_BEFORE, reinterpret_cast<AFUNPTR>(OnSyscallExit),
                   IARG_THREAD_ID, IARG_INST_PTR, IARG_REG_VALUE, LEVEL_BASE::REG_RAX,
                   IARG_END);
    ++g_syscall_exit_sites;
    ++g_syscall_returns_matched;
    // 不早退：这条指令仍可能命中别的探针（模板等）。
  }
  if (INS_IsSyscall(ins)) {
    // 只登记 SSN 与四个参数寄存器，不做任何地址计算——这些值在
    // IPOINT_BEFORE 处读，正是内核即将看到的那一份。
    INS_InsertCall(ins, IPOINT_BEFORE, reinterpret_cast<AFUNPTR>(OnSyscallEnter),
                   IARG_THREAD_ID, IARG_INST_PTR, IARG_REG_VALUE, LEVEL_BASE::REG_RAX,
                   IARG_REG_VALUE, LEVEL_BASE::REG_R10, IARG_REG_VALUE,
                   LEVEL_BASE::REG_RDX, IARG_REG_VALUE, LEVEL_BASE::REG_R8,
                   IARG_REG_VALUE, LEVEL_BASE::REG_R9, IARG_END);
    ++g_syscall_sites;

    // ── 返回点：`syscall` 之后的那条指令 ──
    // 用户态能看到的返回路径就在这里（内核用的 `sysret` 是内核态指令，
    // Pin 插装不到——这是第一版 `exit=0` 的原因，见 OnSyscallExit 的注释）。
    //
    // 不能写 `INS_Next(ins)`：`syscall` 是 Pin 里的**基本块终止指令**，
    // 它没有"块内下一条"。返回地址只能算出来，然后在插桩期按地址认出来
    // （见 is_syscall_return / record_syscall_return）。
    //
    // 为什么这个顺序能成立：含 `syscall` 的块必然先被翻译并执行，
    // 之后内核才返回到下一条指令——而那条指令所在的**新块**是那时才被翻译的，
    // 所以我们登记在先、匹配在后。少数反例（有人从别处跳进返回点）会让
    // 记录数与匹配数对不上，收尾统计里会如实显示。
    record_syscall_return(static_cast<ADDRINT>(INS_Address(ins) + INS_Size(ins)));
    return;
  }
  if (INS_IsSysret(ins)) {
    // 保底：某些实现/平台确实用 `sysret` 返回。当前 Windows 上一次都没命中，
    // 但它不该被删掉——删掉就再也不知道"什么时候会命中"了。
    INS_InsertCall(ins, IPOINT_BEFORE, reinterpret_cast<AFUNPTR>(OnSyscallExit),
                   IARG_THREAD_ID, IARG_INST_PTR, IARG_REG_VALUE,
                   LEVEL_BASE::REG_RAX, IARG_END);
    ++g_syscall_sites;
    ++g_syscall_exit_sites;
    return;
  }

  // ── TF 相关指令（popfq/iretq/pushfq）──
  instrument_tf(ins);

  // ── 模板命中（放在会 return 的分支之前）──
  on_instruction_patterns(ins, address);

  // ── 内存访问探针 ──
  // 注意这一段在系统调用判断**之后**：系统调用指令本身不访存用户内存，
  // 两者互斥，早 return 也省掉了重复判断。
  if (!g_mem.enabled || g_mem_soft_stopped || g_shutdown) {
    return;
  }
  if (!mem_instruction_in_scope(address)) {
    return;
  }
  if (g_mem.writes && INS_IsMemoryWrite(ins)) {
    if (INS_IsValidForIpointAfter(ins)) {
      // 两段式：BEFORE 记地址（EA 的 IARG 只在 BEFORE 有效），AFTER 读写入后的值。
      INS_InsertCall(ins, IPOINT_BEFORE, reinterpret_cast<AFUNPTR>(OnMemWriteBefore),
                     IARG_THREAD_ID, IARG_INST_PTR, IARG_MEMORYWRITE_EA,
                     IARG_MEMORYWRITE_SIZE, IARG_END);
      INS_InsertCall(ins, IPOINT_AFTER, reinterpret_cast<AFUNPTR>(OnMemWriteAfter),
                     IARG_THREAD_ID, IARG_END);
      ++g_mem_sites;
    } else {
      // 不能在这里改成 BEFORE 采集：那会把**写入前**的值当成"新值"送出去，
      // 而调用方无法分辨。跳过并计数，宁可少一条。
      ++g_mem_skipped_no_after;
    }
  }
  if (g_mem.reads && INS_IsMemoryRead(ins)) {
    INS_InsertCall(ins, IPOINT_BEFORE, reinterpret_cast<AFUNPTR>(OnMemRead),
                   IARG_THREAD_ID, IARG_INST_PTR, IARG_MEMORYREAD_EA,
                   IARG_MEMORYREAD_SIZE, IARG_END);
    ++g_mem_sites;
  }
}

/// 模板命中的插桩入口（每条指令都会来这里查一次）。
void on_instruction_patterns(INS ins, ADDRINT address) {
  if (!g_pattern_enabled || g_match_count == 0 || g_shutdown) {
    return;
  }
  // 按指令**范围**匹配（模式常常落在指令中间，例如 movabs 的立即数）。
  const UINT32 pattern_id = pattern_id_in_range(address, INS_Size(ins));
  if (pattern_id == 0) {
    return;
  }
  INS_InsertCall(ins, IPOINT_BEFORE, reinterpret_cast<AFUNPTR>(OnPatternHit),
                 IARG_THREAD_ID, IARG_INST_PTR, IARG_ADDRINT,
                 static_cast<ADDRINT>(pattern_id), IARG_END);
}

/// 模板命中：执行到了登记过的地址。
void OnPatternHit(THREADID tid, ADDRINT ins_address, ADDRINT pattern_id) {
  if (g_shutdown) {
    return;
  }
  ThreadStats* stats = stats_for(tid);
  if (stats->in_hook) {
    ++stats->reentrant_skips;
    return;
  }
  stats->in_hook = true;
  ++stats->pattern_hits;
  ++g_pattern_hits;
  const UINT64 args[2] = {static_cast<UINT64>(pattern_id), 0};
  push_probe_event(stats, 5 /* PB_EV_PATTERN_HIT */, tid, ins_address, args, 2);
  stats->in_hook = false;
}
// ══════════════════════════════════════════════════════════════ TF 单步虚拟化
//
// EFLAGS.TF（位 8）是反跟踪最常用的一招：置位之后每条指令都会触发
// STATUS_SINGLE_STEP 异常，调试器/工具会被淹没；反过来，如果我们要单步观察
// 又不让目标察觉，就得把"目标看到的 TF"和"真实的 TF"分开。
//
// 三种模式（`-pb_tf`）：
//   off        ：不注册任何东西，零开销。
//   watch      ：只**观察**——目标是真想单步还是想骗我们，看得见。
//   virtualize ：在 watch 基础上把真实 TF 剥掉。目标的判断依然成立
//                （它读 flags 时 TF 是置位的），但它永远不会真的被单步打断。
//
// ── 为什么改栈上的副本，而不是改上下文 ──────────────────────────────────
// `popfq` / `iretq` 会**从栈加载** flags：在 IPOINT_BEFORE 改 EFLAGS 会被这条
// 指令自己覆盖掉。改它即将加载的那份副本才有效。
//
// 读的那一侧（`pushfq`）反过来：在 BEFORE 没法改"即将压栈的值"，
// 但在 **AFTER** 改栈上刚压好的那份副本就行——此时 RSP 已经减过 8，
// `[RSP]` 就是刚压进去的 flags。
//
// ── 实测依据（决定这个设计能不能被验证）─────────────────────────────────
// MSVC 的 `__writeeflags()` 会编译成 `popfq`、`__readeflags()` 会编译成
// `pushfq`（用 dumpbin 确认过）。所以自写靶子不需要内联汇编就能走完整条路径，
// 判据可以精确到：
//   virtualize：靶子报告 real_steps=0、tf_reads=N（它确信自己在单步，其实没有）
//   不虚拟化  ：靶子报告 real_steps=N、tf_reads=0（内核在派发异常时清掉了 TF）
enum class TfMode { Off, Watch, Virtualize };

TfMode g_tf_mode = TfMode::Off;
volatile bool g_tf_enabled = false;
UINT64 g_tf_sets = 0;                // 观察到"置位 TF"的意图
UINT64 g_tf_clears = 0;              // 观察到"清除 TF"的意图
UINT64 g_tf_masked = 0;              // 实际从栈上副本里剥掉的置位
UINT64 g_tf_read_patches = 0;        // 往 pushfq 压栈副本里补回去的次数
UINT64 g_tf_steps_observed = 0;      // 真实单步异常次数
UINT64 g_tf_steps_suppressed = 0;    // virtualize 下被清掉 TF 的真实单步
UINT64 g_tf_stack_failures = 0;      // 读/写栈上副本失败
UINT64 g_tf_sites = 0;               // 插装了多少个 popfq/iretq/pushfq 点

/// `STATUS_SINGLE_STEP`（不能 include windows.h：Pin 自己的 minwindef 会冲突）。
constexpr INT32 kExceptionSingleStep = static_cast<INT32>(0x80000004);
/// EFLAGS.TF
constexpr UINT64 kTrapFlagBit = 0x100ULL;

/// `popfq` / `iretq`：flags 即将从栈上加载。
VOID OnFlagsLoadBefore(THREADID tid, ADDRINT ins_address, ADDRINT stack_pointer,
                       INT32 is_iret) {
  if (!g_tf_enabled || g_shutdown) {
    return;
  }
  // iretq 的栈布局：RIP、CS、RFLAGS、RSP、SS —— flags 在 RSP+16。
  const ADDRINT flags_address = is_iret != 0 ? stack_pointer + 16 : stack_pointer;
  UINT64 flags = 0;
  if (PIN_SafeCopy(&flags, reinterpret_cast<const VOID*>(flags_address), 8) != 8) {
    ++g_tf_stack_failures;
    return;
  }
  ThreadStats* stats = stats_for(tid);
  const bool setting = (flags & kTrapFlagBit) != 0;
  if (setting == stats->virtual_tf) {
    return;  // 与当前虚拟状态相同：没有意图变化，不报（否则会刷屏）
  }
  stats->virtual_tf = setting;
  if (setting) {
    ++g_tf_sets;
  } else {
    ++g_tf_clears;
  }
  // 上报：args[0] 的语义见 docs（1=置位意图 0=清除意图 2=真实单步异常）。
  const UINT64 args[2] = {setting ? 1ULL : 0ULL, 0};
  push_probe_event(stats, 9 /* PB_EV_TRAP_FLAG */, tid, ins_address, args, 2);

  if (g_tf_mode == TfMode::Virtualize && setting) {
    // 把真实 TF 从这份副本里剥掉：目标不会被真的单步打断。
    const UINT64 masked = flags & ~kTrapFlagBit;
    if (PIN_SafeCopy(reinterpret_cast<VOID*>(flags_address), &masked, 8) == 8) {
      ++g_tf_masked;
    } else {
      ++g_tf_stack_failures;
    }
  }
}

/// `pushfq` 之后：把虚拟 TF 补回刚压栈的那份副本，让目标"读到"它设置的 TF。
VOID OnFlagsStoreAfter(THREADID tid, ADDRINT stack_pointer) {
  if (!g_tf_enabled || g_tf_mode != TfMode::Virtualize || g_shutdown) {
    return;
  }
  ThreadStats* stats = stats_for(tid);
  if (!stats->virtual_tf) {
    return;
  }
  // AFTER 点：RSP 已经减过 8，`[RSP]` 就是刚压进去的那份 flags。
  UINT64 flags = 0;
  if (PIN_SafeCopy(&flags, reinterpret_cast<const VOID*>(stack_pointer), 8) != 8) {
    ++g_tf_stack_failures;
    return;
  }
  const UINT64 patched = flags | kTrapFlagBit;
  if (PIN_SafeCopy(reinterpret_cast<VOID*>(stack_pointer), &patched, 8) == 8) {
    ++g_tf_read_patches;
  } else {
    ++g_tf_stack_failures;
  }
}

/// 上下文变化：真实单步异常。
VOID OnContextChange(THREADID tid, CONTEXT_CHANGE_REASON reason, const CONTEXT* from,
                     CONTEXT* to, INT32 info, VOID* v) {
  (void)from;
  (void)v;
  if (!g_tf_enabled || reason != CONTEXT_CHANGE_REASON_EXCEPTION ||
      info != kExceptionSingleStep) {
    return;
  }
  ++g_tf_steps_observed;
  // 立刻诊断：收到真实单步说明"没有拦住"，这一刻的计数就是原因线索。
  report("TF 单步：收到真实单步异常（已剥离 %llu 次 / 读补 %llu 次 / 栈失败 %llu 次 / 插装点 %llu 个）",
         static_cast<unsigned long long>(g_tf_masked),
         static_cast<unsigned long long>(g_tf_read_patches),
         static_cast<unsigned long long>(g_tf_stack_failures),
         static_cast<unsigned long long>(g_tf_sites));
  ThreadStats* stats = stats_for(tid);
  const UINT64 args[2] = {2, 0};
  push_probe_event(stats, 9 /* PB_EV_TRAP_FLAG */, tid,
                   PIN_GetContextReg(to, LEVEL_BASE::REG_INST_PTR), args, 2);
  if (g_tf_mode == TfMode::Virtualize && to != nullptr) {
    // Pin 明确允许在这里改 `to`：把这个真实单步消化掉，不让它级联。
    // 注意这**会改变目标看到的行为**（这正是 virtualize 的目的），
    // 所以它必须是显式选择，不能是默认。
    const ADDRINT flags = PIN_GetContextReg(to, LEVEL_BASE::REG_GFLAGS);
    PIN_SetContextReg(to, LEVEL_BASE::REG_GFLAGS, flags & ~kTrapFlagBit);
    ++g_tf_steps_suppressed;
  }
}

/// TF 相关指令的插桩（只在 `-pb_tf` 打开时才做助记符比较）。
void instrument_tf(INS ins) {
  if (!g_tf_enabled) {
    return;
  }
  if (INS_IsIRet(ins)) {
    INS_InsertCall(ins, IPOINT_BEFORE, reinterpret_cast<AFUNPTR>(OnFlagsLoadBefore),
                   IARG_THREAD_ID, IARG_INST_PTR, IARG_REG_VALUE,
                   LEVEL_BASE::REG_STACK_PTR, IARG_ADDRINT, static_cast<ADDRINT>(1),
                   IARG_END);
    return;
  }
  const std::string mnemonic = INS_Mnemonic(ins);
  // ⚠ `INS_Mnemonic` 返回的是**大写**助记符（实测：第一条指令打印出来是
  // "MOV"）。第一版按小写 "popfq" 比较，于是一个相关指令都没插桩，
  // 表现为"虚拟化没生效"——而其实是识别环节就断了。
  if (mnemonic == "POPFQ" || mnemonic == "POPF") {
    ++g_tf_sites;
    INS_InsertCall(ins, IPOINT_BEFORE, reinterpret_cast<AFUNPTR>(OnFlagsLoadBefore),
                   IARG_THREAD_ID, IARG_INST_PTR, IARG_REG_VALUE,
                   LEVEL_BASE::REG_STACK_PTR, IARG_ADDRINT, static_cast<ADDRINT>(0),
                   IARG_END);
    return;
  }
  if ((mnemonic == "PUSHFQ" || mnemonic == "PUSHF") &&
      INS_IsValidForIpointAfter(ins)) {
    INS_InsertCall(ins, IPOINT_AFTER, reinterpret_cast<AFUNPTR>(OnFlagsStoreAfter),
                   IARG_THREAD_ID, IARG_REG_VALUE, LEVEL_BASE::REG_STACK_PTR, IARG_END);
    ++g_tf_sites;
  }
}


// ══════════════════════════════════════════════════════════════════ 收尾
/// 进程退出前：停录、把事件取回来打印。
///
/// 打印事件是**刻意的**：一份"安静成功"的 Pin tool 无法区分
/// "探针没命中"与"事件通道断了"。这里把条数与内容都摆在 stderr 上。
void OnFini(INT32 code, VOID* v) {
  (void)v;
  // 第一件事：关掉所有回调。晚一步就会在"已经被拆掉的模块"里调用函数
  // （见 g_shutdown 的说明——这是实测踩出来的坑）。
  g_shutdown = true;
  // 冲刷线程可能刚被 g_shutdown 叫停，这里做最后一次兜底冲刷：
  // 「事件已经产生但没送出去」是最不该有的丢失，因为它不可见。
  const UINT64 flushed_at_fini = flush_all_rings();
  report("目标退出（code=%d），入口探针%s命中", code, g_entry_hit ? "已" : "**未**");
  (void)flushed_at_fini;

  if (g_api.stop_trace != nullptr && !KnobTrace.Value().empty()) {
    uint64_t records = 0;
    uint64_t events = 0;
    const int32_t stop = g_api.stop_trace(&records, &events);
    if (stop == PB_OK) {
      report("停止录制：记录 %llu 条 / 事件 %llu 条",
             static_cast<unsigned long long>(records),
             static_cast<unsigned long long>(events));
    } else {
      // **已知且刻意**的取舍，不是未处理的错误：Pin 的 Fini 晚于目标对 DLL 的
      // 卸载，而桥接层的卸载保护会拒绝这次调用（返回 NOT_ATTACHED）。
      // 代价是轨迹缺 SUMMARY/END 段；补偿是 agent 侧**增量落盘**，
      // 所以文件里已经有可读的记录。
      report("停止录制失败（返回 %d）——退出期的收尾被卸载保护拒绝。", stop);
      report("  这是已知取舍：不为了一个摘要段去调用已拆掉的 CRT。");
      report("  轨迹已增量落盘，读取器会报「汇总缺失」，不会假装完整。");
    }
  }

  // ── 探针账本 ──────────────────────────────────────────────────────────
  // "看到多少"与"漏了多少"必须一起报。只报前者会让人以为覆盖完整，
  // 而重入跳过与推事件失败都是**真实存在但看不见**的损失。
  UINT64 enters = 0;
  UINT64 exits = 0;
  UINT64 skips = 0;
  UINT64 failures = 0;
  for (UINT32 i = 0; i < kMaxThreadSlots; ++i) {
    enters += g_thread_slots[i].enters;
    exits += g_thread_slots[i].exits;
    skips += g_thread_slots[i].reentrant_skips;
    failures += g_thread_slots[i].push_failures;
  }
  enters += g_overflow_slot.enters;
  exits += g_overflow_slot.exits;
  skips += g_overflow_slot.reentrant_skips;
  failures += g_overflow_slot.push_failures;
  report("系统调用探针：进入点 %ld 个 / 返回点 %ld 个；捕获 enter=%llu exit=%llu",
         static_cast<long>(g_syscall_sites),
         static_cast<long>(g_syscall_exit_sites),
         static_cast<unsigned long long>(enters),
         static_cast<unsigned long long>(exits));
  report("  返回点：登记 %llu 个、成功插装 %llu 个%s",
         static_cast<unsigned long long>(g_syscall_returns_recorded),
         static_cast<unsigned long long>(g_syscall_returns_matched),
         g_syscall_returns_matched < g_syscall_returns_recorded
             ? "（有差额：返回点所在的基本块在登记之前就被翻译过了，那些返回不会被捕获）"
             : "");
  if (g_syscall_exit_without_enter != 0) {
    report("  有 %llu 次返回没有配对的进入（回退路径与 syscall 路径共用同一个 ret），\
这些事件的 args[0] 记为 0（调用号未知，不猜）",
           static_cast<unsigned long long>(g_syscall_exit_without_enter));
  }
  report("  重入跳过 %llu 条，推事件失败 %llu 条%s",
         static_cast<unsigned long long>(skips),
         static_cast<unsigned long long>(failures),
         g_slot_overflow != 0 ? "（线程槽位溢出，统计已并入共享槽）" : "");
  // 环形缓冲的账：溢出丢弃与已送出条数。少了这两行，"事件为什么少了"
  // 就只能靠猜——而猜测不是可复现的证据。
  UINT64 ring_dropped = 0;
  for (UINT32 i = 0; i < kMaxThreadSlots; ++i) {
    ring_dropped += g_thread_slots[i].ring.dropped;
  }
  ring_dropped += g_overflow_slot.ring.dropped;
  // 寄存器快照的账：**"没交上去"与"从来没命中过"是两件事**——
  // 前者是缺陷，后者是事实。少了这一行，调用方问"为什么 pb_registers
  // 读不到寄存器"时只能靠猜。
  report("  寄存器快照：%s（交付 %llu 次，失败 %llu 次，跳过 %llu 次无效）",
         g_api.set_thread_registers != nullptr ? "转发层支持" : "转发层没有该符号",
         static_cast<unsigned long long>(g_register_deliveries),
         static_cast<unsigned long long>(g_register_failures),
         static_cast<unsigned long long>(g_register_skipped));
  report("  环形缓冲：已送出 %llu 条（含收尾兜底 %llu 条），溢出丢弃 %llu 条",
         static_cast<unsigned long long>(g_flushed_total + flushed_at_fini),
         static_cast<unsigned long long>(flushed_at_fini),
         static_cast<unsigned long long>(ring_dropped));

  // 自动化判据：没捕够就明确打出一条"失败"标记，由 ctest 的
  // FAIL_REGULAR_EXPRESSION 判定。**不能只用一行普通日志**——
  // 那等于让"探针没工作"看起来像"跑成功了"。
  //
  // 为什么不用退出码：这段代码在 Fini 里，进程已经在退出流程中，
  // 从 Fini 里改退出码在各版本 Pin 上行为不一致；正则断言是可靠的。
  // 另外把"溢出丢弃"也纳入断言（`溢出丢弃 [1-9]`）——静默丢事件是禁止的。
  if (g_required_syscalls != 0) {
    const UINT64 captured = enters + exits;
    if (captured < g_required_syscalls) {
      report("失败：要求至少捕获 %llu 条系统调用**进入**事件，实际 %llu 条",
             static_cast<unsigned long long>(g_required_syscalls),
             static_cast<unsigned long long>(captured));
    } else {
      report("✓ 进入事件捕获数满足要求（%llu >= %llu）",
             static_cast<unsigned long long>(captured),
             static_cast<unsigned long long>(g_required_syscalls));
    }
  }
  if (g_required_syscall_exits != 0) {
    if (exits < g_required_syscall_exits) {
      report("失败：要求至少捕获 %llu 条系统调用**返回**事件，实际 %llu 条",
             static_cast<unsigned long long>(g_required_syscall_exits),
             static_cast<unsigned long long>(exits));
    } else {
      report("✓ 返回事件捕获数满足要求（%llu >= %llu）",
             static_cast<unsigned long long>(exits),
             static_cast<unsigned long long>(g_required_syscall_exits));
    }
  }

  // ── 内存探针的账 ──────────────────────────────────────────────────────
  if (g_mem.enabled) {
    UINT64 mem_writes_total = 0;
    UINT64 mem_reads_total = 0;
    for (UINT32 i = 0; i < kMaxThreadSlots; ++i) {
      mem_writes_total += g_thread_slots[i].mem_writes;
      mem_reads_total += g_thread_slots[i].mem_reads;
    }
    mem_writes_total += g_overflow_slot.mem_writes;
    mem_reads_total += g_overflow_slot.mem_reads;
    report("内存探针：插装点 %ld 个，捕获 write=%llu read=%llu",
           static_cast<long>(g_mem_sites),
           static_cast<unsigned long long>(mem_writes_total),
           static_cast<unsigned long long>(mem_reads_total));
    report("  地址范围过滤掉 %llu 条，缺 AFTER 点跳过 %ld 个写入点，值读不出 %llu 条",
           static_cast<unsigned long long>(g_mem_filtered),
           static_cast<long>(g_mem_skipped_no_after),
           static_cast<unsigned long long>(g_mem_unreadable));
    if (g_required_mem != 0) {
      const UINT64 captured = mem_writes_total + mem_reads_total;
      if (captured < g_required_mem) {
        report("失败：要求至少捕获 %llu 条内存事件，实际 %llu 条",
               static_cast<unsigned long long>(g_required_mem),
               static_cast<unsigned long long>(captured));
      } else {
        report("✓ 内存事件捕获数满足要求（%llu >= %llu）",
               static_cast<unsigned long long>(captured),
               static_cast<unsigned long long>(g_required_mem));
      }
    }
  }
  // ── TF 单步的账 ───────────────────────────────────────────────────────
  if (g_tf_enabled) {
    report("TF 单步：置位意图 %llu 次、清除意图 %llu 次，真实单步异常 %llu 次",
           static_cast<unsigned long long>(g_tf_sets),
           static_cast<unsigned long long>(g_tf_clears),
           static_cast<unsigned long long>(g_tf_steps_observed));
    if (g_tf_mode == TfMode::Virtualize) {
      report("  虚拟化：剥离真实 TF %llu 次，补回读副本 %llu 次，消化真实单步 %llu 次",
             static_cast<unsigned long long>(g_tf_masked),
             static_cast<unsigned long long>(g_tf_read_patches),
             static_cast<unsigned long long>(g_tf_steps_suppressed));
    }
    if (g_tf_stack_failures != 0) {
      report("  栈上 flags 副本读写失败 %llu 次（这部分没能虚拟化）",
             static_cast<unsigned long long>(g_tf_stack_failures));
    }
    if (g_required_tf_sets != 0) {
      if (g_tf_sets < g_required_tf_sets) {
        report("失败：要求至少观察到 %llu 次 TF 置位意图，实际 %llu 次",
               static_cast<unsigned long long>(g_required_tf_sets),
               static_cast<unsigned long long>(g_tf_sets));
      } else {
        report("✓ TF 置位意图数满足要求（%llu >= %llu）",
               static_cast<unsigned long long>(g_tf_sets),
               static_cast<unsigned long long>(g_required_tf_sets));
      }
    }
  }

  // ── 模板命中的账 ──────────────────────────────────────────────────────
  if (g_pattern_enabled) {
    UINT64 hits = 0;
    for (UINT32 i = 0; i < kMaxThreadSlots; ++i) {
      hits += g_thread_slots[i].pattern_hits;
    }
    hits += g_overflow_slot.pattern_hits;
    report("模板命中：登记地址 %u 个，命中 %llu 次", g_match_count,
           static_cast<unsigned long long>(hits));
    if (g_required_pattern_hits != 0) {
      if (hits < g_required_pattern_hits) {
        report("失败：要求至少命中 %llu 次，实际 %llu 次",
               static_cast<unsigned long long>(g_required_pattern_hits),
               static_cast<unsigned long long>(hits));
      } else {
        report("✓ 模板命中次数满足要求（%llu >= %llu）",
               static_cast<unsigned long long>(hits),
               static_cast<unsigned long long>(g_required_pattern_hits));
      }
    }
  }

  const UINT32 want = KnobEvents.Value();
  if (want == 0) {
    return;
  }
  // 用裸数组而不是 std::vector：Pin 的包含顺序会先引入它自带的 STL 头，
  // 实测 `std::vector::data()` 在这个编译单元里解析不到（报"不是成员"）。
  // 这点小事不值得跟它的包含顺序较劲。
  PbEventRaw* buffer = new PbEventRaw[want];
  UINT32 written = 0;
  const INT32 code2 = g_api.drain_events(buffer, want, &written);
  if (code2 != PB_OK) {
    report("取事件失败：%d %s", code2, last_error_text().c_str());
    delete[] buffer;
    return;
  }
  report("取回 %u 条事件（丢弃 %llu 条）", written,
         static_cast<unsigned long long>(g_api.stats != nullptr ? g_api.stats(0) : 0));
  for (UINT32 i = 0; i < written; ++i) {
    const PbEventRaw& e = buffer[i];
    report("  #%llu kind=%u tid=%u addr=0x%llX arg0=0x%llX",
           static_cast<unsigned long long>(e.sequence), e.kind, e.thread_id,
           static_cast<unsigned long long>(e.address),
           static_cast<unsigned long long>(e.args[0]));
  }
  delete[] buffer;
}

}  // namespace

/// Pin 工具的入口（由 Pin 调用；链接时需要 /EXPORT:main）。
int main(int argc, char** argv) {
  if (PIN_Init(argc, argv)) {
    std::fputs("PinBridge: PIN_Init 失败（参数不合法？）\n", stderr);
    return 1;
  }
  PIN_InitSymbols();

  report("Pin %d.%d build %d (%s) | 目标 pid=%u", PIN_PRODUCT_VERSION_MAJOR,
         PIN_PRODUCT_VERSION_MINOR, PIN_BUILD_NUMBER, PIN_COMMIT_HASH, PIN_GetPid());

  if (!load_bridge()) {
    // 返回非零：Pin 会把退出码交给调用方。**不静默继续**——
    // 一个"跑起来了但什么都没接上"的 Pin tool 比明确失败更浪费时间。
    return 2;
  }
  // 加载成功之后立刻钉住：这是消除"退出期回调进已卸载模块"窗口的关键一步，
  // 必须在布下任何探针**之前**做（探针一旦生效就可能随时被命中）。
  pin_loaded_modules();

  if (g_api.attach(PIN_GetPid()) != PB_OK) {
    report("pb_attach 失败：%s", last_error_text().c_str());
    return 3;
  }
  const uint32_t caps = KnobCaps.Value() == "all" ? PB_CAP_ALL : PB_CAP_READ_ONLY_BASELINE;
  if (g_api.set_capabilities(caps) != PB_OK) {
    report("pb_set_capabilities(0x%X) 失败：%s", caps, last_error_text().c_str());
    return 4;
  }
  report("已附加并下发能力 0x%X（%s）", caps,
         caps == PB_CAP_ALL ? "全部" : "只读基线 mem_read|trace_record");

  // 探针开关：默认开系统调用探针，并带上限。上限的存在不是为了省事，
  // 而是为了让"长时间运行的目标"不会把事件队列灌满、也不会让日志无法阅读；
  // 达到上限时会**明确报告**（见 stop_syscall_instrumentation）。
  pb_syscall_enabled = KnobSyscalls.Value() != 0;

  g_syscall_cap = parse_u64(KnobSyscallMax.Value().c_str(), 4096);
  g_required_syscalls = parse_u64(KnobRequireSyscalls.Value().c_str(), 0);
  g_required_syscall_exits = parse_u64(KnobRequireSyscallExits.Value().c_str(), 0);

  // 内存探针：模式串 `0/w/r/rw`；范围形如 `lo-hi`（十六进制）。
  const std::string mem_mode = KnobMem.Value();
  g_mem.enabled = mem_mode != "0" && !mem_mode.empty();
  g_mem.writes = mem_mode.find('w') != std::string::npos;
  g_mem.reads = mem_mode.find('r') != std::string::npos;
  g_mem.main_image_only = !KnobMemAllModules.Value();
  g_mem.max_events = parse_u64(KnobMemMax.Value().c_str(), 20000);
  g_required_mem = parse_u64(KnobRequireMem.Value().c_str(), 0);
  const std::string mem_range = KnobMemRange.Value();
  const std::size_t dash = mem_range.find('-');
  if (dash != std::string::npos) {
    g_mem.low = parse_hex_u64(mem_range.substr(0, dash).c_str());
    g_mem.high = parse_hex_u64(mem_range.substr(dash + 1).c_str());
    if (g_mem.high < g_mem.low) {
      // 不给"空区间"这种含糊状态：交换并说出来。
      report("警告：-pb_mem_range 的 hi < lo，已交换两端");
      const UINT64 swap = g_mem.low;
      g_mem.low = g_mem.high;
      g_mem.high = swap;
    }
  }
  // TF 单步：三档语义完全不同，所以旋钮值必须**精确**匹配，不猜。
  {
    const std::string tf_mode = KnobTf.Value();
    if (tf_mode == "off") {
      g_tf_mode = TfMode::Off;
    } else if (tf_mode == "watch") {
      g_tf_mode = TfMode::Watch;
    } else if (tf_mode == "virtualize") {
      g_tf_mode = TfMode::Virtualize;
    } else {
      report("无法识别 -pb_tf 的取值 \"%s\"（只能是 off / watch / virtualize）", tf_mode.c_str());
      return 7;
    }
    g_tf_enabled = g_tf_mode != TfMode::Off;
    g_required_tf_sets = parse_u64(KnobRequireTfSets.Value().c_str(), 0);
    report("TF 单步：%s%s", tf_mode.c_str(),
           g_tf_mode == TfMode::Virtualize
               ? "（**会改变目标行为**：真实 TF 会被剥掉，目标读到的仍是置位的）"
               : "");
  }

  // 模板：解析失败就直接结束（不静默跳过某一条）。
  g_pattern_enabled = KnobPattern.Value().empty() == false;
  g_pattern_max_per_pattern = parse_u64(KnobPatternMax.Value().c_str(), 64);
  g_required_pattern_hits = parse_u64(KnobRequirePatternHits.Value().c_str(), 0);
  if (g_pattern_enabled) {
    if (!parse_patterns(KnobPattern.Value().c_str())) {
      report("模板解析失败，拒绝继续（静默跳过某条模板会让\"它没生效\"看起来像\"它没命中\"）");
      return 6;
    }
    for (UINT32 i = 0; i < g_pattern_count; ++i) {
      report("模板 %u：%s（%u 字节，%s）", g_patterns[i].id, g_patterns[i].name,
             g_patterns[i].length, "只扫主模块");
    }
  } else {
    g_pattern_enabled = false;
  }

  char range_lo[24];
  char range_hi[24];
  format_hex_into(range_lo, sizeof(range_lo), g_mem.low);
  format_hex_into(range_hi, sizeof(range_hi), g_mem.high);
  report("内存探针：%s（读=%s 写=%s，%s，范围 %s-%s，上限 %llu）",
         g_mem.enabled ? "开" : "关", g_mem.reads ? "是" : "否", g_mem.writes ? "是" : "否",
         g_mem.main_image_only ? "仅主模块" : "全部模块",
         (g_mem.low == 0 && g_mem.high == 0) ? "不限" : range_lo,
         (g_mem.low == 0 && g_mem.high == 0) ? "" : range_hi,
         static_cast<unsigned long long>(g_mem.max_events));
  report("系统调用探针：%s（上限 %llu 条，0 表示不限）",
         pb_syscall_enabled ? "开" : "关",
         static_cast<unsigned long long>(g_syscall_cap));

  if (!KnobTrace.Value().empty()) {
    const int32_t started = g_api.start_trace(KnobTrace.Value().c_str(), 1);
    if (started != PB_OK) {
      report("开始录制失败：%s", last_error_text().c_str());
      return 5;
    }
    report("录制中 → %s", KnobTrace.Value().c_str());
  }

  IMG_AddInstrumentFunction(OnImageLoad, nullptr);
  if (g_tf_enabled) {
    // 只在需要时才注册：这个回调只在异常发生时才被调用，平时零成本。
    PIN_AddContextChangeFunction(OnContextChange, nullptr);
  }
  IMG_AddUnloadFunction(OnImageUnload, nullptr);
  INS_AddInstrumentFunction(OnInstruction, nullptr);
  PIN_AddFiniFunction(OnFini, nullptr);

  // 冲刷线程：**必须在布下探针之前起来**，否则第一批事件会一直躺在环里
  // 等到第一次轮询（10 ms 后）才出去。它不是回调上下文、也不被插桩，
  // 因此是唯一可以安全跨边界推事件的地方（见 push_probe_event 的注释）。
  PIN_THREAD_UID flusher_uid = 0;
  // `PIN_SpawnInternalThread` 用 `PIN_ThreadId` 之外的方式表示失败：
  // 它返回 `PIN_THREAD_UID` 的指针，失败时是…实测拿 `nullptr` 比较会被拒
  // （C2446 无法把 nullptr 转为 PIN_THREAD_UID），所以用句柄有效性判断。
  PIN_SpawnInternalThread(FlusherMain, nullptr, 0, &flusher_uid);
  if (flusher_uid == static_cast<PIN_THREAD_UID>(0)) {
    report("警告：冲刷线程未启动，事件只会留在环形缓冲里（收尾时仍会兜底冲刷）");
  } else {
    report("冲刷线程已启动（10 ms 轮询；它不是回调上下文，跨边界在这里发生）");
  }

  PIN_StartProgram();  // 不返回
  return 0;
}
