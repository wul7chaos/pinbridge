# 已知缺陷：`-pb_tf watch` 下靶子跑死（附带：`--tf-set` 任何模式都死）

- 发现于：第 45 轮（`--tf-set`）、第 47 轮（`--tf-threads` + `watch`）
- 状态：**未修**。`scripts/check-tf-location.ps1` 的第二段因此按设计 **exit 77 跳过**；
  缺陷修好后那段会自动变成真正的地址比对断言，脚本不用改。
- 相关：`native/tests/sample_target.cpp`（靶子）、`native/src/pinbridge_tool.cpp`（工具）

## 现象（实测）

| 场景 | 结果 |
|------|------|
| `--tf-threads 3` + `-pb_tf virtualize` | ✅ 正常：靶子跑完并打印 `PB_TF_THREADS rounds=3 …`；`PB_TF_EXC count=0`（virtualize 的目的就是让目标看不到单步） |
| `--tf-threads 3` + `-pb_tf watch` | ✗ **靶子跑死**：Pin 的输出停在 `入口点命中 0x…（序号 140，线程 0）`，之后**没有任何目标输出**（`PB_TF_THREADS`、`PB_TF_EXC` 一行都没有） |
| `--tf-set 8` | ✗ 任何模式都死（`watch` / `off` / 不带 `-pb_tf` 都一样）——所以它与 `-pb_tf` 的取值无关 |

"跑死"的判据：退出码非 0，且**目标侧一行输出都没有** —— 这与"目标完成了但没打印"必须分开，
否则会把假失败当成真结论（本项目在别处也踩过这个坑）。

## 为什么这条缺陷挡住了"异常位置比对"

那条判据要的是：**同一次**单步异常，工具说它发生在 X，靶子自己读到 `ExceptionAddress` = Y，验 X == Y。

* `virtualize`：工具把真实单步**消化掉**了 ⇒ 靶子根本看不到异常 ⇒ 没有可比的 Y；
* `watch`：本该两侧都看到 ⇒ 但靶子跑死 ⇒ 拿不到 Y。

所以"位置一致"这条目前**无法验证**，不是没实现。修掉 watch 模式的崩溃之后即可验证。

## 下一步（排查建议）

1. 把 `-pb_tf watch` 与 `virtualize` 下**工具侧的插装动作**逐项对照 —— `virtualize` 会做
   "剥离真实 TF + 补回 pushfq 栈上副本"，而 `watch` 只观察；差异最可能在**观察动作本身**
   （例如某个插装点在 watch 下改了状态或时序）；
2. 先确认靶子死在**哪一条指令**：在 `watch` 下开工具已有的有界诊断（只打前几次回调），
   看最后一条被插桩的指令；
3. 一个小实验就能缩小范围：把靶子的 TF 实验缩到 **1 轮**，看是否仍在入口点之后立刻死。

## 已经做到、且不依赖这条缺陷的部分

* 工具侧新增 `TF 异常位置：…` 一行（`virtualize` 下明确报"没有观察到真实单步"）——
  已由既有用例 `pin_tool_tf_per_thread` 的输出覆盖，并被 `pin_tool_tf_location` 断言；
* 靶子侧会报出它自己读到的异常位置（`PB_TF_EXC count=N addr0=0x…`），
  单独运行时可验证：`count=8 addr0..7=0x7FF6EEFE1C9E`（8 次同一地址，符合预期）；
* `pin_tool_tf_location`（脚本驱动，`SKIP_RETURN_CODE 77`）已经落地：`virtualize` 段是真断言，
  `watch` 段在缺陷存在时明确跳过并打印现象。
