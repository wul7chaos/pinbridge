// 桥接层往返测试：验证"Hub 侧调用 → 桥接 DLL → agent DLL → 事件回来"整条链。
//
// 为什么这个测试值得写：在 Pin SDK 到位之前，我们只能证明"类型对得上"。
// 这个测试证明的是**函数真的被调到、状态码真的被翻译、事件真的走了两个
// 编译产物、两个语言边界**。它对应的验收标准是 docs/roadmap.md 的 M1：
// 「pinbridge-sys 加载器端到端」。
//
// 用法（由 CMake/CTest 传入，本地手工跑也一样）：
//   bridge_roundtrip_test <桥接DLL> <agentDLL> positive
//   bridge_roundtrip_test <桥接DLL> <不存在的DLL> missing
//
// 退出码：0 通过 / 2 断言失败 / 3 用法错误 / 77 跳过（agent DLL 缺失）

#include "pinbridge.h"

#include <windows.h>

#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <string>

namespace {

int g_failures = 0;

void check(bool condition, const char* what) {
  if (condition) {
    std::printf("  [ok]   %s\n", what);
    return;
  }
  std::printf("  [FAIL] %s\n", what);
  ++g_failures;
}

/// 把错误文本转成可读且**无歧义**的形式。
///
/// 为什么需要它：本项目所有错误文本都是 UTF-8 中文，而 Windows 控制台默认是
/// 代码页 936（GBK）。直接把 UTF-8 字节打到 GBK 控制台上会变成乱码，
/// 于是"内存悬垂"和"只是终端编码不对"在肉眼上完全一样——这会让排查方向跑偏。
/// 这里把非 ASCII 字节转义成 \xHH，两种情况立刻可区分。
std::string describe(const char* text) {
  if (text == nullptr) {
    return "(null)";
  }
  std::string out;
  char escape[8];
  for (const unsigned char* cursor = reinterpret_cast<const unsigned char*>(text);
       *cursor != 0 && out.size() < 240; ++cursor) {
    if (*cursor >= 0x20 && *cursor < 0x7F) {
      out.push_back(static_cast<char>(*cursor));
    } else {
      (void)std::snprintf(escape, sizeof(escape), "\\x%02X", *cursor);
      out += escape;
    }
  }
  return out;
}

void check_status(int32_t actual, int32_t expected, const char* what, const char* last_error) {
  if (actual == expected) {
    std::printf("  [ok]   %s（%d）\n", what, static_cast<int>(actual));
    return;
  }
  std::printf("  [FAIL] %s：期望 %d，实际 %d；last_error=\"%s\"\n", what,
              static_cast<int>(expected), static_cast<int>(actual), describe(last_error).c_str());
  ++g_failures;
}

using fn_abi_version = uint32_t(PB_CALL*)();
using fn_start = int32_t(PB_CALL*)(const char*, const char*);
using fn_attach = int32_t(PB_CALL*)(uint32_t);
using fn_set_capabilities = int32_t(PB_CALL*)(uint32_t);
using fn_continue = int32_t(PB_CALL*)();
using fn_pause = int32_t(PB_CALL*)();
using fn_drain_events = int32_t(PB_CALL*)(pb_event_raw*, uint32_t, uint32_t*);
using fn_last_error = const char*(PB_CALL*)();
using fn_allocate_sequence = uint64_t(PB_CALL*)();
using fn_push_event = int32_t(PB_CALL*)(const pb_event_raw*);
using fn_stats = uint64_t(PB_CALL*)(uint32_t);

template <typename T>
T resolve(HMODULE module, const char* name, std::string* missing) {
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

pb_event_raw make_event(uint64_t sequence, uint8_t kind, uint32_t thread_id, uint64_t address,
                        uint64_t arg0) {
  pb_event_raw event;
  std::memset(&event, 0, sizeof(event));
  event.sequence = sequence;
  event.kind = kind;
  event.thread_id = thread_id;
  event.address = address;
  event.args[0] = arg0;
  return event;
}

/// agent DLL 缺失时的行为：必须**明确失败**，不能静默"成功"。
/// 静默成功比失败危险得多——上层会在错误前提上继续推理。
int run_missing_agent_case(const char* bridge_dll, const char* agent_dll) {
  std::printf("== 反向用例：agent 缺失时必须明确失败 ==\n");
  HMODULE bridge = ::LoadLibraryA(bridge_dll);
  if (bridge == nullptr) {
    std::printf("无法加载桥接 DLL：%s（错误码 %lu）\n", bridge_dll, ::GetLastError());
    return 2;
  }

  std::string missing;
  auto abi_version = resolve<fn_abi_version>(bridge, "pb_abi_version", &missing);
  auto attach = resolve<fn_attach>(bridge, "pb_attach", &missing);
  auto drain = resolve<fn_drain_events>(bridge, "pb_drain_events", &missing);
  auto last_error = resolve<fn_last_error>(bridge, "pb_last_error", &missing);
  check(missing.empty(),
        ("桥接 DLL 导出面完整" + (missing.empty() ? std::string() : "，缺：" + missing)).c_str());
  if (abi_version == nullptr || attach == nullptr || drain == nullptr || last_error == nullptr) {
    return 2;
  }

  // 即使 agent 不在，桥接层自己的 ABI 版本也必须能回答：
  // 否则 Hub 连"这个 DLL 是不是我能加载的"都判断不了。
  check(abi_version() == PB_ABI_VERSION, "agent 缺失时 pb_abi_version 仍返回本层版本");

  const int32_t attach_status = attach(4242);
  const std::string attach_text = last_error();
  check(attach_status != PB_STATUS_OK, "agent 缺失时 pb_attach 不返回成功");
  check(attach_text.find("agent") != std::string::npos ||
            attach_text.find("PINBRIDGE_AGENT_DLL") != std::string::npos,
        ("错误文本指出了 agent 加载问题：" + attach_text).c_str());
  check(attach_text.find(agent_dll) != std::string::npos,
        ("错误文本点出了具体是哪个路径加载失败：" + describe(attach_text.c_str())).c_str());
  check(attach_text.find("PINBRIDGE_AGENT_DLL") != std::string::npos,
        "错误文本指出了可用的排查手段（环境变量名）");

  pb_event_raw buffer[4];
  uint32_t written = 0;
  const int32_t drain_status = drain(buffer, 4, &written);
  check_status(drain_status, attach_status, "agent 缺失时 pb_drain_events 返回同一状态",
               last_error());
  check(written == 0, "失败路径下 written 被置 0（调用方不会读到未初始化数据）");

  ::FreeLibrary(bridge);
  return g_failures == 0 ? 0 : 2;
}

int run_positive_case(const char* bridge_dll, const char* agent_dll) {
  std::printf("== 正向用例：真实 agent，事件走完整条链 ==\n");
  if (::GetFileAttributesA(agent_dll) == INVALID_FILE_ATTRIBUTES) {
    std::printf("跳过：找不到 agent DLL %s（先 cargo build -p pinbridge-agent）\n", agent_dll);
    return 77;
  }

  HMODULE bridge = ::LoadLibraryA(bridge_dll);
  if (bridge == nullptr) {
    std::printf("无法加载桥接 DLL：%s（错误码 %lu）\n", bridge_dll, ::GetLastError());
    return 2;
  }

  std::string missing;
  struct Api {
    fn_abi_version abi_version;
    fn_start start;
    fn_attach attach;
    fn_set_capabilities set_capabilities;
    fn_continue resume;
    fn_pause pause;
    fn_drain_events drain;
    fn_last_error last_error;
    fn_allocate_sequence allocate_sequence;
    fn_push_event push_event;
    fn_stats stats;
  } api{};
  api.abi_version = resolve<fn_abi_version>(bridge, "pb_abi_version", &missing);
  api.start = resolve<fn_start>(bridge, "pb_start", &missing);
  api.attach = resolve<fn_attach>(bridge, "pb_attach", &missing);
  api.set_capabilities = resolve<fn_set_capabilities>(bridge, "pb_set_capabilities", &missing);
  api.resume = resolve<fn_continue>(bridge, "pb_continue", &missing);
  api.pause = resolve<fn_pause>(bridge, "pb_pause", &missing);
  api.drain = resolve<fn_drain_events>(bridge, "pb_drain_events", &missing);
  api.last_error = resolve<fn_last_error>(bridge, "pb_last_error", &missing);
  api.allocate_sequence =
      resolve<fn_allocate_sequence>(bridge, "pb_allocate_sequence", &missing);
  api.push_event = resolve<fn_push_event>(bridge, "pb_push_event", &missing);
  api.stats = resolve<fn_stats>(bridge, "pb_stats", &missing);
  check(missing.empty(),
        ("Hub 期望的 8 个 tool 符号 + 3 个桥接符号都在" +
         (missing.empty() ? std::string() : "，缺：" + missing))
            .c_str());
  if (!missing.empty()) {
    ::FreeLibrary(bridge);
    return 2;
  }

  check(api.abi_version() == PB_ABI_VERSION, "pb_abi_version 与编译期 PB_ABI_VERSION 一致");

  // 没有探针就不该假装能启动目标：必须返回明确的“不支持”。
  const int32_t start_status = api.start("TARGET.exe", "");
  check_status(start_status, PB_STATUS_NOT_SUPPORTED, "pb_start 明确返回不支持", api.last_error());
  check(std::strstr(api.last_error(), "Pin") != nullptr,
        "不支持的原因里写明了需要 Pin（而不是含糊的失败）");

  check_status(api.attach(4242), PB_STATUS_OK, "pb_attach 经桥接层转发成功", api.last_error());

  // 只读基线：mem_read | trace_record = 257。这个值本身也被 static_assert 钉住了。
  const uint32_t baseline = PB_CAP_MEM_READ | PB_CAP_TRACE_RECORD;
  check(baseline == 257u, "只读基线仍是 257（与 Rust 侧一致）");
  check_status(api.set_capabilities(baseline), PB_STATUS_OK, "pb_set_capabilities 转发成功",
               api.last_error());

  // ---- 第一道闸门：能力 ----
  // 只有 mem_read|trace_record 时，进程控制类操作必须被拒。
  // 这条断言的价值在于：它是**跨两个语言边界**验证权限链路的唯一一处。
  check_status(api.pause(), PB_STATUS_CAPABILITY_DENIED, "只读基线不能 pause（能力闸门生效）",
               api.last_error());
  const std::string denied_text = api.last_error();
  check(!denied_text.empty(), ("能力不足时错误文本非空：" + describe(denied_text.c_str())).c_str());
  check_status(api.resume(), PB_STATUS_CAPABILITY_DENIED, "只读基线不能 continue", api.last_error());

  // ---- 第二道闸门：实现是否具备 ----
  // 给出全部能力位之后，桩实现必须返回明确的“不支持”，而不是转而成功。
  // 顺序很关键：先能力、后实现。反过来的话，"能力校验是否真的生效"
  // 在 Pin 到位之前永远不会被覆盖。
  check_status(api.set_capabilities(0xFFFFFFFFu), PB_STATUS_OK, "下发全量能力位",
               api.last_error());
  // 暂停/继续现在是**真实实现**（采集语义：见 design/script-runtime.md）。
  // 这里不再断言"返回不支持"——那是在断言一个已经不存在的事实。
  // 校验的是：有权限时它**成真**，并且状态可查、可以恢复。
  check_status(api.pause(), PB_STATUS_OK, "有权限时 pause 应当成功", api.last_error());
  check(api.stats(4) == 1, "暂停状态应当可查（pb_stats(4)）");
  check_status(api.resume(), PB_STATUS_OK, "continue 应当成功", api.last_error());
  check(api.stats(4) == 0, "恢复之后状态应当归零");
  const std::string state_text = api.last_error();
  check(state_text != denied_text, "两道闸门的错误文本可区分（不是同一个兜底消息）");

  // ---- 暂停/继续各自留下了一条状态变化事件，先把它们取出来看好 ----
  //
  // 这一段的顺序是有意的：**先取空，再推 3 条**，否则后面的"恰好 3 条"会
  // 因为这两条状态事件而失败（第一次改完暂停语义时就撞上了：
  // 断言失败但原因与"事件没入库"完全无关）。
  {
    pb_event_raw state_batch[8];
    uint32_t from_state = 0;
    check_status(api.drain(state_batch, 8, &from_state), PB_STATUS_OK, "取状态变化事件",
                 api.last_error());
    unsigned paused_seen = 0;
    unsigned resumed_seen = 0;
    for (uint32_t i = 0; i < from_state; ++i) {
      if (state_batch[i].kind != PB_EV_STATE_CHANGE) {
        continue;
      }
      if (state_batch[i].args[0] == 1) {
        ++paused_seen;
      }
      if (state_batch[i].args[0] == 0) {
        ++resumed_seen;
      }
    }
    check(paused_seen == 1, "暂停这件事留下了状态变化事件（args[0]=1）");
    check(resumed_seen == 1, "恢复这件事留下了状态变化事件（args[0]=0）");
  }

  // ---- 事件往返：序号由 agent 分配，类型/参数原样回来 ----
  const uint64_t first_sequence = api.allocate_sequence();
  pb_event_raw outgoing[3];
  outgoing[0] = make_event(first_sequence, PB_EV_SYSCALL_ENTER, 100, 0x7FF600002000ULL, 0x40ULL);
  outgoing[1] = make_event(first_sequence + 1, PB_EV_MEM_WRITE, 100, 0x7FF600003000ULL, 0xDEADULL);
  outgoing[2] = make_event(first_sequence + 2, PB_EV_BREAKPOINT_HIT, 100, 0x140001000ULL,
                           0x140001000ULL);
  for (const pb_event_raw& event : outgoing) {
    check_status(api.push_event(&event), PB_STATUS_OK, "pb_push_event 转发成功", api.last_error());
  }
  check(api.stats(1) == 3, "agent 侧积压计数为 3");

  pb_event_raw drained[8];
  uint32_t written = 0;
  std::memset(drained, 0, sizeof(drained));
  check_status(api.drain(drained, 8, &written), PB_STATUS_OK, "pb_drain_events 取回事件",
               api.last_error());
  check(written == 3, "取回 3 条（恰好等于推入的条数）");
  if (written == 3) {
    bool intact = true;
    for (std::size_t index = 0; index < 3; ++index) {
      const pb_event_raw& in = outgoing[index];
      const pb_event_raw& out = drained[index];
      intact = intact && out.sequence == in.sequence && out.kind == in.kind &&
               out.thread_id == in.thread_id && out.address == in.address &&
               out.args[0] == in.args[0];
    }
    // 跨两个 DLL、两个语言边界之后，序号/类型/线程/地址/参数都必须逐字段一致，
    // 任何一处偏移都会在这里变成"值错了"而不是"程序崩了"——这就是这个断言的价值。
    check(intact, "跨语言边界后事件逐字段完好（序号/类型/线程/地址/args[0]）");
  }
  check(api.stats(1) == 0, "取回后积压清零");
  // 已取走的是**全部**取出去的条数：3 条推入 + 2 条状态变化（暂停/恢复）。

  // ---- 参数校验必须早于加载状态判断 ----
  written = 12345;
  check_status(api.drain(drained, 0, &written), PB_STATUS_INVALID_ARGUMENT,
               "capacity 为 0 被拒（且不是当成“加载失败”）", api.last_error());
  check(written == 12345, "参数非法时不动 written（不是我们的输出契约）");
  check_status(api.drain(nullptr, 4, &written), PB_STATUS_INVALID_ARGUMENT,
               "out 为空指针被拒", api.last_error());
  check(std::strstr(api.last_error(), "空指针") != nullptr, "空指针错误文本可读");

  ::FreeLibrary(bridge);
  std::printf("正向用例完成，失败断言 %d 条\n", g_failures);
  return g_failures == 0 ? 0 : 2;
}

}  // namespace

int main(int argc, char** argv) {
  if (argc < 4) {
    std::printf("用法: %s <桥接DLL> <agentDLL> <positive|missing>\n", argv[0]);
    return 3;
  }
  const char* bridge_dll = argv[1];
  const char* agent_dll = argv[2];
  const std::string mode = argv[3];

  // 桥接层在第一次调用时读取这个变量来决定加载哪个 agent。
  // 必须在 LoadLibrary 之前设置好。
  (void)::_putenv_s("PINBRIDGE_AGENT_DLL", agent_dll);
  std::printf("桥接 DLL: %s\nagent DLL: %s\n", bridge_dll, agent_dll);

  if (mode == "missing") {
    return run_missing_agent_case(bridge_dll, agent_dll);
  }
  if (mode == "positive") {
    return run_positive_case(bridge_dll, agent_dll);
  }
  std::printf("未知模式：%s\n", mode.c_str());
  return 3;
}
