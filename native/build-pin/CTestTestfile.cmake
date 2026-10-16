# CMake generated Testfile for 
# Source directory: C:/Users/19256/Documents/Projects/pinbridge/native
# Build directory: C:/Users/19256/Documents/Projects/pinbridge/native/build-pin
# 
# This file includes the relevant testing commands required for 
# testing this directory and lists subdirectories to be tested as well.
add_test("abi_selfcheck" "C:/Users/19256/Documents/Projects/pinbridge/native/build-pin/abi_selfcheck_test.exe")
set_tests_properties("abi_selfcheck" PROPERTIES  _BACKTRACE_TRIPLES "C:/Users/19256/Documents/Projects/pinbridge/native/CMakeLists.txt;212;add_test;C:/Users/19256/Documents/Projects/pinbridge/native/CMakeLists.txt;0;")
add_test("bridge_roundtrip" "C:/Users/19256/Documents/Projects/pinbridge/native/build-pin/bridge_roundtrip_test.exe" "C:/Users/19256/Documents/Projects/pinbridge/native/build-pin/pinbridge_bridge.dll" [[C:\Users\19256\Documents\Projects\pinbridge\target\debug\pinbridge_agent.dll]] "positive")
set_tests_properties("bridge_roundtrip" PROPERTIES  SKIP_RETURN_CODE "77" _BACKTRACE_TRIPLES "C:/Users/19256/Documents/Projects/pinbridge/native/CMakeLists.txt;244;add_test;C:/Users/19256/Documents/Projects/pinbridge/native/CMakeLists.txt;0;")
add_test("bridge_missing_agent" "C:/Users/19256/Documents/Projects/pinbridge/native/build-pin/bridge_roundtrip_test.exe" "C:/Users/19256/Documents/Projects/pinbridge/native/build-pin/pinbridge_bridge.dll" "C:/Users/19256/Documents/Projects/pinbridge/native/build-pin/不存在-pinbridge_agent.dll" "missing")
set_tests_properties("bridge_missing_agent" PROPERTIES  SKIP_RETURN_CODE "77" _BACKTRACE_TRIPLES "C:/Users/19256/Documents/Projects/pinbridge/native/CMakeLists.txt;248;add_test;C:/Users/19256/Documents/Projects/pinbridge/native/CMakeLists.txt;0;")
add_test("pin_tool_instruments_a_target" "C:/Users/19256/Documents/Projects/pinbridge/third_party/pin/pin.exe" "-t" "C:/Users/19256/Documents/Projects/pinbridge/native/build-pin/dist/pinbridge.dll" "-pb_trace" "C:/Users/19256/Documents/Projects/pinbridge/native/build-pin/pin-tool-trace.pbt" "-pb_events" "16" "--" "C:/Users/19256/Documents/Projects/pinbridge/native/build-pin/pb_sample_target.exe")
set_tests_properties("pin_tool_instruments_a_target" PROPERTIES  ENVIRONMENT [[PINBRIDGE_AGENT_DLL=C:\Users\19256\Documents\Projects\pinbridge\target\debug\pinbridge_agent.dll]] FAIL_REGULAR_EXPRESSION [[入口探针\*\*未\*\*命中]] PASS_REGULAR_EXPRESSION "入口点命中" WORKING_DIRECTORY "C:/Users/19256/Documents/Projects/pinbridge/native/build-pin/dist" _BACKTRACE_TRIPLES "C:/Users/19256/Documents/Projects/pinbridge/native/CMakeLists.txt;262;add_test;C:/Users/19256/Documents/Projects/pinbridge/native/CMakeLists.txt;0;")
