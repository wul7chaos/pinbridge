// agent 桥接层：把 Hub 侧的 tool ABI 转发到 agent DLL。
//
// ───────────────────────────────────────────────────────────────────────────
// 这个文件存在的理由
// ───────────────────────────────────────────────────────────────────────────
// Hub 侧（crates/pinbridge-sys）通过运行时加载一个 DLL 来驱动目标端。在 Pin
// SDK 到位之前，那个 DLL 不存在，于是"Hub ↔ agent"这条链路**一行都没被执行过**——
// 版本校验、能力下发、事件批量取回、错误文本传递全都停留在"测试过的类型"层面。
//
// 本文件提供的就是那段缺失的转发：导出 Hub 期望的 8 个 tool 符号，内部按
// 同样的"先校验 ABI 版本、再解析函数指针"的顺序加载 agent DLL 并转发调用。
// 它与未来的 Pin tool（native/src/pinbridge_tool.cpp）是**同一套转发拓扑**，
// 区别只有一个：本文件没有探针，因此 `pb_start` 无法真正启动目标。
//
// 于是今天就能做到：Hub 加载真实 DLL → 校验真实 ABI → 下发真实能力 →
// 真实地取回真实 agent 生产的事件。Pin 到位后，把同一份转发逻辑搬进
// pinbridge_tool.cpp，再加探针注册即可。
//
// ───────────────────────────────────────────────────────────────────────────
// 边界（不要在这上面做幻想）
// ───────────────────────────────────────────────────────────────────────────
// * `pb_start(image, args)` 返回 PB_STATUS_NOT_SUPPORTED：启动目标需要 Pin
//   的 JIT 注入，桥接层没有这个能力。它**故意**返回明确的"不支持"，
//   而不是假装启动成功——静默降级会让上层在错误前提上继续推理。
// * agent DLL 的路径按以下顺序确定：
//     1. 环境变量 PINBRIDGE_AGENT_DLL
//     2. 与本 DLL 同目录的 pinbridge_agent.dll
//     3. 系统搜索路径下的 pinbridge_agent.dll
//   找不到时所有转发调用都返回 PB_STATUS_NOT_ATTACHED，且 pb_last_error()
//   会给出**尝试过的具体路径**（"DLL 加载失败"这种话在排查时毫无价值）。
// * 本层不解析 NDJSON、不认识会话与 epoch：那是 Hub 的事。这里是纯粹的
//   函数指针转发 + 状态码翻译。

#include "pinbridge.h"

#include <windows.h>

#include <cstdarg>
#include <cstdio>
#include <cstring>
#include <mutex>
#include <string>

namespace {

// ---------------------------------------------------------------- 函数指针表
// 只声明我们用到的那些。与 pinbridge-sys/src/loader.rs 的表保持同名同型。
using fn_abi_version = uint32_t(PB_CALL*)();
using fn_attach = int32_t(PB_CALL*)(uint32_t);
using fn_set_capabilities = int32_t(PB_CALL*)(uint32_t);
using fn_continue = int32_t(PB_CALL*)();
using fn_pause = int32_t(PB_CALL*)();
using fn_single_step = int32_t(PB_CALL*)(uint32_t);
using fn_set_thread_registers = int32_t(PB_CALL*)(uint32_t, const uint64_t*, uint32_t);
using fn_drain_events = int32_t(PB_CALL*)(pb_event_raw*, uint32_t, uint32_t*);
using fn_last_error = const char*(PB_CALL*)();
using fn_allocate_sequence = uint64_t(PB_CALL*)();
using fn_push_event = int32_t(PB_CALL*)(const pb_event_raw*);
using fn_stats = uint64_t(PB_CALL*)(uint32_t);
// 采集覆盖（元信息，最终由 agent 写进轨迹的 AUDIT 通道）。
// **可选**符号：老 agent 没有它，只是离线侧看不到覆盖缺口。
using fn_set_coverage = int32_t(PB_CALL*)(uint64_t, uint64_t, uint64_t, uint64_t, uint32_t);
// 轨迹录制也属于**工具 ABI**（头文件第 156-157 行声明了它们）。
// 早先桥接层漏了这两个转发，于是"Hub/Pin tool 让目标自己录一份轨迹"
// 这条路径在桥接拓扑下不可达——而且失败信息是"导出面不完整"，
// 看不出缺的是哪两个。这是 Pin tool 第一次跑起来才暴露出来的。
using fn_start_trace = int32_t(PB_CALL*)(const char*, uint32_t);
using fn_stop_trace = int32_t(PB_CALL*)(uint64_t*, uint64_t*);

struct AgentApi {
  HMODULE module = nullptr;
  /// 0 表示加载成功；否则是一个 pb_status（供 pb_last_error 之外的路径直接返回）。
  int32_t load_status = PB_STATUS_OK;
  /// 加载失败的细节，由 pb_last_error 暴露。
  std::string detail;

