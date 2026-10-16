# 被 `pb_sample_target.exe --agent-check` 加载的脚本。
#
# 它做的是**最小可判定**的事：收到系统调用进入事件时，把事件里的 SSN
# 用 `pb.mark` 报回来。靶子侧据此断言"脚本确实被事件驱动了、而且看到了内容"——
# 只断言"有输出"是不够的：输出可能来自加载期的 `pb.log`。
#
# 注意 `import pb`：`pb` 是 agent 注入到 `sys.modules` 里的模块，不 import
# 就直接用会在运行期报 NameError——这条用例第一次跑就踩到了这个坑
# （靶子把 CPython 的堆栈原样打出来，所以一眼能看出是脚本的问题而不是工具的问题）。

import pb


def on_syscall(event):
    pb.mark("seen", event.args[0])


pb.subscribe("syscall_enter", on_syscall)