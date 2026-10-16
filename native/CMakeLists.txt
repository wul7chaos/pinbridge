# pinbridge.dll（C++ / Pin tool 层）
#
# 这一层**没有**在 M0 阶段接入 Intel Pin：本地没有 Pin SDK 时不能把
# CMake 配置写成"必须先装 Pin 才能构建"，否则整个仓库的 CI 就得依赖
# 一个几百 MB 的第三方包。因此这里只做两件事：
#
#   1. 声明构建方式与产物位置（Hub 侧通过 pinbridge-sys 运行时加载）；
#   2. 用 static_assert 把 C ABI 的布局契约钉在编译期。
#
# 接入真实 Pin 的步骤见 docs/roadmap.md 的 M1 与 third_party/pin/README.md。

cmake_minimum_required(VERSION 3.20)
project(pinbridge_native LANGUAGES CXX)

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_CXX_EXTENSIONS OFF)

if(NOT CMAKE_BUILD_TYPE)
  set(CMAKE_BUILD_TYPE Release)
endif()

# 与 Rust 侧 `.cargo/config.toml` 的 target-feature=+crt-static 对齐：
# 不让 vcruntime 依赖被带进目标进程——目标机上不一定有对应的运行库。
if(MSVC)
  set(CMAKE_MSVC_RUNTIME_LIBRARY "MultiThreaded")
  # **必须有 /utf-8**：本仓库的注释与字符串字面量是 UTF-8（中文为主），
  # 而 MSVC 默认按系统代码页（简体中文环境是 936/GBK）解释源文件。
  # 后果不是"注释乱码"这么轻——连续的 UTF-8 字节被拆成两个 GBK 字符后，
  # 可能吞掉字符串的收尾引号，于是报出"字符串字面量中的换行符"这种
  # 与真正原因毫无关系的错误。这一条是被真实的编译失败教会的。
  add_compile_options(/utf-8)
  # MSVC 在**非英文界面**下用本地化文本输出包含文件列表（简体中文是“注意: 包含文件:”），
  # 而 Ninja 靠这个前缀解析依赖关系。设错的表现是"构建日志被上千行“注意: 包含文件”刷屏"，
  # 把真正的错误淹掉——排查时非常误导。这里显式告诉 CMake 该前缀。
  if(CMAKE_GENERATOR MATCHES "Ninja")
    set(CMAKE_CL_SHOWINCLUDES_PREFIX "注意: 包含文件:  " CACHE STRING "MSVC showIncludes 前缀")
  endif()
endif()

# 产物统一放在构建目录根部（不分 Debug/Release 子目录）：
# ctest 与 CI 都用固定路径取产物，多一层 $<CONFIG> 只会让路径无法预测。
set(CMAKE_RUNTIME_OUTPUT_DIRECTORY "${CMAKE_BINARY_DIR}")
set(CMAKE_LIBRARY_OUTPUT_DIRECTORY "${CMAKE_BINARY_DIR}")
set(CMAKE_ARCHIVE_OUTPUT_DIRECTORY "${CMAKE_BINARY_DIR}")

# --------------------------------------------------------------------------
# ABI 自检目标：不需要 Pin，任何机器上都能构建
# 它的价值在于：C++ 侧的 static_assert 一旦不成立，CI 立刻红，
# 而不是等到运行期调用 pb_drain_events 时才把目标进程搞崩。
# --------------------------------------------------------------------------
add_library(pinbridge_abi_selfcheck STATIC src/abi_selfcheck.cpp)
target_include_directories(pinbridge_abi_selfcheck PUBLIC include)
add_library(pinbridge::abi_selfcheck ALIAS pinbridge_abi_selfcheck)

