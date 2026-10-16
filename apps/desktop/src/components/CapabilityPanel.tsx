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
