/*=============================================================================
 * pinbridge.h —— PinBridge 的稳定 C ABI
 *
 * 这个文件是 Rust 侧 `crates/pinbridge-sys/src/abi.rs` 的 C 孪生体。
 * 两份必须逐字段一致；改动规则见 docs/specs/c-abi.md：
 *   1. 同时改两侧；2. 提升 PB_ABI_VERSION；3. 更新规范文档与测试。
 *
 * 设计约束（不要在这里加"更优雅"的东西）：
 *   * 只允许定长整数、指针 + 长度、定长数组、repr(C) 结构体；
 *   * 不允许 bool / 变长结构 / 回调里跨越调用边界存活的对象；
 *   * 不允许从 C++ 抛异常穿过边界；Rust 侧不允许 panic 穿过边界；
 *   * 所有字符串是 UTF-8 + NUL 结尾，所有权归调用方。
 *
 * 谁调用谁：
 *   pin.exe
 *     └─ pinbridge.dll（C++ / Pin tool；探针、JIT 上下文、TF 虚拟化）
 *           └─ 按本头文件调用 pinbridge_agentdll.dll（Rust，crates/pinbridge-agent）
 *===========================================================================*/

#ifndef PINBRIDGE_H
#define PINBRIDGE_H

#include <stddef.h> /* offsetof（下面的 static_assert 用到） */
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#if defined(_WIN32)
#  define PB_CALL __cdecl
   /* PB_EXPORT 的三态：实现方导出、使用方导入。
    * PINBRIDGE_BRIDGE_BUILD 是给 native/src/agent_bridge.cpp 用的——
    * 它自己也导出同名符号（把 Hub 的调用转发给 agent），
    * 因此不能按“导入”处理。 */
#  if defined(PINBRIDGE_AGENT_BUILD) || defined(PINBRIDGE_BRIDGE_BUILD) || \
      defined(PINBRIDGE_TOOL_BUILD)
#    define PB_EXPORT __declspec(dllexport)
#  else
#    define PB_EXPORT __declspec(dllimport)
#  endif
#else
#  define PB_CALL
#  define PB_EXPORT __attribute__((visibility("default")))
#endif

/*--------------------------------------------------------------- 版本与状态 */

/* ABI 版本。与 Rust 侧 PB_ABI_VERSION 必须相同；不匹配时加载器拒绝加载。 */
#define PB_ABI_VERSION 1u

/* 单条事件的参数个数。与 Rust 侧 MAX_ARGS 必须相同。 */
#define PB_MAX_ARGS 8u

/* 状态码。数值一旦发布不可更改（C 侧可能已按数值分支）。 */
typedef enum pb_status {
  PB_STATUS_OK                =  0,
  PB_STATUS_INVALID_ARGUMENT  = -1,
  PB_STATUS_NOT_ATTACHED      = -2,
  PB_STATUS_CAPABILITY_DENIED = -3,
  PB_STATUS_NOT_SUPPORTED     = -4,
  PB_STATUS_BUSY              = -5,
  PB_STATUS_TIMEOUT           = -6,
  PB_STATUS_INTERNAL          = -7,
  PB_STATUS_ABI_MISMATCH      = -8
} pb_status;

/* 能力位。与 Rust 侧 `Capability::bit()` 一一对应。
 * 注意：位序来自 Capability::ALL 的下标，不是枚举顺序，改动即为破坏性变更。 */
#define PB_CAP_MEM_READ           (1u << 0)
#define PB_CAP_MEM_WRITE          (1u << 1)
#define PB_CAP_REG_WRITE          (1u << 2)
#define PB_CAP_BREAKPOINT         (1u << 3)
#define PB_CAP_SINGLE_STEP        (1u << 4)
#define PB_CAP_SYSCALL_HOOK       (1u << 5)
#define PB_CAP_PATTERN_HOOK       (1u << 6)
#define PB_CAP_EXCEPTION_STEER    (1u << 7)
#define PB_CAP_TRACE_RECORD       (1u << 8)
#define PB_CAP_PROCESS_CONTROL    (1u << 9)
#define PB_CAP_SCRIPT_LOAD        (1u << 10)
#define PB_CAP_CAPABILITY_GRANT   (1u << 11)

/* 事件类型判别值。与 Rust 侧 EventKind、Python 侧 EventKind 必须相同。
 * 数值写入 .pbt 文件，**不得重排**；新增只能追加。 */
typedef enum pb_event_kind {
  PB_EV_RESERVED       = 0,
  PB_EV_BREAKPOINT_HIT = 1,
  PB_EV_EXCEPTION      = 2,
  PB_EV_SYSCALL_ENTER  = 3,
  PB_EV_SYSCALL_EXIT   = 4,
  PB_EV_PATTERN_HIT    = 5,
  PB_EV_MEM_WRITE      = 6,
  PB_EV_MEM_READ       = 7,
  PB_EV_REG_WRITE      = 8,
  PB_EV_TRAP_FLAG      = 9,
  PB_EV_STATE_CHANGE   = 10,
  PB_EV_SCRIPT_OUTPUT  = 11,
  PB_EV_REPEAT         = 12
} pb_event_kind;

