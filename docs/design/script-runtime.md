# 脚本运行时

## 为什么是脚本，而不是"更多工具接口"

传统"调试器 + MCP"方案把能力做成一个个固定函数（读内存、读寄存器、下断点……）。
AI 只能在这些函数的输入输出空间里排列组合。遇到这样的任务就断了：

> 脱壳：先观测外壳行为 → 判断 OEP 大致在哪 → 根据 OEP 处内存属性决定
> Dump 时机 → 再修正属性 → 验证 dump 出的 PE 能不能加载。

这不是"几个函数调用"能表达的，它是**带分支和回退的流程**。
所以 PinBridge 的主要工作方式是：**AI 写一段脚本，注入目标进程执行**。
MCP 工具层刻意保持薄（开门、观察、收尾）。

好处不只是"自由度更高"：

* 判断逻辑在**目标进程内**执行，不需要把上下文一轮轮搬回 LLM；
* 中间的循环/试错不消耗 token；
* 脚本自带状态（可以记住上一次命中的地址），不需要往 LLM 上下文里塞。

## 运行时选择：内嵌 CPython

| 候选 | 取舍 |
|------|------|
| **内嵌 CPython** | ✅ 采用。生态成熟、AI 生成质量最高、调试体验好 |
| JS（Frida 路线） | 生成质量尚可，但二进制处理的库生态弱于 Python |
| Lua | 体积小，但 AI 生成的正确率明显更低 |
| WASM | 隔离性好，但拿不到指针，做不了插装侧的活儿 |

代价必须写清楚：**内嵌 CPython 会把解释器带进目标进程**。
具体影响：

* 目标进程的内存占用增加（几十 MB 量级）；
* `os`/`subprocess`/`ctypes` 这类模块必须被**禁用**（见下文沙箱）；
* 目标可能通过"进程里多了解释器"这类特征做检测（属于对抗范畴）。

## 脚本形态

```python
# 目标进程内运行，由 PinBridge 提供的 `pb` 模块提供接口
import pb

def on_syscall(ev):
    # ev.args[0] 是系统调用号；这里只在改内存属性时记一笔
    if ev.args[0] == 0x50 and ev.args[1] & 0x20:
        pb.log("potential oep at 0x%x" % ev.address)
        pb.mark("oep_candidate", ev.address)

def on_mem_write(ev):
    if ev.address >= 0x7FF6_0000_3000:
        pb.snapshot(ev.address, 0x1000)   # 记录一段内存到轨迹

pb.subscribe("syscall_enter", on_syscall)
pb.subscribe("mem_write", on_mem_write)
```

接口分三组（M1 落地）：

| 组 | 接口 | 说明 |
|----|------|------|
| 订阅 | `subscribe(kind, callback)` | 声明关心的时机；**订阅集决定开销** |
| 输出 | `log(msg)` / `mark(tag, value)` | 写入 `script_output` 事件（会进轨迹） |
| 采集 | `snapshot(addr, len)` / `read(addr, len)` / `dump(path)` | 需要对应能力 |

## 生命周期

```text
Load ──▶ Active ──▶ Superseded ──▶ Unloaded
           │                │
           └──▶ TimedOut ◀──┘（超时或异常）
```

* **同名重载**：旧版标记为 `Superseded`，**不立即卸载**。
  它可能还有在途回调，强行清掉会在目标进程里留下不一致的中间状态。
* **超时**：默认 `SCRIPT_TIME_BUDGET_MS = 5000`。超时后脚本被标记 `TimedOut`
  并强制卸载——脚本里的死循环会直接冻结目标，这是安全阀不是性能优化。
* **脚本哈希**：每次加载都算 sha256 并写进溯源。回答"当前跑的是哪一版"时，
  哈希比"文件名 + 时间"可靠得多。

## 沙箱：必须禁掉的东西

脚本是**AI 生成的**，它可能出于好意写出危险代码（例如 `os.system("rm -rf")`），
也可能在探索时意外触达。因此 `pb` 运行时的策略是**默认拒绝**：

| 禁止 | 理由 |
|------|------|
| `os` / `subprocess` / `shutil` | 目标进程内的外部命令执行没有任何合法用途 |
| `ctypes` / `cffi` | 绕过我们所有的能力校验直接调系统 API |
| `socket` / `urllib` / `http` | 下传数据与回连；本项目的立场是不引入网络栈 |
| `import` 任意模块 | 只允许白名单（`pb`、`math`、`struct`、`json`、`re`、`binascii`…） |
| `open()` 写模式 | 只有 `pb.snapshot`/`pb.dump` 能在受控路径下写文件 |

**明确说清限制**：这是一个**策略性沙箱**，不是安全边界。
内嵌解释器里做"真正的沙箱"在已知的攻击面前不成立。
它的目的是**防意外**（AI 写出危险代码、探索时误触），
不是防一个已经拿到目标进程控制权的攻击者——那种情况下对方本来就已经赢了。

## 事件分发模型

```text
探针命中
   │
   ├─▶ 构造 Event（序号由 pb_allocate_sequence 分配）
   ├─▶ 折叠器 → .pbt（若在录制）
   ├─▶ 有界通道（满了按 DropOldest 丢弃并计数）
   └─▶ 脚本调度：按订阅集匹配 → 回调
                       │
                       └─ 回调在**目标线程**上执行 ⇒ 必须极快返回
```

**最重要的一条纪律**：回调跑在目标线程上，长时间占用会让目标卡住。
所以：

* 回调里只做"判断 + 记一笔"，重活（扫描、dump、解密）放到下一次 `pause` 之后；
* 我们提供 `pb.defer(fn)` 把工作排到安全的执行点（M1）；
* 单次回调超过预算会被记 `script_output` 警告，并在 summary 里体现。

## M0 现状

| 部分 | 状态 |
|------|------|
| 脚本注册表、sha256 溯源、Superseded/Unloaded/TimedOut 状态机 | ✅ `pinbridge-hub::script` |
| 加载路径的能力校验与 epoch 校验 | ✅ `HubService::load_script` |
| C ABI 入口 `pb_load_script`（先校验能力再报 not_supported） | ✅ 桩 |
| 内嵌 CPython 宿主、`pb` 模块、回调调度 | ⏳ M1 |
| 沙箱白名单与 `pb.defer` | ⏳ M1 |