  fn_abi_version abi_version = nullptr;
  fn_attach attach = nullptr;
  fn_set_capabilities set_capabilities = nullptr;
  fn_continue resume = nullptr;
  fn_pause pause = nullptr;
  fn_single_step single_step = nullptr;
  fn_set_thread_registers set_thread_registers = nullptr;
  fn_drain_events drain_events = nullptr;
  fn_last_error last_error = nullptr;
  fn_allocate_sequence allocate_sequence = nullptr;
  fn_push_event push_event = nullptr;
  fn_stats stats = nullptr;
  fn_set_coverage set_coverage = nullptr;
  fn_start_trace start_trace = nullptr;
  fn_stop_trace stop_trace = nullptr;

  /// ABI 版本一致时才算“可用”。
  bool usable() const { return load_status == PB_STATUS_OK; }
};

/// 本 DLL 自己的模块句柄（用于推导同目录的 agent 路径）。
HMODULE g_bridge_module = nullptr;

/// 错误文本缓冲。`pb_last_error` 返回的指针必须在下一次写错误之前有效。
///
/// ── 为什么**不能**用 thread_local ──────────────────────────────────────
/// 本 DLL 用静态 CRT（/MT），`thread_local` 会走 CRT 的**线程私有数据（PTD）**。
/// 而进程退出时会 `FreeLibrary` 我们，CRT 的私有数据随之被释放——此时再去访问
/// 任何 `thread_local`，底层就是"`TlsGetValue` 返回 null，再取偏移 8"，
/// 于是得到 `RECEIVED_ACCESS_FAULT, Access Address = 0x8`，
/// 而崩溃 IP 落在本 DLL 的映像里、栈上毫无线索（**实测踩过**，见下面 g_detaching
/// 的说明：系统调用探针在退出期仍会命中，所以这条路径真的会被走到）。
///
/// 换成普通静态缓冲：容量不变、语义不变（多线程下"最后一条错误"本来就只保证
/// 覆盖顺序，不保证归属），但**不再依赖任何会随卸载消失的东西**。
static char g_error[1024] = {0};

void set_error(const char* format, ...) {
  va_list args;
  va_start(args, format);
  (void)std::vsnprintf(g_error, sizeof(g_error), format, args);
  va_end(args);
}

void clear_error() { g_error[0] = '\0'; }

/// 取本 DLL 所在目录（带结尾反斜杠）。
std::string module_directory(HMODULE module) {
  char path[MAX_PATH] = {0};
  const DWORD length = ::GetModuleFileNameA(module, path, MAX_PATH);
  if (length == 0 || length >= MAX_PATH) {
    return std::string();
  }
  std::string text(path, length);
  const std::size_t slash = text.find_last_of("\\/");
  if (slash == std::string::npos) {
    return std::string();
  }
  return text.substr(0, slash + 1);
}

/// 按优先级算出候选路径。返回值用于错误信息，也用于实际加载。
std::string agent_candidate_paths(std::string* out) {
  std::string tried;
  const char* from_env = ::getenv("PINBRIDGE_AGENT_DLL");
  if (from_env != nullptr && *from_env != '\0') {
    tried += from_env;
    tried += " ";
  }
  const std::string sibling = module_directory(g_bridge_module) + "pinbridge_agent.dll";
  if (sibling != "pinbridge_agent.dll") {
    tried += sibling;
    tried += " ";
  }
  tried += "pinbridge_agent.dll";
  *out = tried;
  return *out;
}

template <typename T>
T resolve_symbol(HMODULE module, const char* name, std::string* missing) {
  // 先转成 void* 再转函数指针：绕开 FARPROC 的“不安全转换”告警，
  // 也避免不同编译器对函数指针与数据指针互转的差异。
  void* raw = reinterpret_cast<void*>(::GetProcAddress(module, name));
  if (raw == nullptr) {
    if (!missing->empty()) {
      *missing += ", ";
    }
    *missing += name;
    return nullptr;
  }
  return reinterpret_cast<T>(raw);
}

/// 加载 agent 并解析全部需要的符号。**先校验 ABI 版本，再解析其余符号**：
///
/// 反过来的话，一个版本不匹配的 DLL 里同名的函数会被我们按错误的签名调用，
/// 结果是把目标进程打崩，而崩因会指向目标程序本身——这是最难排查的一类事故。
void load_agent(AgentApi* api) {
  std::string tried;
  (void)agent_candidate_paths(&tried);

  const char* from_env = ::getenv("PINBRIDGE_AGENT_DLL");
  const bool explicit_path = (from_env != nullptr && *from_env != '\0');

  HMODULE module = nullptr;
  if (explicit_path) {
    // **显式指定就不兜底**。这一点是刻意的：如果操作员指向一个新构建、
    // 但路径写错了，而我们从自己目录里悄悄加载了一个旧 DLL，
    // 那么接下来所有的调试都发生在错误的二进制上——这是最难发现的
    // 一类事故（现象与"代码没生效"完全一样）。
    module = ::LoadLibraryA(from_env);
    if (module == nullptr) {
      api->load_status = PB_STATUS_NOT_ATTACHED;
      api->detail = std::string("PINBRIDGE_AGENT_DLL 指定的 agent 加载失败: ") + from_env +
                    "（显式指定时不回退到同目录，避免加载到陈旧构建）";
      set_error("%s", api->detail.c_str());
      return;
    }
  } else {
    // 未显式指定：先找同目录（部署时的常见形态），再交给系统搜索路径。
    const std::string sibling = module_directory(g_bridge_module) + "pinbridge_agent.dll";
    module = ::LoadLibraryA(sibling.c_str());
    if (module == nullptr) {
      module = ::LoadLibraryA("pinbridge_agent.dll");
    }
  }
  if (module == nullptr) {
    api->load_status = PB_STATUS_NOT_ATTACHED;
    api->detail = "无法加载 agent DLL。已尝试: " + tried +
                  "（可用 PINBRIDGE_AGENT_DLL 指定完整路径）";
    set_error("%s", api->detail.c_str());
    return;
  }
  api->module = module;

  // ---- 第一步：ABI 版本闸门 ----
  std::string missing;
  api->abi_version = resolve_symbol<fn_abi_version>(module, "pb_abi_version", &missing);
  if (api->abi_version == nullptr) {
    api->load_status = PB_STATUS_ABI_MISMATCH;
    api->detail = "agent DLL 缺少 pb_abi_version，无法校验 ABI：缺 " + missing;
    set_error("%s", api->detail.c_str());
    return;
  }
  const uint32_t agent_version = api->abi_version();
  if (agent_version != PB_ABI_VERSION) {
    api->load_status = PB_STATUS_ABI_MISMATCH;
    char text[256] = {0};
    (void)std::snprintf(text, sizeof(text),
                        "ABI 版本不匹配：桥接层 %u，agent %u（拒绝加载，不做尽力兼容）",
                        static_cast<unsigned>(PB_ABI_VERSION),
                        static_cast<unsigned>(agent_version));
    api->detail = text;
    set_error("%s", api->detail.c_str());
    return;
  }

  // ---- 第二步：解析函数指针 ----
  api->attach = resolve_symbol<fn_attach>(module, "pb_attach", &missing);
  api->set_capabilities =
      resolve_symbol<fn_set_capabilities>(module, "pb_set_capabilities", &missing);
  api->resume = resolve_symbol<fn_continue>(module, "pb_continue", &missing);
  api->pause = resolve_symbol<fn_pause>(module, "pb_pause", &missing);
  api->single_step =
      resolve_symbol<fn_single_step>(module, "pb_single_step", &missing);
  // 寄存器快照是**可选**符号：它属于 M1 的追加能力，旧版 agent DLL 没有它
  // 只是拿不到寄存器（Pin 工具侧也按可选处理）。把它算进"导出面不全"会让
  // "旧 agent + 新桥接层"这种完全可用的组合直接起不来——**追加能力不该
  // 变成硬依赖**。
  {
    std::string optional_missing;
    api->set_thread_registers =
        resolve_symbol<fn_set_thread_registers>(module, "pb_set_thread_registers",
                                                &optional_missing);
  }
  // 采集覆盖同样是**可选**符号，理由同上：追加能力不能变成硬依赖。
  {
    std::string optional_missing;
    api->set_coverage = resolve_symbol<fn_set_coverage>(module, "pb_set_coverage",
                                                       &optional_missing);
  }
  api->drain_events = resolve_symbol<fn_drain_events>(module, "pb_drain_events", &missing);
  api->last_error = resolve_symbol<fn_last_error>(module, "pb_last_error", &missing);
  api->allocate_sequence =
      resolve_symbol<fn_allocate_sequence>(module, "pb_allocate_sequence", &missing);
  api->push_event = resolve_symbol<fn_push_event>(module, "pb_push_event", &missing);
  api->stats = resolve_symbol<fn_stats>(module, "pb_stats", &missing);
  api->start_trace = resolve_symbol<fn_start_trace>(module, "pb_start_trace", &missing);
  api->stop_trace = resolve_symbol<fn_stop_trace>(module, "pb_stop_trace", &missing);

  if (!missing.empty()) {
    api->load_status = PB_STATUS_NOT_SUPPORTED;
    api->detail = "agent DLL 缺少导出符号（ABI 版本一致但导出面不全）：" + missing;
    set_error("%s", api->detail.c_str());
    return;
  }

  api->load_status = PB_STATUS_OK;
  clear_error();
}

AgentApi& agent() {
  // ── 为什么用堆上单例而不是函数内静态对象 ─────────────────────────────
  // 函数内静态对象（`static AgentApi instance;`）会在 **DLL 卸载时被析构**。
  // 而"卸载之后还会被调用"这件事是真实存在的：目标进程退出时会 FreeLibrary
  // 我们的 DLL，但系统调用插装此刻**仍在命中**（`NtTerminateProcess` 本身
  // 就是一次系统调用）。于是回调走进了已经析构的对象——`AgentApi` 里的
  // `std::string detail` 内部指针已被释放，读它就得到
  // `RECEIVED_ACCESS_FAULT, Access Address = 0x8` 这种"空指针 + 偏移"。
  //
  // 实测证据：故障 IP 固定在桥接层 `+0x19fa`（同一台机器上每次都一样），
  // 只在系统调用探针下出现，且总在目标打印完最后一行之后。
  //
  // 因此：这个对象**故意不析构**（进程退出时由系统回收）。用一个"指针"
  // 形式的函数内静态量：它的初始化是常量初始化，没有析构、也没有 guard 变量。
  static AgentApi* instance = new AgentApi();
  static std::once_flag once;
  // 不捕获 `instance`：静态存储期的变量**不能**被 lambda 捕获
  // （MSVC: "简单捕获必须是含自动存储持续时间的变量"），
  // 而静态变量本来就能在 lambda 体里直接引用。
  std::call_once(once, []() { load_agent(instance); });
  return *instance;
}

/// 桥接层正在卸载。
///
/// `DllMain(DLL_PROCESS_DETACH)` 时置位。之后任何调用都**明确报错**而不是
/// 去碰可能已经不可用的东西——"能被调用"与"调用有意义"是两件事，
/// 前者我们无法阻止（宿主可能随时调），后者我们必须说清楚。
bool g_detaching = false;

/// 统一的“转发前置检查”：加载失败时把原因写进错误文本并返回对应状态码。
bool require_agent(int32_t* status) {
  if (g_detaching) {
    // ── 卸载中：**什么都不碰** ────────────────────────────────────────
    // 这里不写错误文本，是有实测依据的：`g_error` 是 `thread_local`，
    // 而 DLL 卸载之后每个线程的 CRT 私有数据已经释放。此时**任何 CRT 调用**
    // （`vsnprintf`，甚至访问 `thread_local` 本身）都会走到空指针上，
    // 表现为 `RECEIVED_ACCESS_FAULT, Access Address = 0x8`
    // ——那正是"TlsGetValue 返回 null，再取偏移 8"的形状，而且崩溃 IP 会
    // 落在本 DLL 的映像里，栈上看不出任何线索。
    //
    // 所以：只写调用方给我的 out 参数（在它的栈上），别的一律不动。
    *status = PB_STATUS_NOT_ATTACHED;
    return false;
  }
  const AgentApi& api = agent();
  if (api.usable()) {
    return true;
  }
  // 加载阶段已经把详情写进 g_error；这里保持它不被覆盖。
  if (g_error[0] == '\0') {
    set_error("%s", api.detail.empty() ? "agent 不可用" : api.detail.c_str());
  }
  *status = api.load_status;
  return false;
}

}  // namespace

