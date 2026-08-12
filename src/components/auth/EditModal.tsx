import { useEffect, useState } from "react";
import { Plus, Save, X } from "lucide-react";
import type { AuthAccount } from "../../types";
import { MappingRow } from "../channel-form/MappingRow";

export function EditModal({ account, pending, onClose, onSave }: { account: AuthAccount; pending: boolean; onClose: () => void; onSave: (input: Pick<AuthAccount, "id" | "label" | "priority" | "weight" | "model_mapping">) => Promise<void> }) {
  const [label, setLabel] = useState(account.label);
  const [priority, setPriority] = useState(String(account.priority));
  const [weight, setWeight] = useState(String(account.weight));
  const [error, setError] = useState<string | null>(null);

  // Model mappings: array of { from, to } pairs
  const initialMappings = account.model_mapping
    ? Object.entries(account.model_mapping).flatMap(([from, to]) => {
        const targets = Array.isArray(to) ? to : [to];
        return targets.map(t => ({ from, to: t }));
      })
    : [];
  const [mappings, setMappings] = useState<{ from: string; to: string }[]>(initialMappings);

  // Available target models = auth account's synced models
  const availableTargets = account.models.map(m => m.id);
  const existingFroms = Array.from(new Set(mappings.map(m => m.from).filter(Boolean))).sort();

  useEffect(() => {
    const listener = (event: KeyboardEvent) => event.key === "Escape" && onClose();
    document.addEventListener("keydown", listener);
    return () => document.removeEventListener("keydown", listener);
  }, [onClose]);

  const addMapping = () => setMappings(prev => [...prev, { from: "", to: "" }]);
  const removeMapping = (idx: number) => setMappings(prev => prev.filter((_, i) => i !== idx));
  const updateMapping = (idx: number, field: "from" | "to", value: string) =>
    setMappings(prev => prev.map((m, i) => i === idx ? { ...m, [field]: value } : m));

  const submit = async (event: React.FormEvent) => {
    event.preventDefault();
    const nextPriority = Number(priority);
    const nextWeight = Number(weight);
    if (!label.trim()) return setError("账号名称不能为空");
    if (!Number.isInteger(nextPriority) || nextPriority < 0) return setError("优先级必须是不小于 0 的整数");
    if (!Number.isInteger(nextWeight) || nextWeight < 1) return setError("权重必须是不小于 1 的整数");

    // Build model_mapping object from mappings array
    const modelMapping: Record<string, string | string[]> = {};
    const validMappings = mappings.filter(m => m.from.trim() && m.to.trim());
    validMappings.forEach(m => {
      const from = m.from.trim();
      if (modelMapping[from]) {
        // If key already exists, convert to array
        const existing = modelMapping[from];
        if (Array.isArray(existing)) {
          if (!existing.includes(m.to)) existing.push(m.to);
        } else {
          modelMapping[from] = existing !== m.to ? [existing, m.to] : existing;
        }
      } else {
        modelMapping[from] = m.to;
      }
    });

    setError(null);
    await onSave({ id: account.id, label: label.trim(), priority: nextPriority, weight: nextWeight, model_mapping: modelMapping });
  };

  return <div className="fixed inset-0 z-50 flex items-center justify-center bg-foreground/35 p-4" role="dialog" aria-modal="true" aria-labelledby="edit-auth-title">
    <form onSubmit={submit} className="surface w-full max-w-md rounded-[24px] p-6 shadow-2xl max-h-[90vh] overflow-y-auto">
      <div className="flex items-start justify-between gap-3"><div><h2 id="edit-auth-title" className="text-lg font-semibold">编辑 Auth 账号</h2><p className="mt-1 text-sm text-muted-foreground">{account.email || account.account_id} · plan: {account.plan_type || "未知"} · 账号级限额</p></div><button type="button" onClick={onClose} aria-label="关闭编辑弹窗" className="rounded-lg p-1 text-muted-foreground hover:bg-muted"><X size={18} /></button></div>
      <div className="mt-5 space-y-4">
        <label className="block text-sm font-medium">账号名称<input value={label} onChange={event => setLabel(event.target.value)} className="mt-1.5 w-full rounded-xl border border-border px-3 py-2.5" autoFocus /></label>
        <div className="grid grid-cols-2 gap-3">
          <div>
            <label className="mb-2 block text-sm font-medium">优先级</label>
            <input value={priority} onChange={event => setPriority(event.target.value)} type="number" min="0" step="1" className="mt-1.5 w-full rounded-xl border border-border px-3 py-2.5" />
            <p className="mt-1.5 text-xs text-muted-foreground">数字越大优先级越高</p>
          </div>
          <div>
            <label className="mb-2 block text-sm font-medium">权重</label>
            <input value={weight} onChange={event => setWeight(event.target.value)} type="number" min="1" step="1" className="mt-1.5 w-full rounded-xl border border-border px-3 py-2.5" />
            <p className="mt-1.5 text-xs text-muted-foreground">同优先级间的负载比例</p>
          </div>
        </div>

        {/* 模型映射 */}
        <div>
          <div className="mb-2 flex items-center justify-between">
            <label className="text-sm font-medium">模型映射</label>
            <span className="text-xs text-muted-foreground">左侧填映射名，右侧选账号实际模型</span>
          </div>
          {mappings.length === 0 ? (
            <div className="rounded-2xl border border-dashed border-border bg-background/40 px-4 py-6 text-center">
              <p className="text-sm text-muted-foreground mb-3">尚未配置模型映射</p>
              <button type="button" onClick={addMapping} disabled={availableTargets.length === 0} className="action-secondary inline-flex items-center gap-1.5 disabled:opacity-40 disabled:cursor-not-allowed">
                <Plus size={14} /> 添加映射
              </button>
            </div>
          ) : (
            <div className="space-y-2.5">
              {mappings.map((map, idx) => (
                <MappingRow
                  key={idx}
                  from={map.from}
                  to={map.to}
                  availableTargets={availableTargets}
                  existingFroms={existingFroms}
                  onRemove={() => removeMapping(idx)}
                  onChange={(field, value) => updateMapping(idx, field, value)}
                />
              ))}
              <button type="button" onClick={addMapping} disabled={availableTargets.length === 0} className="action-secondary inline-flex items-center gap-1.5 disabled:opacity-40 disabled:cursor-not-allowed">
                <Plus size={14} /> 添加映射
              </button>
            </div>
          )}
        </div>

        {error && <p role="alert" className="text-sm text-destructive">{error}</p>}
      </div>
      <div className="mt-6 flex justify-end gap-2"><button type="button" onClick={onClose} className="action-secondary">取消</button><button disabled={pending} className="action-primary">{pending ? "保存中…" : <><Save size={16} />保存</>}</button></div>
    </form>
  </div>;
}