# --------------------------------------------------------------------------
# 桥接 DLL：**不需要 Pin**，但在没有 Pin 的情况下让 Hub ↔ agent 真实跑起来
# 它导出 Hub 期望的 8 个 tool 符号，内部转发给 agent DLL（运行时加载）。
# 这是 M1 中「pinbridge-sys 加载器端到端」这条验收标准的落点：
# 在 Pin 到位之前，加载器已经能被真实地驱动与测试。
#
# 只在 Windows 上构建：它依赖 `LoadLibraryA`/`GetProcAddress`/`DLLMain`，
# 而这些正是 agent 链路在 M0/M1 的真实形态（见 docs/specs/c-abi.md）。
# 非 Windows 平台下**跳过而不是假装能编**：ABI 自检仍然会构建并运行，
# 于是"头文件是否可移植"这件事在 Linux/macOS 上依然被守住。
# --------------------------------------------------------------------------
if(WIN32)
  add_library(pinbridge_bridge SHARED
    src/agent_bridge.cpp
    src/abi_selfcheck.cpp
  )
  target_include_directories(pinbridge_bridge PRIVATE include)
  target_compile_definitions(pinbridge_bridge PRIVATE PINBRIDGE_BRIDGE_BUILD=1)
  target_link_libraries(pinbridge_bridge PRIVATE pinbridge::abi_selfcheck)
  set_target_properties(pinbridge_bridge PROPERTIES PREFIX "")
else()
  message(STATUS "非 Windows：跳过桥接层（它依赖 LoadLibraryA；agent 链路目前仅 Windows）")
endif()

# --------------------------------------------------------------------------
# 真正的工具 DLL：需要 Pin SDK（PIN_ROOT = kit 根目录）
#
# 下面这些编译/链接参数**不是凭经验猜的**：它们逐条取自 kit 里的
# `source/tools/Config/makefile.win.config`（行号见各行注释）。
# Pin 用自己的 CRT（pincrt），还要求关掉 C++ 异常/RTTI、强制包含
# msvc_compat.h、并用 Ptrace_DllMainCRTStartup 作为入口——
# 少任何一条都会得到一串看起来毫无关联的链接错误。
# --------------------------------------------------------------------------
option(PINBRIDGE_WITH_PIN "构建依赖 Intel Pin 的工具 DLL" OFF)

