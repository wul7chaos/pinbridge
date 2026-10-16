// 被插桩的最小目标程序。
//
// 它的存在只为一件事：让"Pin tool 真的工作"这件事**可执行地**被验证。
// 内容刻意简单，而且每一档都对应一个可判定的问题：
//
//   （无参数）     启动 → 打印一行 → 退出。用于验证**入口点探针**。
//   --syscalls N   做 N 次真实系统调用（Sleep(0) → NtDelayExecution）。
//                  用于验证**系统调用探针**分辨得出"发生了系统调用"。
//   --spin N       纯用户态循环 N 次，**一次系统调用都不做**。
//                  这是**反向对照**：如果 --spin 也产出系统调用事件，
//                  那说明探针在乱命中，"捕获到事件"就不再是证据。
//
// 靶子一律自写：不拿系统二进制或外部样本。否则一次"测试失败"会同时包含
// "我们的工具坏了"与"那个二进制有反制"，两者必须能分开归因。
//
//   --agent-check <agent.dll> [<script.py>]
//                 在**进程内**直接驱动 agent：加载 DLL → 设能力 → 加载脚本
//                 → 推一条系统调用事件 → 取事件（脚本应当产出 script_output）
//                 → 下断点 → 触发 → 取事件（应当产出 breakpoint_hit）
//                 → 打印一行 `PB_AGENT_CHECK ...` 供 ctest 断言。
//
//                 这条模式把 roadmap 里"真实内存读写/断点/脚本在目标内跑通"
//                 从**手工验证清单**变成一条机器可判定的用例：整个进程是
//                 **被 Pin 插装着**跑的，所以验证的是"在真实插装环境里 agent
//                 仍然工作"，而不只是"agent 单独跑得通"。
//
//                 没装 Python 时打印 `PB_AGENT_CHECK SKIP no-python` 并以 77 退出
//                 （ctest 侧按 SKIP_RETURN_CODE 处理）：**跳过要说清原因**，
//                 不能和"实现坏了"混成一个失败。

#include <cstdio>
#include <cstdlib>
#include <intrin.h>
#include <windows.h>
#include <cstring>
#include <string>

#include "pinbridge.h"

#ifdef _WIN32
// 只声明用到的两个 Win32 入口，不引入 windows.h：
// Pin 的 CRT 里有一份部分重实现的 minwindef.h，与真实 windows.h 同处一个
// 编译单元会报"BOOL 不明确的符号"（见 third_party/pin/README.md）。
extern "C" {
__declspec(dllimport) void __stdcall Sleep(unsigned long milliseconds);
}
#endif