// ════════════════════════════════════════════════════════════════════ tool ABI
// 以下 8 个符号是 Hub 侧 pinbridge-sys 的加载器期望的全部内容。
// 名称与签名必须与 crates/pinbridge-sys/src/loader.rs 和 docs/specs/c-abi.md 一致。

extern "C" {

BOOL WINAPI DllMain(HINSTANCE instance, DWORD reason, LPVOID reserved) {
  (void)reserved;
  if (reason == DLL_PROCESS_ATTACH) {
    g_bridge_module = instance;
  } else if (reason == DLL_PROCESS_DETACH) {
    // 立刻置位：之后任何转发调用都会得到明确错误（见 require_agent）。
    g_detaching = true;
  }
  return TRUE;
}

/// 桥接层自身的 ABI 版本。**即使 agent 缺失也必须能回答**——
/// 否则 Hub 连"这个 DLL 是不是我能加载的"都判断不了。
PB_EXPORT uint32_t PB_CALL pb_abi_version(void) { return PB_ABI_VERSION; }

/// 启动目标并插装。
///
/// 桥接层没有探针，因此返回明确的不支持。真实实现见
/// native/src/pinbridge_tool.cpp（需要 Intel Pin）。
PB_EXPORT int32_t PB_CALL pb_start(const char* image_path, const char* args) {
  if (image_path == nullptr || args == nullptr) {
    set_error("pb_start: image_path/args 为空指针");
    return PB_STATUS_INVALID_ARGUMENT;
  }
  set_error(
      "pb_start 需要 Intel Pin 的 JIT 注入，本桥接层不提供；"
      "请改用 pb_attach 附加到已运行的进程（见 docs/roadmap.md 的 M1）");
  return PB_STATUS_NOT_SUPPORTED;
}

PB_EXPORT int32_t PB_CALL pb_attach(uint32_t pid) {
  int32_t status = PB_STATUS_OK;
  if (!require_agent(&status)) {
    return status;
  }
  return agent().attach(pid);
}

PB_EXPORT int32_t PB_CALL pb_set_capabilities(uint32_t bits) {
  int32_t status = PB_STATUS_OK;
  if (!require_agent(&status)) {
    return status;
  }
  return agent().set_capabilities(bits);
}

PB_EXPORT int32_t PB_CALL pb_continue(void) {
  int32_t status = PB_STATUS_OK;
  if (!require_agent(&status)) {
    return status;
  }
  return agent().resume();
}

PB_EXPORT int32_t PB_CALL pb_pause(void) {
  int32_t status = PB_STATUS_OK;
  if (!require_agent(&status)) {
    return status;
  }
  return agent().pause();
}

// 寄存器快照：Pin 工具侧在探针命中时捕获，交给 agent 保存（agent 自己抓不到
// 探针线程的上下文——它拿到的"当前线程"永远是为请求服务的那条线程）。
PB_EXPORT int32_t PB_CALL pb_set_thread_registers(uint32_t thread, const uint64_t* values,
                                                  uint32_t count) {
  if (values == nullptr) {
    set_error("pb_set_thread_registers: values 为空指针");
    return PB_STATUS_INVALID_ARGUMENT;
  }
  int32_t status = PB_STATUS_OK;
  if (!require_agent(&status)) {
    return status;
  }
  return agent().set_thread_registers(thread, values, count);
}

// 单步：与 `pb_pause` 同样的转发形状。它当前**明确未实现**（agent 侧返回
// NOT_SUPPORTED），但 Hub 必须能**够到**它——否则调用方拿到的是
// "符号缺失"，而那是"桥接层坏了"的语义，不是"这个能力还没做"。
// 采集覆盖：工具侧的探针上限/环形缓冲丢弃/跳过点这些**元信息**，
// 经 agent 写进轨迹的 AUDIT 文本通道。
//
// 为什么不走事件通道：工具的每线程环形缓冲满时，连这条记录自己都会被丢掉
// （第 35 轮实测 `溢出丢弃 10252 条`，轨迹里连 `state_change` 都没有）。
// 而 agent 是拥有文件、负责收尾的一方，在收尾前写一定进得去。
PB_EXPORT int32_t PB_CALL pb_set_coverage(uint64_t sites, uint64_t skipped, uint64_t unreadable,
                                          uint64_t filtered, uint32_t capped) {
  int32_t status = PB_STATUS_OK;
  if (!require_agent(&status)) {
    return status;
  }
  if (agent().set_coverage == nullptr) {
    // 老 agent：明确说"这个能力没有"，而不是静默成功。
    set_error("pb_set_coverage: agent 没有导出该符号（旧版 agent）");
    return PB_STATUS_NOT_SUPPORTED;
  }
  return agent().set_coverage(sites, skipped, unreadable, filtered, capped);
}

PB_EXPORT int32_t PB_CALL pb_single_step(uint32_t count) {
  int32_t status = PB_STATUS_OK;
  if (!require_agent(&status)) {
    return status;
  }
  return agent().single_step(count);
}

PB_EXPORT int32_t PB_CALL pb_start_trace(const char* path, uint32_t fold) {
  if (path == nullptr) {
    set_error("pb_start_trace: path 为空指针");
    return PB_STATUS_INVALID_ARGUMENT;
  }
  int32_t status = PB_STATUS_OK;
  if (!require_agent(&status)) {
    return status;
  }
  return agent().start_trace(path, fold);
}

PB_EXPORT int32_t PB_CALL pb_stop_trace(uint64_t* out_records, uint64_t* out_events) {
  if (out_records == nullptr || out_events == nullptr) {
    set_error("pb_stop_trace: out_records/out_events 为空指针");
    return PB_STATUS_INVALID_ARGUMENT;
  }
  int32_t status = PB_STATUS_OK;
  if (!require_agent(&status)) {
    return status;
  }
  return agent().stop_trace(out_records, out_events);
}

PB_EXPORT int32_t PB_CALL pb_drain_events(pb_event_raw* out, uint32_t capacity,
                                          uint32_t* written) {
  // 空指针是调用方的 bug，不是权限问题：先判空，再判加载状态。
  // 顺序反了会让排查方向完全走错（会去查 DLL，而真正的问题是传了 NULL）。
  if (out == nullptr || written == nullptr) {
    set_error("pb_drain_events: out/written 为空指针");
    return PB_STATUS_INVALID_ARGUMENT;
  }
  if (capacity == 0) {
    set_error("pb_drain_events: capacity 为 0");
    return PB_STATUS_INVALID_ARGUMENT;
  }
  int32_t status = PB_STATUS_OK;
  if (!require_agent(&status)) {
    *written = 0;
    return status;
  }
  return agent().drain_events(out, capacity, written);
}

/// 永不返回 NULL：加载失败时返回本层的错误文本，便于上层把"为什么不能用"
/// 直接展示给使用者（而不是只说一句"加载失败"）。
PB_EXPORT const char* PB_CALL pb_last_error(void) {
  if (g_error[0] != '\0') {
    return g_error;
  }
  const AgentApi& api = agent();
  if (api.usable() && api.last_error != nullptr) {
    return api.last_error();
  }
  return g_error;
}

// ═══════════════════════════════════════════════════════ 桥接层额外导出面
// **这两个不属于 Hub 的 tool ABI**（loader.rs 不要求它们）。
// 用途：在没有 Pin 探针的情况下做真实端到端联调与回放——
// 让事件从"探针层"进入 agent，再从 Hub 侧取回，全程走真实 ABI。
// Pin 到位后，这些调用由 pinbridge_tool.cpp 直接做，桥接层可以保持不变。

PB_EXPORT uint64_t PB_CALL pb_allocate_sequence(void) {
  int32_t status = PB_STATUS_OK;
  if (!require_agent(&status)) {
    return 0;
  }
  return agent().allocate_sequence();
}

PB_EXPORT int32_t PB_CALL pb_push_event(const pb_event_raw* raw) {
  if (raw == nullptr) {
    set_error("pb_push_event: raw 为空指针");
    return PB_STATUS_INVALID_ARGUMENT;
  }
  int32_t status = PB_STATUS_OK;
  if (!require_agent(&status)) {
    return status;
  }
  return agent().push_event(raw);
}

/// `which`: 0=丢弃数 1=积压 2=已取走（透传 agent 的统计）。
PB_EXPORT uint64_t PB_CALL pb_stats(uint32_t which) {
  int32_t status = PB_STATUS_OK;
  if (!require_agent(&status)) {
    return 0;
  }
  return agent().stats(which);
}

}  // extern "C"
