import { useEffect } from "react";
import { AlertTriangle } from "lucide-react";

// 协议/供应商切换确认框（设计 3.3）。
// 仅当 URL / 模型 / 端点已编辑（存在连接参数）时弹出。两个动作：
//   - 应用预设：重置 URL、端点和建议模型；API Key、备注、映射、priority、weight、timeout 保留。
//   - 仅切换标识：保留当前连接参数（URL/模型/端点/Key），只改协议/提供商身份。
export function ConfirmSwitchDialog({
  onApply,
  onKeep,
  onCancel,
}: {
  onApply: () => void;
  onKeep: () => void;
  onCancel: () => void;
}) {
  // Q4：Escape 取消；自动聚焦主操作按钮。
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => { if (e.key === "Escape") onCancel(); };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onCancel]);

  return (
    <div className="fixed inset-0 z-[70] flex items-center justify-center bg-black/60 p-4 backdrop-blur-sm" onClick={onCancel}>
      <div className="surface w-full max-w-md rounded-[28px] p-6" onClick={e => e.stopPropagation()}>
        <div className="flex items-center gap-3">
          <div className="rounded-2xl border border-amber-200 bg-amber-50 p-2.5">
            <AlertTriangle className="h-5 w-5 text-amber-600" />
          </div>
          <div>
            <h3 className="text-base font-semibold">切换会应用预设</h3>
            <p className="text-sm text-muted-foreground">连接参数已被编辑</p>
          </div>
        </div>

        <div className="mt-4 rounded-2xl border border-border bg-background/50 px-4 py-3 text-sm leading-6 text-slate-600">
          应用预设会重置 <span className="font-medium text-foreground">URL、端点和建议模型</span>；
          <span className="font-medium text-foreground">API Key、备注、模型映射、优先级、权重和超时</span> 保留。
        </div>

        <div className="mt-5 flex flex-col gap-2">
          <button type="button" onClick={onApply} autoFocus className="action-primary w-full justify-center">
            应用预设
          </button>
          <button type="button" onClick={onKeep} className="action-secondary w-full justify-center">
            仅切换标识，保留当前连接参数
          </button>
          <button type="button" onClick={onCancel} className="mt-1 text-sm text-muted-foreground transition-colors hover:text-foreground">
            取消
          </button>
        </div>
      </div>
    </div>
  );
}
