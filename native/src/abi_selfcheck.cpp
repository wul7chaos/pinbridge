// ABI 自检：把"跨语言结构体布局"这件事从运行期事故降级为编译期错误。
//
// 这个翻译单元不需要 Pin、不需要 Windows SDK，因此可以在任何 CI 上构建。
// 它同时验证三件事：
//   1. C 头文件里的结构体布局与预期一致；
//   2. 能力位与事件类型表的取值没有被人悄悄改动；
//   3. 状态码映射是双射（每个枚举值唯一）。

#include "pinbridge.h"

#include <array>
#include <cstddef>
#include <cstdint>

namespace {

// ---------------------------------------------------------------- 布局断言
static_assert(sizeof(pb_event_raw) == 88, "pb_event_raw 大小漂移");
static_assert(alignof(pb_event_raw) == 8, "pb_event_raw 对齐漂移");
static_assert(offsetof(pb_event_raw, sequence) == 0, "sequence 偏移漂移");
static_assert(offsetof(pb_event_raw, kind) == 8, "kind 偏移漂移");
static_assert(offsetof(pb_event_raw, reserved) == 9, "reserved 偏移漂移");
static_assert(offsetof(pb_event_raw, thread_id) == 12, "thread_id 偏移漂移");
static_assert(offsetof(pb_event_raw, address) == 16, "address 偏移漂移");
static_assert(offsetof(pb_event_raw, args) == 24, "args 偏移漂移");
static_assert(sizeof(((pb_event_raw*)nullptr)->args) == 64, "args 长度漂移");

// ------------------------------------------------------------ 能力位断言
// 位序来自 Rust 侧 Capability::ALL 的下标。改动任何一条都会让
// "AI 以为自己有 mem_read、实际拿到 mem_write" 这类事故成为可能。
static_assert(PB_CAP_MEM_READ == 1u, "mem_read 位漂移");
static_assert(PB_CAP_MEM_WRITE == 2u, "mem_write 位漂移");
static_assert(PB_CAP_BREAKPOINT == 8u, "breakpoint 位漂移");
static_assert(PB_CAP_TRACE_RECORD == 256u, "trace_record 位漂移");
static_assert(PB_CAP_PROCESS_CONTROL == 512u, "process_control 位漂移");
static_assert(PB_CAP_SCRIPT_LOAD == 1024u, "script_load 位漂移");
static_assert(PB_CAP_CAPABILITY_GRANT == 2048u, "capability_grant 位漂移");

// 只读基线 = mem_read | trace_record。这个组合是安全的默认值，
// 一旦被改成包含写能力，"AI 默认最小权限"这条设计承诺就失效了。
static_assert((PB_CAP_MEM_READ | PB_CAP_TRACE_RECORD) == 257u, "只读基线漂移");

// ------------------------------------------------------------ 事件类型断言
static_assert(PB_EV_RESERVED == 0, "reserved 必须是 0");
static_assert(PB_EV_BREAKPOINT_HIT == 1, "breakpoint_hit 判别值漂移");
static_assert(PB_EV_SYSCALL_ENTER == 3, "syscall_enter 判别值漂移");
static_assert(PB_EV_MEM_WRITE == 6, "mem_write 判别值漂移");
static_assert(PB_EV_REPEAT == 12, "repeat 判别值漂移（.pbt 文件里会出现它）");

// ---------------------------------------------------------------- 运行期检查

/// 检查状态码集合是"从 0 向下的连续区间"且无重复。
bool status_codes_are_dense() {
  const std::array<int, 9> codes = {
      PB_STATUS_OK,           PB_STATUS_INVALID_ARGUMENT, PB_STATUS_NOT_ATTACHED,
      PB_STATUS_CAPABILITY_DENIED, PB_STATUS_NOT_SUPPORTED, PB_STATUS_BUSY,
      PB_STATUS_TIMEOUT,      PB_STATUS_INTERNAL,         PB_STATUS_ABI_MISMATCH};
  for (std::size_t index = 0; index < codes.size(); ++index) {
    if (codes[index] != -static_cast<int>(index)) {
      return false;
    }
  }
  return true;
}

/// 检查事件类型表没有空洞（0..=12 连续）。
bool event_kinds_are_dense() {
  const std::array<int, 13> kinds = {
      PB_EV_RESERVED,      PB_EV_BREAKPOINT_HIT, PB_EV_EXCEPTION,
      PB_EV_SYSCALL_ENTER, PB_EV_SYSCALL_EXIT,   PB_EV_PATTERN_HIT,
      PB_EV_MEM_WRITE,     PB_EV_MEM_READ,       PB_EV_REG_WRITE,
      PB_EV_TRAP_FLAG,     PB_EV_STATE_CHANGE,   PB_EV_SCRIPT_OUTPUT,
      PB_EV_REPEAT};
  for (std::size_t index = 0; index < kinds.size(); ++index) {
    if (kinds[index] != static_cast<int>(index)) {
      return false;
    }
  }
  return true;
}

}  // namespace

/// 供 CTest 调用的入口。返回 0 表示全部通过。
extern "C" int pinbridge_abi_selfcheck(void) {
  if (!status_codes_are_dense()) {
    return 1;
  }
  if (!event_kinds_are_dense()) {
    return 2;
  }
  return 0;
}