if(PINBRIDGE_WITH_PIN)
  if(NOT DEFINED PIN_ROOT)
    message(FATAL_ERROR
      "启用 PINBRIDGE_WITH_PIN 时必须指定 PIN_ROOT=<pin kit 根目录>"
      "（即含 pin.exe 与 intel64/ 的那一层；获取与校验见 third_party/pin/README.md）")
  endif()
  if(NOT EXISTS "${PIN_ROOT}/source/include/pin/pin.H")
    message(FATAL_ERROR "PIN_ROOT 下找不到 source/include/pin/pin.H：${PIN_ROOT}")
  endif()
  if(NOT EXISTS "${PIN_ROOT}/intel64/lib/pin.lib")
    message(FATAL_ERROR "PIN_ROOT 下找不到 intel64/lib/pin.lib：${PIN_ROOT}")
  endif()

  # `_WINDOWS_H_PATH_` 必须是**不带引号**的裸路径：
  # Pin 的 extras/crt/include/windows.h 第 38 行这样用它——
  #     #define WINDOWS_H_PATH <_WINDOWS_H_PATH_/Windows.h>
  #     #include WINDOWS_H_PATH
  # 所以宏里多一对引号会得到 `<"C:/..."/Windows.h>`，报出
  # "无法打开包括文件: "C:/..."/Windows.h" 这种带引号的怪路径（实测踩过）。
  # 官方 makefile 也是靠命令行把引号吃掉才成立的
  # （Config/makefile.win.config:104）。
  find_path(PINBRIDGE_WINDOWS_H_DIR Windows.h)
  if(NOT PINBRIDGE_WINDOWS_H_DIR)
    message(FATAL_ERROR "找不到 Windows.h：请在 vcvars 环境里配置（先运行 vcvars64.bat）")
  endif()
  message(STATUS "Windows.h 目录：${PINBRIDGE_WINDOWS_H_DIR}")

  # 复用已经测过的转发层，再叠加探针：
  #   探针（本文件）→ pinbridge_bridge.dll → pinbridge_agent.dll
  # 这样 Pin tool 不需要第二套"加载 agent / 校验 ABI / 转发调用"的实现。
  add_library(pinbridge SHARED src/pinbridge_tool.cpp)
  target_link_libraries(pinbridge PRIVATE pinbridge::abi_selfcheck)

  target_include_directories(pinbridge PRIVATE
    include
    # makefile.win.config:193-208（KIT=1 分支的 TOOL_INCLUDES）
    "${PIN_ROOT}/source/include/pin"
    "${PIN_ROOT}/source/include/pin/gen"
    "${PIN_ROOT}/extras/stlport/include"
    "${PIN_ROOT}/extras"
    "${PIN_ROOT}/extras/libstdc++/include"
    "${PIN_ROOT}/extras/crt/include"
    "${PIN_ROOT}/extras/crt"
    "${PIN_ROOT}/extras/crt/include/arch-x86_64"
    "${PIN_ROOT}/extras/crt/include/kernel/uapi"
    "${PIN_ROOT}/extras/crt/include/kernel/uapi/asm-x86"
    # makefile.win.config:278（XED_ROOT/include/xed —— 注意末尾的 xed，
    # 少了它 pin.H 会报"找不到 xed-interface.h"，而那个错误看起来像缺文件）
    "${PIN_ROOT}/extras/xed-intel64/include/xed"
    "${PIN_ROOT}/source/tools/Utils"
    "${PIN_ROOT}/source/tools/InstLib"
    "${PIN_ROOT}/extras/components/include"
  )

  # makefile.win.config:100/106/111/310/327 与 158（OPT_CXX := /MD）
  target_compile_options(pinbridge PRIVATE
    /EHs- /EHa- /wd4530 /DTARGET_WINDOWS /nologo /Gy
    /Oi- /GR- /GS-
    /DPIN_CRT=1
    "/D_WINDOWS_H_PATH_=${PINBRIDGE_WINDOWS_H_DIR}"
    /D__LP64__ /DTARGET_IA32E /DHOST_IA32E
    /Zc:threadSafeInit- /Zc:sizedDealloc- /wd5208
    /FIinclude/msvc_compat.h
  )
  target_compile_definitions(pinbridge PRIVATE PINBRIDGE_TOOL_BUILD=1)

  # Pin 要求 /MD（与仓库其它目标的 /MT 相反）：**必须逐目标覆盖**，
  # 否则会得到"pincrt 与 libcmt 冲突"这类含义晦涩的链接错误。
  set_property(TARGET pinbridge PROPERTY MSVC_RUNTIME_LIBRARY "MultiThreadedDLL")

  # makefile.win.config:128/137/218/283-284/329
  target_link_options(pinbridge PRIVATE
    /DLL /EXPORT:main /NODEFAULTLIB /NOLOGO /INCREMENTAL:NO
    /IGNORE:4210 /IGNORE:4049 /MACHINE:x64
    /ENTRY:Ptrace_DllMainCRTStartup /DYNAMICBASE /NXCOMPAT
    "${PIN_ROOT}/intel64/runtime/pincrt/crtbeginS.obj"
  )
  # makefile.win.config:72 与 211-213/283-284。
  # 路径必须按**实测**布局写：pincrt.lib 在 runtime/pincrt 下，不在 lib 下。
  target_link_libraries(pinbridge PRIVATE
    "${PIN_ROOT}/intel64/lib/pin.lib"
    "${PIN_ROOT}/extras/xed-intel64/lib/xed.lib"
    "${PIN_ROOT}/intel64/lib/pinipc.lib"
    "${PIN_ROOT}/intel64/runtime/pincrt/pincrt.lib"
    kernel32.lib
  )
  target_link_directories(pinbridge PRIVATE
    "${PIN_ROOT}/intel64/lib"
    "${PIN_ROOT}/intel64/lib-ext"
    "${PIN_ROOT}/intel64/runtime/pincrt"
  )

  set_target_properties(pinbridge PROPERTIES
    PREFIX ""                 # pin.exe -t pinbridge.dll 期望没有 lib 前缀
    RUNTIME_OUTPUT_DIRECTORY "${CMAKE_BINARY_DIR}/dist"
  )

  # 把 agent DLL 一并放到 dist/：这样 `dist/` 是一个**自包含**的可运行目录，
  # `cd dist && pin.exe -t pinbridge.dll -- TARGET.exe` 直接就能跑通，
  # 不需要任何环境变量。少一个环境变量 = 少一类"在我机器上能跑"。
  if(WIN32 AND PINBRIDGE_AGENT_DLL AND EXISTS "${PINBRIDGE_AGENT_DLL}")
    add_custom_command(TARGET pinbridge POST_BUILD
      COMMAND ${CMAKE_COMMAND} -E copy_if_different
              "${PINBRIDGE_AGENT_DLL}"
              "${CMAKE_BINARY_DIR}/dist/pinbridge_agent.dll"
      COMMENT "把 pinbridge_agent.dll 复制到 dist/（让 dist 自包含）")
  endif()

  message(STATUS "Pin tool 目标已启用：PIN_ROOT=${PIN_ROOT}")
