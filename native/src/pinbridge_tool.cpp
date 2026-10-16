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
                                      "捕获到的系统调用事件少于该值时以非零退出（0 表示不检查）");

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


// 系统调用探针的开关、上限与观测计数（定义见下方"系统调用探针"一节）
bool pb_syscall_enabled = true;
UINT64 g_syscall_cap = 0;
/// 捕获数下限（见 KnobRequireSyscalls）。
UINT64 g_required_syscalls = 0;
volatile long g_syscall_sites = 0;

/// 入口点命中：这是**真实事件**（不是模拟数据），它会经过
/// 转发层 → agent → 事件队列 → 轨迹文件 → Hub 侧加载器。
void OnEntryPointHit(THREADID thread_id, const CONTEXT* ctxt, ADDRINT address) {
  (void)ctxt;
  if (g_entry_hit) {
    return;
  }
  g_entry_hit = true;
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

void OnImageLoad(IMG img, VOID* v) {
  (void)v;
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
  report("主模块 %s 入口点 0x%llX，已布下入口探针", IMG_Name(img).c_str(),
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
constexpr UINT32 kRingCapacity = 1024;

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
};

/// 线程统计表。用固定槽位而不是每条线程都 new：探针回调在**任意线程**上
/// 运行，那里不能做可能失败/可能阻塞的分配（分配本身可能又触发系统调用）。
///
/// 槽位溢出时不假装成功：`g_slot_overflow` 会 +1 并在收尾时报出来。
// 线程槽位：每个槽位带一个 64 KiB 的环，所以不能随便放大（见 kRingCapacity）。
constexpr UINT32 kMaxThreadSlots = 64;
ThreadStats g_thread_slots[kMaxThreadSlots];
ThreadStats g_overflow_slot;   // 槽位用尽后的共享槽（只记不细分）
volatile long g_slot_overflow = 0;
volatile long g_syscall_captured = 0;
/// 已成功跨边界送出的事件数（由冲刷线程累加）。
volatile UINT64 g_flushed_total = 0;
/// 跨边界推事件失败的次数（必须报出来，不允许静默丢弃）。
volatile UINT64 g_push_failures = 0;
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
UINT64 flush_ring(EventRing* ring) {
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
    ring->tail = ring->tail + 1;
    ++flushed;
  }
  return flushed;
}

/// 冲刷所有线程的环。返回本次送出的事件数。
UINT64 flush_all_rings() {
  UINT64 flushed = 0;
  for (UINT32 i = 0; i < kMaxThreadSlots; ++i) {
    flushed += flush_ring(&g_thread_slots[i].ring);
  }
  flushed += flush_ring(&g_overflow_slot.ring);
  return flushed;
}

/// 冲刷线程主体（Pin 内部线程）。
///
/// 用 10 ms 轮询而不是事件驱动：目标是"低开销采集"，不是"实时"。
/// 环满时生产者会丢弃并计数（丢最新），因此延迟不会变成丢失的借口。
VOID FlusherMain(VOID* arg) {
  (void)arg;
  while (!g_shutdown) {
    g_flushed_total += flush_all_rings();
    ::Sleep(10);
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

/// `sysret` 之前：此时 RAX 已经是内核写回的返回状态。
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
  const UINT64 args[1] = {static_cast<UINT64>(status)};
  push_probe_event(stats, 4 /* PB_EV_SYSCALL_EXIT */, tid, address, args, 1);
  stats->in_hook = false;
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

/// 指令级插桩：入口点 + 系统调用。
void OnInstruction(INS ins, VOID* v) {
  (void)v;

  const ADDRINT address = static_cast<ADDRINT>(INS_Address(ins));

  // 自己模块里的指令一律不插桩（理由见 is_own_module）。
  if (is_excluded(address)) {
    return;
  }

  // ── 入口点探针（只关心主模块入口那一条指令）──
  if (!g_entry_hit && g_entry_address != 0 && address == g_entry_address) {
    INS_InsertCall(ins, IPOINT_BEFORE, reinterpret_cast<AFUNPTR>(OnEntryPointHit),
                   IARG_THREAD_ID, IARG_CONTEXT, IARG_INST_PTR, IARG_END);
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
  if (INS_IsSyscall(ins)) {
    // 只登记 SSN 与四个参数寄存器，不做任何地址计算——这些值在
    // IPOINT_BEFORE 处读，正是内核即将看到的那一份。
    INS_InsertCall(ins, IPOINT_BEFORE, reinterpret_cast<AFUNPTR>(OnSyscallEnter),
                   IARG_THREAD_ID, IARG_INST_PTR, IARG_REG_VALUE, LEVEL_BASE::REG_RAX,
                   IARG_REG_VALUE, LEVEL_BASE::REG_R10, IARG_REG_VALUE,
                   LEVEL_BASE::REG_RDX, IARG_REG_VALUE, LEVEL_BASE::REG_R8,
                   IARG_REG_VALUE, LEVEL_BASE::REG_R9, IARG_END);
    ++g_syscall_sites;
    return;
  }
  if (INS_IsSysret(ins)) {
    INS_InsertCall(ins, IPOINT_BEFORE, reinterpret_cast<AFUNPTR>(OnSyscallExit),
                   IARG_THREAD_ID, IARG_INST_PTR, IARG_REG_VALUE,
                   LEVEL_BASE::REG_RAX, IARG_END);
    ++g_syscall_sites;
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
  report("系统调用探针：插装点 %ld 个，捕获 enter=%llu exit=%llu",
         static_cast<long>(g_syscall_sites),
         static_cast<unsigned long long>(enters),
         static_cast<unsigned long long>(exits));
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
      report("失败：要求至少捕获 %llu 条系统调用事件，实际 %llu 条",
             static_cast<unsigned long long>(g_required_syscalls),
             static_cast<unsigned long long>(captured));
    } else {
      report("✓ 捕获数满足要求（%llu >= %llu）",
             static_cast<unsigned long long>(captured),
             static_cast<unsigned long long>(g_required_syscalls));
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
