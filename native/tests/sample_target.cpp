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

}  // namespace

int main(int argc, char** argv) {
  unsigned long syscalls = 0;
  unsigned long spins = 0;
  for (int i = 1; i < argc; ++i) {
    if (std::strcmp(argv[i], "--syscalls") == 0 && i + 1 < argc) {
      syscalls = std::strtoul(argv[++i], nullptr, 10);
    } else if (std::strcmp(argv[i], "--spin") == 0 && i + 1 < argc) {
      spins = std::strtoul(argv[++i], nullptr, 10);
    }
  }

  if (syscalls != 0) {
    syscall_loop(syscalls);
  }
  if (spins != 0) {
    spin(spins);
  }

  const int value = add(20, 22);
  std::printf("pb_sample_target: %d\n", value);
  return value == 42 ? 0 : 1;
}