namespace {

// 让编译器不要把它优化掉：入口点探针要的就是"执行流确实到了这里"。
volatile int g_sink = 0;

int add(int left, int right) {
  g_sink = left + right;
  return g_sink;
}

/// 纯用户态工作：N 次循环，不产生任何系统调用。
void spin(unsigned long iterations) {
  volatile unsigned long accumulator = 0;
  for (unsigned long i = 0; i < iterations; ++i) {
    accumulator += i;
  }
  g_sink = static_cast<int>(accumulator & 0x7FFFFFFF);
}

/// N 次真实系统调用。`Sleep(0)` 在 Windows 上仍然是 `NtDelayExecution`。
void syscall_loop(unsigned long iterations) {
#ifndef _WIN32
  (void)iterations;
  std::fputs("pb_sample_target: --syscalls 只在 Windows 上实现\n", stderr);
#else
  for (unsigned long i = 0; i < iterations; ++i) {
    Sleep(0);
  }
#endif
}

/// 一个模块内的全局缓冲区：内存探针要观察的"目标地址"就在这里。
/// 刻意做成固定大小 + 固定访问模式，让"地址落在 [base, base+size)"这件事可判定。
constexpr unsigned long kTouchBufferBytes = 256;
alignas(64) unsigned char g_touch_buffer[kTouchBufferBytes];

/// 对缓冲区做 N 次"写 + 读"。
///
/// 访问模式是刻意的：写与读落在同一个 256 字节窗口内，
/// 因此可以给探针一个**明确的地址范围**去过滤，从而把
/// "捕获到事件"与"捕获到我关心的那段内存"区分开。
void touch_loop(unsigned long iterations) {
  unsigned long checksum = 0;
  for (unsigned long i = 0; i < iterations; ++i) {
    const unsigned long index = i % kTouchBufferBytes;
    g_touch_buffer[index] = static_cast<unsigned char>(i & 0xFF);
    checksum += g_touch_buffer[(index + 7) % kTouchBufferBytes];
  }
  g_sink = static_cast<int>(checksum & 0x7FFFFFFF);
}

/// 一个**签名可寻址**的函数。
///
/// 函数体里的 8 字节魔数会作为立即数（`movabs`）出现在编译产物里，
/// 因此在**代码段**里能被字节模式找到——这正是"没有符号时怎么定位函数"
/// 的最小示例。
///
/// 模式（十六进制）**必须按小端写**：立即数在机器码里是低位字节在前，
/// 所以 `0x5042524944474531`（文本 "PBRIDGE1"）在代码段里是
///     31 45 47 44 49 52 42 50
/// 写反了会命中 0 处，而且看起来像"模板功能没生效"。
/// （这条是实测出来的：第一版按文本顺序写，扫描结果就是 0 个命中点。）
/// 用 `--print-buffer` 之外的排查手段：直接在 exe 里按字节搜。
///
/// 之所以让靶子自己带这个魔数：测试要能**精确**断言"调用了 N 次就有 N 次命中"，
/// 而不是"看到了一些 pattern_hit"。
volatile unsigned long long g_pattern_sink = 0;

__declspec(noinline) void pattern_demo_function(unsigned long long value) {
  const unsigned long long magic = 0x5042524944474531ULL;  // "PBRIDGE1"
  g_pattern_sink = value ^ magic;
}

/// 调用上面那个函数 N 次。
void pattern_loop(unsigned long iterations) {
  for (unsigned long i = 0; i < iterations; ++i) {
    pattern_demo_function(i);
  }
}

/// TF（EFLAGS 位 8）单步实验。
///
/// 目标的做法：置位 TF → 执行一条语句 → 若被单步则进 `__except` → 读回 flags。
/// 两个计数就是判据：
///   real_steps：**真的**被单步打断了多少次（SEH 捕获 STATUS_SINGLE_STEP）
///   tf_reads ：读回的 flags 里 TF 处于置位的次数
///
/// 为什么用 `__writeeflags`/`__readeflags` 而不是内联汇编：
/// MSVC x64 不支持内联汇编。而这两个内建函数正好编译成 `popfq` / `pushfq`
/// （用 dumpbin 确认过），所以它们走的正是 TF 虚拟化要拦截的那条路径。
///
/// 期望（判据见 CMakeLists 里的两个用例）：
///   单独跑            ：real_steps=N、tf_reads=0（内核在派发异常时清掉了 TF）
///   -pb_tf watch      ：real_steps=N、tf_reads=0，工具侧看到 N 次置位意图
///   -pb_tf virtualize ：real_steps=0、tf_reads=N ← **目标确信自己在单步，其实没有**
volatile unsigned long long g_tf_sink = 0;

__declspec(noinline) unsigned long tf_experiment(unsigned long iterations,
                                                unsigned long* real_steps) {
  unsigned long steps = 0;
  unsigned long reads_with_tf = 0;
  for (unsigned long i = 0; i < iterations; ++i) {
    const unsigned long long flags = __readeflags();
    __writeeflags(flags | 0x100ULL);  // 置位 TF（编译成 popfq）
    __try {
      // TF 置位后，这里的第一条指令会触发 STATUS_SINGLE_STEP。
      g_tf_sink += 1;
    } __except (GetExceptionCode() == EXCEPTION_SINGLE_STEP ? EXCEPTION_EXECUTE_HANDLER
                                                             : EXCEPTION_CONTINUE_SEARCH) {
      ++steps;
    }
    if ((__readeflags() & 0x100ULL) != 0) {  // 读回（编译成 pushfq）
      ++reads_with_tf;
    }
    __writeeflags(__readeflags() & ~0x100ULL);  // 收尾清掉，避免污染后续逻辑
  }
  *real_steps = steps;
  return reads_with_tf;
}

// ---------------------------------------------------------------- agent 检查
//
// 在被插装的进程里直接驱动 agent。刻意用**运行期加载 + 取符号**而不是链接导入库：
// 靶子必须在"没有 agent DLL"的环境里也能编出来、跑起来（否则这条用例会把
// 构建配置本身变成前提）。也没链接任何 Rust 侧静态库——跨语言边界只过 C ABI。

struct AgentApi {
  HMODULE dll = nullptr;
  int32_t(__cdecl* attach)(uint32_t) = nullptr;
  int32_t(__cdecl* set_caps)(uint32_t) = nullptr;
  int32_t(__cdecl* load_script)(const char*, const char*) = nullptr;
  uint64_t(__cdecl* allocate_sequence)() = nullptr;
  int32_t(__cdecl* push_event)(const pb_event_raw*) = nullptr;
  int32_t(__cdecl* drain_events)(pb_event_raw*, uint32_t, uint32_t*) = nullptr;
  int32_t(__cdecl* memory_read)(uint64_t, uint32_t, uint8_t*, uint32_t, uint32_t*) = nullptr;
  int32_t(__cdecl* memory_write)(uint64_t, const uint8_t*, uint32_t) = nullptr;
  int32_t(__cdecl* set_breakpoint)(uint64_t) = nullptr;
  int32_t(__cdecl* clear_breakpoint)(uint64_t) = nullptr;
  int32_t(__cdecl* read_registers)(uint32_t, uint64_t*, uint32_t, uint32_t*) = nullptr;
  int32_t(__cdecl* thread_ids_with_registers)(uint32_t*, uint32_t, uint32_t*) = nullptr;
  const char* (__cdecl* last_error)() = nullptr;
};

/// 取一个导出符号；缺失时打印**是哪个**并返回 false。
template <typename T>
bool resolve(HMODULE dll, const char* name, T* out) {
  *out = reinterpret_cast<T>(GetProcAddress(dll, name));
  if (*out == nullptr) {
    std::printf("PB_AGENT_CHECK FAIL missing-export %s\n", name);
    return false;
  }
  return true;
}

/// 会被下断点的函数（`--agent-check` 用）。
/// `volatile` 让编译器不能把它优化成常量返回，否则"这个函数的第一个字节"就不存在了。
volatile int g_agent_check_sink = 0;
int __declspec(noinline) agent_check_target(int value) {
  g_agent_check_sink = value * 3 + 1;
  return g_agent_check_sink;
}

int agent_check(const char* dll_path, const char* script_path) {
  AgentApi api;
  api.dll = LoadLibraryA(dll_path);
  if (api.dll == nullptr) {
    std::printf("PB_AGENT_CHECK FAIL load-agent %s\n", dll_path);
    return 1;
  }
  if (!resolve(api.dll, "pb_attach", &api.attach) ||
      !resolve(api.dll, "pb_set_capabilities", &api.set_caps) ||
      !resolve(api.dll, "pb_load_script", &api.load_script) ||
      !resolve(api.dll, "pb_allocate_sequence", &api.allocate_sequence) ||
      !resolve(api.dll, "pb_push_event", &api.push_event) ||
      !resolve(api.dll, "pb_drain_events", &api.drain_events) ||
      !resolve(api.dll, "pb_memory_read", &api.memory_read) ||
      !resolve(api.dll, "pb_memory_write", &api.memory_write) ||
      !resolve(api.dll, "pb_set_breakpoint", &api.set_breakpoint) ||
      !resolve(api.dll, "pb_clear_breakpoint", &api.clear_breakpoint) ||
      !resolve(api.dll, "pb_read_registers", &api.read_registers) ||
      !resolve(api.dll, "pb_thread_ids_with_registers", &api.thread_ids_with_registers) ||
      !resolve(api.dll, "pb_last_error", &api.last_error)) {
    return 1;
  }

  // 1) 附加 + 给足能力（`0xFFF` = 12 位全开，见 capability.rs）
  if (api.attach(GetCurrentProcessId()) != PB_STATUS_OK) {
    std::printf("PB_AGENT_CHECK FAIL attach %s\n", api.last_error());
    return 1;
  }
  if (api.set_caps(0xFFFu) != PB_STATUS_OK) {
    std::printf("PB_AGENT_CHECK FAIL set-caps %s\n", api.last_error());
    return 1;
  }

  // 2) 加载脚本。没有 Python 的环境要给**明确的跳过**，不是失败。
  std::FILE* file = std::fopen(script_path, "rb");
  if (file == nullptr) {
    std::printf("PB_AGENT_CHECK FAIL open-script %s\n", script_path);
    return 1;
  }
  std::fseek(file, 0, SEEK_END);
  const long size = std::ftell(file);
  std::fseek(file, 0, SEEK_SET);
  std::string source;
  source.resize(static_cast<size_t>(size));
  if (size > 0) {
    std::fread(&source[0], 1, static_cast<size_t>(size), file);
  }
  std::fclose(file);

  const int32_t loaded = api.load_script("agent-check", source.c_str());
  if (loaded == PB_STATUS_NOT_SUPPORTED) {
    // 原因必须带出来：跳过与"实现坏了"是两件事。
    std::printf("PB_AGENT_CHECK SKIP no-python %s\n", api.last_error());
    return 77;
  }
  if (loaded != PB_STATUS_OK) {
    std::printf("PB_AGENT_CHECK FAIL load-script %s\n", api.last_error());
    return 1;
  }

  // 3) 推一条系统调用进入事件；脚本应当把它变成一条 script_output
  pb_event_raw event = {};
  event.sequence = api.allocate_sequence();
  event.kind = PB_EV_SYSCALL_ENTER;
  event.thread_id = GetCurrentThreadId();
  event.address = 0x1000;
  event.args[0] = 0x50;  // 约定的系统调用号
  if (api.push_event(&event) != PB_STATUS_OK) {
    std::printf("PB_AGENT_CHECK FAIL push %s\n", api.last_error());
    return 1;
  }

  // 4) 真实内存读写：写进去、读回来、逐字节核对。
  //    在插装环境里做这一步是有意义的——它与"我们的内存读走到了
  //    ReadProcessMemory、而不是解引用"是同一件事的验证。
  unsigned memory_round_trip = 0;
  {
    static const unsigned char kMagic[8] = {0x50, 0x42, 0x4D, 0x45, 0x4D, 0x58,
                                            0x59, 0x5A};  // "PBMEMXYZ"
    static unsigned char buffer[16] = {};
    const uint64_t address = reinterpret_cast<uint64_t>(buffer);
    if (api.memory_write(address, kMagic, sizeof(kMagic)) != PB_STATUS_OK) {
      std::printf("PB_AGENT_CHECK FAIL memory-write %s\n", api.last_error());
      return 1;
    }
    unsigned char read_back[8] = {};
    uint32_t written = 0;
    if (api.memory_read(address, sizeof(read_back), read_back, sizeof(read_back),
                        &written) != PB_STATUS_OK) {
      std::printf("PB_AGENT_CHECK FAIL memory-read %s\n", api.last_error());
      return 1;
    }
    if (written != sizeof(read_back) || std::memcmp(read_back, kMagic, sizeof(kMagic)) != 0) {
      std::printf("PB_AGENT_CHECK FAIL memory-mismatch written=%u\n", written);
      return 1;
    }
    memory_round_trip = 1;
  }

  // 5) 下断点 → 触发 → 应当产出 breakpoint_hit
  const uint64_t target =
      reinterpret_cast<uint64_t>(reinterpret_cast<void*>(&agent_check_target));
  if (api.set_breakpoint(target) != PB_STATUS_OK) {
    std::printf("PB_AGENT_CHECK FAIL set-breakpoint %s\n", api.last_error());
    return 1;
  }
  const int value = agent_check_target(7);
  if (value != 22) {
    // 命中之后函数**必须仍然正常执行完**（原字节要写回去）。
    std::printf("PB_AGENT_CHECK FAIL target-after-hit value=%d\n", value);
    return 1;
  }

  // 6) 取事件，统计两类派生事件
  unsigned script_outputs = 0;
  unsigned breakpoint_hits = 0;
  unsigned script_marked_ssn = 0;
  // 取事件直到**看见自己的那几条**为止。
  //
  // 队列是进程级共享的：Pin 工具侧的探针也在往同一个 agent 实例里推事件
  // （这条用例现在共享一个实例，见 CMakeLists 里的 PINBRIDGE_AGENT_DLL），
  // 所以"取 8 批"这种固定循环会被海量探针事件淹没，断言会以"什么都没取到"
  // 的形式失败——而真正的原因只是"排在我们前面的还有几千条"。
  for (int round = 0; round < 4096; ++round) {
    pb_event_raw batch[64];
    uint32_t written = 0;
    if (api.drain_events(batch, 64, &written) != PB_STATUS_OK) {
      std::printf("PB_AGENT_CHECK FAIL drain %s\n", api.last_error());
      return 1;
    }
    for (uint32_t i = 0; i < written; ++i) {
      if (batch[i].kind == PB_EV_SCRIPT_OUTPUT) {
        ++script_outputs;
        // 脚本用 `pb.mark("seen", ev.args[0])` 报回来的值：类别 2 = mark
        if (batch[i].args[0] == 2 && batch[i].args[1] == 0x50) {
          ++script_marked_ssn;
        }
      } else if (batch[i].kind == PB_EV_BREAKPOINT_HIT && batch[i].address == target) {
        ++breakpoint_hits;
      }
    }
    if (written == 0) {
      break;
    }
  }
  api.clear_breakpoint(target);

  // 7) 寄存器快照：Pin 工具侧在探针命中时捕获，agent 存下来。
  //
  //    这一步目前是**诊断**而不是断言：捕获段（工具侧）尚未接通，
  //    `registers` 会是 0。把它当诊断打印出来的价值在于——**把最后一公里
  //    量化**：一旦捕获接通，这一行的数字会自己变成非零，而"0 条快照"
  //    与"快照全是零"在摘要里是两种不同的证据（见 docs/design/script-runtime.md）。
  //    RIP 落在**本进程主模块**里才计数（只有真抓到才可能成立）。
  unsigned registers_ok = 0;
  uint64_t first_rip = 0;
  {
    uint32_t threads[16];
    uint32_t thread_count = 0;
    if (api.thread_ids_with_registers(threads, 16, &thread_count) != PB_STATUS_OK) {
      std::printf("PB_AGENT_CHECK FAIL thread-ids %s\n", api.last_error());
      return 1;
    }
    const uintptr_t module = reinterpret_cast<uintptr_t>(GetModuleHandleA(nullptr));
    // 诊断：直接问 thread 0（Pin 的入口探针就在这条线程上）并打印 agent 的说法
    {
      uint64_t probe[8] = {};
      uint32_t probe_written = 0;
      const int32_t code = api.read_registers(0, probe, 8, &probe_written);
      std::printf("PB_AGENT_CHECK DIAG thread_count=%u read0=%d rip0=0x%llX err=%s\n",
                  thread_count, code, static_cast<unsigned long long>(probe[0]),
                  code == PB_STATUS_OK ? "" : api.last_error());
    }
    for (uint32_t i = 0; i < thread_count; ++i) {
      uint64_t values[8] = {};
      uint32_t written = 0;
      if (api.read_registers(threads[i], values, 8, &written) != PB_STATUS_OK) {
        std::printf("PB_AGENT_CHECK FAIL read-registers %s\n", api.last_error());
        return 1;
      }
      if (written != 8) {
        std::printf("PB_AGENT_CHECK FAIL register-width written=%u\n", written);
        return 1;
      }
      // 主模块镜像只有几个页；RIP 落在里面说明"这是本进程、且在目标代码里"。
      const uintptr_t rip = static_cast<uintptr_t>(values[0]);
      if (rip >= module && rip < module + 0x100000u) {
        ++registers_ok;
        first_rip = values[0];
      }
    }
  }

  // 一行摘要，供 ctest 用正则断言。数字都打印出来，失败时不用再猜。
  std::printf(
      "PB_AGENT_CHECK script_output=%u ssn_mark=%u breakpoint_hit=%u memory=%u registers=%u "
      "rip=0x%llX target_value=%d\n",
      script_outputs, script_marked_ssn, breakpoint_hits, memory_round_trip, registers_ok,
      static_cast<unsigned long long>(first_rip), value);
  if (script_outputs == 0 || script_marked_ssn == 0 || breakpoint_hits == 0 ||
      memory_round_trip == 0) {
    std::printf("PB_AGENT_CHECK FAIL incomplete\n");
    return 1;
  }
  return 0;
}
/// 把缓冲区地址打印出来：脚本据此构造 `-pb_mem_range`。
void print_touch_buffer() {
  std::printf("PB_TOUCH_BUFFER=0x%p size=0x%lX\n",
              static_cast<const void*>(g_touch_buffer), kTouchBufferBytes);
}

}  // namespace

