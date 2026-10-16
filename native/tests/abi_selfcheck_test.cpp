// CTest 驱动：把 `pinbridge_abi_selfcheck()` 的结果变成进程退出码。
//
// 这个测试**不需要 Pin、不需要目标程序**，因此它能在任何 CI runner 上跑，
// 也就意味着"C ABI 被人改坏"这件事永远会在合并前被发现。

#include <cstdio>

extern "C" int pinbridge_abi_selfcheck(void);

int main() {
  const int result = pinbridge_abi_selfcheck();
  switch (result) {
    case 0:
      std::puts("abi selfcheck: ok（布局 / 能力位 / 事件类型表 全部一致）");
      return 0;
    case 1:
      std::fputs("abi selfcheck: 状态码不再是 0..-8 的连续区间\n", stderr);
      return 1;
    case 2:
      std::fputs("abi selfcheck: 事件类型判别值出现空洞\n", stderr);
      return 2;
    default:
      std::fprintf(stderr, "abi selfcheck: 未知结果 %d\n", result);
      return 3;
  }
}