endif()

# --------------------------------------------------------------------------
# 测试：不需要 Pin，验证"Hub 侧能加载并校验 ABI 版本"这条路径
# --------------------------------------------------------------------------
include(CTest)
if(BUILD_TESTING)
  add_executable(abi_selfcheck_test tests/abi_selfcheck_test.cpp)
  target_link_libraries(abi_selfcheck_test PRIVATE pinbridge::abi_selfcheck)
  # 用 $<TARGET_FILE:...> 而不是裸目标名：后者在部分生成器/CMake 版本组合下
  # 会把产物路径拼错（实测出现过 `<build>/Development/x.exe.exe` 这种解析结果），
  # 表现为 ctest 报 "Unable to find executable"。
  add_test(NAME abi_selfcheck COMMAND $<TARGET_FILE:abi_selfcheck_test>)

  # ---------------------------------------------------------------- 桥接层测试
  # 只在 Windows 上：非 Windows 没有桥接 DLL（见上面的 if(WIN32)）。
  # 这里不能"注册了但跑不了"——那样 ctest 会报一个无法解释的失败，
  # 而"平台不适用"与"实现坏了"是两件必须能区分的事。
  if(WIN32)
    add_executable(bridge_roundtrip_test tests/bridge_roundtrip_test.cpp)
    # 只链接 ABI 自检（拿类型与常量），**不链接**桥接 DLL：
    # 桥接层必须通过运行时加载来验证，否则测的是链接器而不是我们的加载逻辑。
    target_link_libraries(bridge_roundtrip_test PRIVATE pinbridge::abi_selfcheck)

    # agent DLL 由 cargo 构建（cdylib）。允许显式指定；否则在 target/{debug,release} 下找。
    if(NOT PINBRIDGE_AGENT_DLL)
      foreach(_profile debug release)
        set(_candidate "${CMAKE_CURRENT_SOURCE_DIR}/../target/${_profile}/pinbridge_agent.dll")
        if(EXISTS "${_candidate}")
          set(PINBRIDGE_AGENT_DLL "${_candidate}")
          break()
        endif()
      endforeach()
    endif()

    if(PINBRIDGE_AGENT_DLL AND EXISTS "${PINBRIDGE_AGENT_DLL}")
      message(STATUS "agent DLL: ${PINBRIDGE_AGENT_DLL}")
    else()
      message(WARNING
        "未找到 pinbridge_agent.dll：桥接层的正向用例会被标记为跳过。"
        "先执行 `cargo build -p pinbridge-agent`，或显式传 -DPINBRIDGE_AGENT_DLL=<路径>。")
    endif()

    # 正向用例：真实 agent 在场，验证"Hub 侧调用 → 桥接 → agent → 事件回来"整条链。
    add_test(NAME bridge_roundtrip
      COMMAND $<TARGET_FILE:bridge_roundtrip_test>
              "$<TARGET_FILE:pinbridge_bridge>" "${PINBRIDGE_AGENT_DLL}" "positive")
    # 反向用例：agent 缺失时必须**明确失败**，而不是静默返回成功。
    add_test(NAME bridge_missing_agent
      COMMAND $<TARGET_FILE:bridge_roundtrip_test>
              "$<TARGET_FILE:pinbridge_bridge>" "${CMAKE_CURRENT_BINARY_DIR}/不存在-pinbridge_agent.dll"
              "missing")
    set_tests_properties(bridge_roundtrip bridge_missing_agent PROPERTIES SKIP_RETURN_CODE 77)

    # ------------------------------------------------------------ Pin tool 端到端
    # 只有真正构建了 Pin tool 才注册：注册一个"跑不了"的测试只会制造
    # 无法解释的失败（"平台不适用"与"实现坏了"必须能区分）。
    if(PINBRIDGE_WITH_PIN AND EXISTS "${PIN_ROOT}/pin.exe")
      add_executable(pb_sample_target tests/sample_target.cpp)
      # 靶子要读 ABI 头（`--agent-check` 模式用），但**不链接**任何 Rust 侧库：
      # 它在运行期 `LoadLibraryA` + `GetProcAddress`，于是在"没有 agent DLL"的
      # 环境里也能编出来、跑起来。
      target_include_directories(pb_sample_target PRIVATE include)
      set_target_properties(pb_sample_target PROPERTIES
        RUNTIME_OUTPUT_DIRECTORY "${CMAKE_BINARY_DIR}")

      # 端到端：**被 Pin 插装**的进程里驱动 agent —— 脚本被事件驱动、断点命中、
      # 内存读可用。这条把 roadmap 的"手工验证清单"变成机器可判定的用例。
      add_test(NAME pin_tool_agent_end_to_end
        COMMAND "${PIN_ROOT}/pin.exe"
                -t "$<TARGET_FILE:pinbridge>"
                -pb_syscalls 1
                -pb_events 2
                --
                "$<TARGET_FILE:pb_sample_target>" --agent-check
                "${PINBRIDGE_AGENT_DLL}"
                "${CMAKE_CURRENT_SOURCE_DIR}/tests/agent_check_script.py")
      set_tests_properties(pin_tool_agent_end_to_end PROPERTIES
        PASS_REGULAR_EXPRESSION "PB_AGENT_CHECK script_output=[1-9][0-9]* ssn_mark=[1-9][0-9]* breakpoint_hit=[1-9][0-9]* memory=1"
        SKIP_RETURN_CODE 77)

      add_test(NAME pin_tool_instruments_a_target
        COMMAND "${PIN_ROOT}/pin.exe"
                -t "$<TARGET_FILE:pinbridge>"
                -pb_trace "${CMAKE_BINARY_DIR}/pin-tool-trace.pbt"
                -pb_events 16
                --
                "$<TARGET_FILE:pb_sample_target>")
      # Pin 的 stdout/stderr 都是我们要检查的证据，失败时原样打出来。
      set_tests_properties(pin_tool_instruments_a_target PROPERTIES
        FAIL_REGULAR_EXPRESSION "入口探针\\*\\*未\\*\\*命中"
        PASS_REGULAR_EXPRESSION "入口点命中"
        # 工作目录设成 dist：Pin tool 用 `LoadLibraryA("pinbridge_bridge.dll")`
        # 按名字加载同目录的桥接层，而 Windows 的搜索顺序里包含当前目录。
        # 这不只是"让测试通过"——它就是人手工运行时该做的事
        # （`cd native/build/dist`，然后 pin.exe -t pinbridge.dll -- ...）。
        WORKING_DIRECTORY "${CMAKE_BINARY_DIR}/dist")
      if(PINBRIDGE_AGENT_DLL)
        # agent DLL 的位置**显式**告诉桥接层：ctest 的环境与我们不同，
        # 靠"同目录猜测"失败时会报"无法加载 agent DLL，已尝试 …"这种含糊信息。
        set_tests_properties(pin_tool_instruments_a_target PROPERTIES
          ENVIRONMENT "PINBRIDGE_AGENT_DLL=${PINBRIDGE_AGENT_DLL}")
      endif()

      # -------------------------------------------------- 系统调用探针端到端
      # 判据不是"退出码为 0"，而是三条正则：
      #   * 必须出现"捕获数满足要求"（工具自己在没捕够时打"失败：…"）；
      #   * 不得出现"溢出丢弃 [1-9]"（静默丢事件是禁止的，实测该负载下为 0）；
      #   * 不得出现 Pin 的内部故障（这一条曾经 5/5 稳定触发，是这轮修掉的核心缺陷）。
      add_test(NAME pin_tool_captures_syscalls
        COMMAND "${PIN_ROOT}/pin.exe"
                -t "$<TARGET_FILE:pinbridge>"
                -pb_syscalls 1
                -pb_syscall_max 100000
                -pb_require_syscalls 300
                -pb_require_syscall_exits 300
                -pb_trace "${CMAKE_BINARY_DIR}/pin-syscall-trace.pbt"
                -pb_events 2
                --
                "$<TARGET_FILE:pb_sample_target>" --syscalls 300)
      set_tests_properties(pin_tool_captures_syscalls PROPERTIES
        PASS_REGULAR_EXPRESSION "返回事件捕获数满足要求"
        FAIL_REGULAR_EXPRESSION "失败：要求至少捕获|exit=0|溢出丢弃 [1-9]|Unhandled internal exception"
        WORKING_DIRECTORY "${CMAKE_BINARY_DIR}/dist")
      if(PINBRIDGE_AGENT_DLL)
        set_tests_properties(pin_tool_captures_syscalls PROPERTIES
          ENVIRONMENT "PINBRIDGE_AGENT_DLL=${PINBRIDGE_AGENT_DLL}")
      endif()

      # -------------------------------------------------- TF 单步虚拟化
      # 靶子的 TF 实验：置位 TF（`__writeeflags` → `popfq`）→ 执行一条语句 →
      # 用 SEH 统计真实单步次数 → 读回 flags（`__readeflags` → `pushfq`）。
      #
      # 四种配置的签名各不相同，每一档都可证伪：
      #   单独跑靶子            ：real_steps=50 tf_reads=0（内核派发异常时清掉了 TF）
      #   Pin + -pb_tf off/watch：进程以 0x80000004 (STATUS_SINGLE_STEP) 终止
      #                            —— 目标自己的 SEH 收不到它自己的单步异常
      #   Pin + -pb_tf virtualize：real_steps=0 tf_reads=50
      #                            —— 目标确信自己在单步，其实一次都没被单步
      add_test(NAME sample_target_tf_control
        COMMAND "$<TARGET_FILE:pb_sample_target>" --tf-set 50)
      set_tests_properties(sample_target_tf_control PROPERTIES
        PASS_REGULAR_EXPRESSION "PB_TF_RESULT real_steps=50 tf_reads=0")

      add_test(NAME pin_tool_tf_virtualize
        COMMAND "${PIN_ROOT}/pin.exe"
                -t "$<TARGET_FILE:pinbridge>"
                -pb_tf virtualize
                -pb_require_tf_sets 50
                -pb_events 0
                -pb_trace "${CMAKE_BINARY_DIR}/pin-tf-trace.pbt"
                --
                "$<TARGET_FILE:pb_sample_target>" --tf-set 50)
      set_tests_properties(pin_tool_tf_virtualize PROPERTIES
        PASS_REGULAR_EXPRESSION "PB_TF_RESULT real_steps=0 tf_reads=50"
        FAIL_REGULAR_EXPRESSION "real_steps=[1-9]|Unhandled internal exception|失败：要求至少观察到"
        WORKING_DIRECTORY "${CMAKE_BINARY_DIR}/dist")
      if(PINBRIDGE_AGENT_DLL)
        set_tests_properties(pin_tool_tf_virtualize PROPERTIES
          ENVIRONMENT "PINBRIDGE_AGENT_DLL=${PINBRIDGE_AGENT_DLL}")
      endif()

      # 不虚拟化时目标会**死于它自己的单步**。这条用例刻意断言"它会失败"：
      # 如果哪天它不再失败了，说明要么 Pin 行为变了、要么我们不小心虚拟化了，
      # 两种都值得被测试套件叫醒。
      add_test(NAME pin_tool_tf_without_virtualize_is_fatal
        COMMAND "${PIN_ROOT}/pin.exe"
                -t "$<TARGET_FILE:pinbridge>"
                -pb_tf off
                -pb_events 0
                --
                "$<TARGET_FILE:pb_sample_target>" --tf-set 5)
      set_tests_properties(pin_tool_tf_without_virtualize_is_fatal PROPERTIES
        WILL_FAIL TRUE
        WORKING_DIRECTORY "${CMAKE_BINARY_DIR}/dist")
      if(PINBRIDGE_AGENT_DLL)
        set_tests_properties(pin_tool_tf_without_virtualize_is_fatal PROPERTIES
          ENVIRONMENT "PINBRIDGE_AGENT_DLL=${PINBRIDGE_AGENT_DLL}")
      endif()

      # -------------------------------------------------- 指令模板匹配端到端
      # 靶子里那个函数带 8 字节魔数（0x5042524944474531），编译后作为立即数
      # 出现在代码段里。模式**必须按小端写**（31 45 47 44 49 52 42 50）：
      # 按文本顺序写会命中 0 处，而且看起来像"模板功能没生效"。
      # 判据是"调 500 次 → 恰好 500 次命中"，由工具自己比对并打标记。
      add_test(NAME pin_tool_pattern_hit
        COMMAND "${PIN_ROOT}/pin.exe"
                -t "$<TARGET_FILE:pinbridge>"
                -pb_pattern "1:pbridge_magic:3145474449524250"
                -pb_pattern_max 64
                -pb_require_pattern_hits 500
                -pb_trace "${CMAKE_BINARY_DIR}/pin-pattern-trace.pbt"
                -pb_events 4
                --
                "$<TARGET_FILE:pb_sample_target>" --pattern-hits 500)
      set_tests_properties(pin_tool_pattern_hit PROPERTIES
        PASS_REGULAR_EXPRESSION "模板命中次数满足要求"
        FAIL_REGULAR_EXPRESSION "失败：要求至少命中|登记命中点 0 个|Unhandled internal exception"
        WORKING_DIRECTORY "${CMAKE_BINARY_DIR}/dist")
      if(PINBRIDGE_AGENT_DLL)
        set_tests_properties(pin_tool_pattern_hit PROPERTIES
          ENVIRONMENT "PINBRIDGE_AGENT_DLL=${PINBRIDGE_AGENT_DLL}")
      endif()

      # -------------------------------------------------- 内存探针端到端
      # 这条必须用脚本：判断"-pb_mem_range 过滤生效"需要先知道缓冲区地址，
      # 而它每次运行都不同（ASLR）。ctest 的一条命令里做不到"先读输出再决定参数"。
      # 脚本自己跑两步，并给出三条判定（写入数精确、零丢弃、过滤计数 > 0）。
      find_program(PINBRIDGE_PWSH pwsh)
      if(PINBRIDGE_PWSH)
        add_test(NAME pin_tool_mem_range_filter
          COMMAND "${PINBRIDGE_PWSH}" -NoProfile -File
                  "${CMAKE_CURRENT_SOURCE_DIR}/../scripts/check-mem-probe.ps1"
                  -PinRoot "third_party/pin" -BuildDir "native/build-pin")
        set_tests_properties(pin_tool_mem_range_filter PROPERTIES SKIP_RETURN_CODE 77)
      endif()
    endif()
  else()
    message(STATUS "非 Windows：只注册 abi_selfcheck（桥接层不可用）")
  endif()
endif()
