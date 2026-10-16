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

#include <cstdio>
#include <cstdlib>
#include <intrin.h>
#include <windows.h>
#include <cstring>

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
    }
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
