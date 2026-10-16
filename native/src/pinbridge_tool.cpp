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
}
using ModuleHandle = void*;

#include <cstdio>
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

// ══════════════════════════════════════════════════════════════════ 探针
volatile bool g_entry_hit = false;
ADDRINT g_entry_address = 0;

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

/// 指令级插桩：只关心"主模块入口点"这一条指令。
///
/// 为什么先用这一个探针：它是**最小可验证**的插桩点——地址在 IMG 加载时就已知，
/// 命中一次即停，不对目标产生可观测的时序影响。系统调用/内存/模板那几类探针
/// 需要过滤、聚合与折叠配合（见 docs/design/injection-points.md），
/// 放在后续提交里逐个加，避免一次引入太多变量而无法定位问题。
void OnInstruction(INS ins, VOID* v) {
  (void)v;
  if (g_entry_hit || g_entry_address == 0) {
    return;
  }
  if (static_cast<ADDRINT>(INS_Address(ins)) != g_entry_address) {
    return;
  }
  INS_InsertCall(ins, IPOINT_BEFORE, reinterpret_cast<AFUNPTR>(OnEntryPointHit),
                 IARG_THREAD_ID, IARG_CONTEXT, IARG_INST_PTR, IARG_END);
}

void OnImageLoad(IMG img, VOID* v) {
  (void)v;
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
}

// ══════════════════════════════════════════════════════════════════ 收尾
/// 进程退出前：停录、把事件取回来打印。
///
/// 打印事件是**刻意的**：一份"安静成功"的 Pin tool 无法区分
/// "探针没命中"与"事件通道断了"。这里把条数与内容都摆在 stderr 上。
void OnFini(INT32 code, VOID* v) {
  (void)v;
  report("目标退出（code=%d），入口探针%s命中", code, g_entry_hit ? "已" : "**未**");

  if (g_api.stop_trace != nullptr && !KnobTrace.Value().empty()) {
    uint64_t records = 0;
    uint64_t events = 0;
    const int32_t stop = g_api.stop_trace(&records, &events);
    report("停止录制：返回 %d，记录 %llu 条 / 事件 %llu 条", stop,
           static_cast<unsigned long long>(records),
           static_cast<unsigned long long>(events));
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

  if (!KnobTrace.Value().empty()) {
    const int32_t started = g_api.start_trace(KnobTrace.Value().c_str(), 1);
    if (started != PB_OK) {
      report("开始录制失败：%s", last_error_text().c_str());
      return 5;
    }
    report("录制中 → %s", KnobTrace.Value().c_str());
  }

  IMG_AddInstrumentFunction(OnImageLoad, nullptr);
  INS_AddInstrumentFunction(OnInstruction, nullptr);
  PIN_AddFiniFunction(OnFini, nullptr);

  PIN_StartProgram();  // 不返回
  return 0;
}
