# TF 异常位置比对：现状与下一步

- 状态：**两侧的报数已实现并有用例**；「等值」这条判据**还没有**，原因见下。
- 相关：`scripts/check-tf-observation.ps1`、ctest `pin_tool_tf_observation`、
  `native/tests/sample_target.cpp`、`native/src/pinbridge_tool.cpp`

## 判据是什么

**同一次**单步异常：工具说它发生在 X，靶子自己从
`GetExceptionInformation()->ExceptionAddress` 读到 Y，验 **X == Y**。
两边对不上就意味着"我们说的异常位置"与"目标看到的那一处"不是同一个 ——
之后所有基于地址的分析都会错位。

## 为什么现在还取不到两侧的数字

| 模式 | 目标能不能活过真实单步 | 有没有可比的 Y |
|------|------------------------|----------------|
| `-pb_tf virtualize` | ✅ 活（但工具把真实单步**剥掉了** ⇒ 目标**根本看不到异常**） | ✗ `PB_TF_EXC count=0` |
| `-pb_tf watch` | ✗ 死 | ✗ |
| `--tf-set` / `--tf-threads` 单独跑（不挂 Pin） | ✅ 活 | ✅（但那是**没有工具**时的原生行为） |

`watch` 下目标为什么死：未接住的单步异常，进程以
`STATUS_SINGLE_STEP`（`0x80000004`）终止。**这不是缺陷** —— 它是本项目**早已记录并
早已断言**的行为：

* `docs/roadmap.md` 的 TF 条目写着"Pin 下不虚拟化则进程以 `0x80000004` 终止"；
* ctest 里有一条 `pin_tool_tf_without_virtualize_is_fatal` 专门断言它。

> **更正（本轮）**：我在前两轮把这个行为写成"新发现的缺陷"，并据此把
> `scripts/check-tf-location.ps1` 做成"已知缺陷 ⇒ 跳过"。那是错的 ——
> 项目里既有文档也有用例。教训与第 38 轮那条同源：**在下结论之前先搜自家的文档与用例**；
> 这次具体化为一条可操作的做法：**动手写"新缺陷"之前先 `ctest -N` 看有没有同名断言**。

## 下一步（唯一一条路）

让靶子**活过**真实单步，才能同时拿到 X 与 Y：

1. 在靶子里改用**向量化异常处理（VEH）**而不是 `__try/__except`
   （`AddVectoredExceptionHandler`）：VEH 不依赖栈上的作用域，即使单步发生在
   Pin 注入的代码边界上也能接到，处理完 `EXCEPTION_CONTINUE_EXECUTION` 继续跑；
2. 从 VEH 的 `ExceptionRecord->ExceptionAddress` 取 Y 并打印（与现在同形的 `PB_TF_EXC`）；
3. 工具侧照旧报 X（已有）；
4. 判据回到"工具的 X ∈ 靶子报出的 Y 集合"，写进 `check-tf-observation.ps1` 的第三档。

## 已经做到、且现在就有断言的部分

* **工具侧** `TF 异常位置：0x…（首个真实单步，共 N 次）`；`virtualize` 下如实报
  `没有观察到真实单步`；
* **靶子侧** `PB_TF_EXC count=N addr0=0x…`（单独运行可验证：`count=8`，8 次同一地址）；
* **分模式契约**（`pin_tool_tf_observation`，本轮新增，**真断言非跳过**）：
  * `virtualize`：目标 0 次真实单步 + **读回 TF 仍为置位**（虚拟化的假象成立）+
    工具明说没有真实单步 + 退出码 0；
  * `watch`：目标按预期死在 `STATUS_SINGLE_STEP`（退出码 `-2147483644` = `0x80000004`）
    —— 预期一旦改变，这条会红并提示同步更新文档。
