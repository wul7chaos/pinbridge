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
# 真正的工具 DLL：需要 Pin SDK（PIN_ROOT）
# --------------------------------------------------------------------------
option(PINBRIDGE_WITH_PIN "构建依赖 Intel Pin 的工具 DLL" OFF)

if(PINBRIDGE_WITH_PIN)
  if(NOT DEFINED PIN_ROOT)
    message(FATAL_ERROR
      "启用 PINBRIDGE_WITH_PIN 时必须指定 PIN_ROOT=.../pin-3.31-xxx\\source\\tools\\Pin"
      "（获取方式见 third_party/pin/README.md）")
  endif()
  if(NOT EXISTS "${PIN_ROOT}/pin.H")
    message(FATAL_ERROR "PIN_ROOT 下找不到 pin.H：${PIN_ROOT}")
  endif()

  add_library(pinbridge SHARED
    src/pinbridge_tool.cpp     # Pin 工具入口：探针注册、JIT 上下文
    src/agent_bridge.cpp       # 加载 pinbridge_agentdll 并转发 C ABI 调用
    src/flags_virtualize.cpp   # TF 虚拟化（见 docs/design/flags-virtualization.md）
  )
  target_include_directories(pinbridge PRIVATE include "${PIN_ROOT}")
  target_link_libraries(pinbridge PRIVATE pinbridge::abi_selfcheck)
  target_compile_definitions(pinbridge PRIVATE PINBRIDGE_TOOL_BUILD=1)

  # Pin 的约定：工具 DLL 不能链接 CRT 的调试版；且必须是 /MT
  if(MSVC)
    target_compile_options(pinbridge PRIVATE /MT /EHsc)
  endif()

  set_target_properties(pinbridge PROPERTIES
    PREFIX ""                 # pin.exe -t pinbridge.dll 期望没有 lib 前缀
    RUNTIME_OUTPUT_DIRECTORY "${CMAKE_BINARY_DIR}/dist"
  )
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
  else()
    message(STATUS "非 Windows：只注册 abi_selfcheck（桥接层不可用）")
  endif()
endif()