/* 一条事件的 C 布局。总长 88 字节、8 字节对齐：
 *   sequence(8) + kind(1) + reserved(3, 显式填充) + thread_id(4)
 *   + address(8) + args[8](64)
 * 刻意显式填充：不同编译器对隐式填充的处理在跨语言边界上并不可靠。 */
typedef struct pb_event_raw {
  uint64_t sequence;
  uint8_t  kind;
  uint8_t  reserved[3];
  uint32_t thread_id;
  uint64_t address;
  uint64_t args[PB_MAX_ARGS];
} pb_event_raw;

/*------------------------------------------------------------ agent 导出面 */
/* 以下函数由 pinbridge_agentdll.dll（Rust）导出，由 pinbridge.dll 调用。
 * 全部返回 pb_status（0 为成功）；失败时用 pb_last_error 取可读信息。 */

PB_EXPORT uint32_t PB_CALL pb_abi_version(void);

PB_EXPORT int32_t  PB_CALL pb_attach(uint32_t pid);
PB_EXPORT int32_t  PB_CALL pb_detach(void);
PB_EXPORT int32_t  PB_CALL pb_is_attached(void);

/* 能力集是"绝对设定"：C++ 侧是唯一真相来源。
 * 忘记下发会表现为 agent 拒绝一切操作——这是刻意选择的失败方向（fail-closed）。 */
PB_EXPORT int32_t  PB_CALL pb_set_capabilities(uint32_t bits);
PB_EXPORT uint32_t PB_CALL pb_get_capabilities(void);

/* 返回的指针在**下一次写入错误状态之前**有效，调用方必须立刻拷贝。 */
PB_EXPORT const char* PB_CALL pb_last_error(void);

/* 热路径：探针命中时调用。pinbridge.dll 侧应保证不在锁内做 I/O。 */
PB_EXPORT uint64_t PB_CALL pb_allocate_sequence(void);
PB_EXPORT int32_t  PB_CALL pb_push_event(const pb_event_raw* raw);
PB_EXPORT int32_t  PB_CALL pb_drain_events(pb_event_raw* out, uint32_t capacity,
                                           uint32_t* written);

/* which: 0=丢弃数 1=积压 2=已取走 */
PB_EXPORT uint64_t PB_CALL pb_stats(uint32_t which);

/* 永不返回 NULL。 */
PB_EXPORT const char* PB_CALL pb_kind_name(uint32_t code);

/* --- 目标侧操作。M0 为桩实现：先校验能力，再返回 PB_STATUS_NOT_SUPPORTED。 --- */
PB_EXPORT int32_t PB_CALL pb_memory_read(uint64_t address, uint32_t length,
                                         uint8_t* out, uint32_t out_capacity,
                                         uint32_t* out_written);
PB_EXPORT int32_t PB_CALL pb_memory_write(uint64_t address, const uint8_t* bytes,
                                          uint32_t length);
PB_EXPORT int32_t PB_CALL pb_set_breakpoint(uint64_t address);
PB_EXPORT int32_t PB_CALL pb_clear_breakpoint(uint64_t address);
PB_EXPORT int32_t PB_CALL pb_single_step(uint32_t count);  /* 桥接层也导出（转发给 agent） */
PB_EXPORT int32_t PB_CALL pb_continue(void);
PB_EXPORT int32_t PB_CALL pb_pause(void);
PB_EXPORT int32_t PB_CALL pb_load_script(const char* name, const char* source);
PB_EXPORT int32_t PB_CALL pb_start_trace(const char* path, uint32_t fold);
PB_EXPORT int32_t PB_CALL pb_stop_trace(uint64_t* out_records, uint64_t* out_events);

/*-------------------------------------------------------------- tool 导出面 */
/* 以下函数由 pinbridge.dll（C++）导出，由 Hub 侧通过 pinbridge-sys 运行时加载。
 * 签名与 crates/pinbridge-sys/src/loader.rs 里的函数指针表一一对应。 */

/* 重新声明 agent 侧签名，便于 C++ 实现里静态断言 */
typedef int32_t  (PB_CALL *pb_fn_attach)(uint32_t);
typedef int32_t  (PB_CALL *pb_fn_set_caps)(uint32_t);
typedef int32_t  (PB_CALL *pb_fn_drain)(pb_event_raw*, uint32_t, uint32_t*);

#ifdef __cplusplus
} /* extern "C" */

/* C++ 侧的编译期契约检查：布局一旦漂移就编译失败，而不是运行期崩。
 * 这类 static_assert 在跨语言项目里是最便宜的一道保险。 */
static_assert(sizeof(pb_event_raw) == 88, "pb_event_raw 布局变化：必须同步 Rust 侧 abi.rs");
static_assert(offsetof(pb_event_raw, args) == 24, "args 偏移变化：必须同步 Rust 侧 abi.rs");
static_assert(PB_ABI_VERSION == 1u, "ABI 版本变化：必须同步 Rust 侧与文档");
#endif

#endif /* PINBRIDGE_H */
