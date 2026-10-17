/**
 * 能力矩阵。
 *
 * 展示的是"当前主体持有什么"，也就是 AI 现在能做什么。
 * 这是人类复核 AI 行为的**第一落点**：看到 `script_load` 亮着，
 * 就知道它能在目标进程里跑代码。
 *
 * 注意：本组件只展示。授予能力是 `pb_capability_grant` 的活儿，
 * 只能由人类发起（AI 给自己扩权在 Hub 侧会被直接拒绝）。
 */
import { formatCapabilityBits } from "../api/hub";

export interface CapabilityPanelProps {
  names: string[];
  bits: number;
}

export function CapabilityPanel({ names, bits }: CapabilityPanelProps) {
  // 没有名字就没有会话：**说清"没接上"**，不要显示一个"能力集 0x0000"让人以为
  // "这个会话什么都没被授予" —— 那是另一回事（而且同样会误导）。
  if (names.length === 0) {
    return (
      <div className="capabilities">
        <span className="capabilities__title">能力集：尚未接上会话</span>
        <span className="capabilities__hint">
          这里展示"当前主体持有什么"，来源是会话的授权记录（`pb_capability_grant`）。
          没有加载轨迹时它没有可显示的内容。
        </span>
      </div>
    );
  }
  return (
    <div className="capabilities">
      <span className="capabilities__title">能力集 {formatCapabilityBits(bits)}</span>
      <ul className="capabilities__list">
        {names.map((name) => (
          <li key={name} className={`capabilities__item ${name === "script_load" ? "capabilities__item--strong" : ""}`}>
            {name}
          </li>
        ))}
      </ul>
      <span className="capabilities__hint">
        `script_load` 是最强的一档：它允许在目标进程内执行 AI 生成的代码。
      </span>
    </div>
  );
}
