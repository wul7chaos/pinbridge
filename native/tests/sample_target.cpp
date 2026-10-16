// 被插桩的最小目标程序。
//
// 它的存在只为一件事：让"Pin tool 真的工作"这件事**可执行地**被验证。
// 内容刻意简单——一个函数调用、一次输出、正常退出——这样任何异常行为
// （没跑起来、卡住、事件没出来）都能立刻归因到插桩那一侧。
//
// 不作为产品的一部分，也不链接任何 PinBridge 代码。

#include <cstdio>

namespace {

// 让编译器不要把它优化掉：入口点探针要的就是"执行流确实到了这里"。
volatile int g_sink = 0;

int add(int left, int right) {
  g_sink = left + right;
  return g_sink;
}

}  // namespace

int main() {
  const int value = add(20, 22);
  std::printf("pb_sample_target: %d\n", value);
  return value == 42 ? 0 : 1;
}
