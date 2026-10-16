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
endif()

# --------------------------------------------------------------------------
# ABI 自检目标：不需要 Pin，任何机器上都能构建
# 它的价值在于：C++ 侧的 static_assert 一旦不成立，CI 立刻红，
# 而不是等到运行期调用 pb_drain_events 时才把目标进程搞崩。
# --------------------------------------------------------------------------
add_library(pinbridge_abi_selfcheck STATIC src/abi_selfcheck.cpp)
target_include_directories(pinbridge_abi_selfcheck PUBLIC include)
add_library(pinbridge::abi_selfcheck ALIAS pinbridge_abi_selfcheck)

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
  add_test(NAME abi_selfcheck COMMAND abi_selfcheck_test)
endif()