int main(int argc, char** argv) {
  unsigned long syscalls = 0;
  unsigned long spins = 0;
  unsigned long touches = 0;
  unsigned long pattern_hits = 0;
  unsigned long tf_sets = 0;
  bool print_buffer = false;
  const char* agent_dll = nullptr;
  const char* agent_script = nullptr;
  for (int i = 1; i < argc; ++i) {
    if (std::strcmp(argv[i], "--syscalls") == 0 && i + 1 < argc) {
      syscalls = std::strtoul(argv[++i], nullptr, 10);
    } else if (std::strcmp(argv[i], "--spin") == 0 && i + 1 < argc) {
      spins = std::strtoul(argv[++i], nullptr, 10);
    } else if (std::strcmp(argv[i], "--touch") == 0 && i + 1 < argc) {
      touches = std::strtoul(argv[++i], nullptr, 10);
    } else if (std::strcmp(argv[i], "--pattern-hits") == 0 && i + 1 < argc) {
      pattern_hits = std::strtoul(argv[++i], nullptr, 10);
    } else if (std::strcmp(argv[i], "--tf-set") == 0 && i + 1 < argc) {
      tf_sets = std::strtoul(argv[++i], nullptr, 10);
    } else if (std::strcmp(argv[i], "--print-buffer") == 0) {
      print_buffer = true;
    } else if (std::strcmp(argv[i], "--agent-check") == 0 && i + 2 < argc) {
      agent_dll = argv[++i];
      agent_script = argv[++i];
    }
  }

  if (agent_dll != nullptr) {
    return agent_check(agent_dll, agent_script);
  }
  if (syscalls != 0) {
    syscall_loop(syscalls);
  }
  if (spins != 0) {
    spin(spins);
  }
  if (touches != 0) {
    touch_loop(touches);
  }
  if (pattern_hits != 0) {
    pattern_loop(pattern_hits);
  }
  if (tf_sets != 0) {
    unsigned long steps = 0;
    const unsigned long reads_with_tf = tf_experiment(tf_sets, &steps);
    std::printf("PB_TF_RESULT real_steps=%lu tf_reads=%lu\n", steps, reads_with_tf);
  }
  if (print_buffer) {
    print_touch_buffer();
  }

  const int value = add(20, 22);
  std::printf("pb_sample_target: %d\n", value);
  return value == 42 ? 0 : 1;
}
