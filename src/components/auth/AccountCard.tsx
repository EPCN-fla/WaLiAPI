import { useEffect, useState } from "react";
import { Check, Download, Edit3, KeyRound, Loader2, Power, RefreshCw, RotateCw, Trash2, X } from "lucide-react";
import type { AuthAccount, AuthModelState } from "../../types";

// 统一 chip 尺寸：+N 与模型 id 完全一致，文字水平垂直居中，样式对齐底部「P0 · W1」等 pill
const chipBase =
  "inline-flex items-center justify-center gap-1 rounded-full bg-muted px-2 py-1 text-[10px] text-muted-foreground";
import { QuotaBlock } from "./QuotaBlock";

function formatTime(value: string | null) {
  if (!value) return "未同步";
  const date = new Date(value);
  return Number.isNaN(date.getTime()) ? "未知" : date.toLocaleString("zh-CN", { month: "numeric", day: "numeric", hour: "2-digit", minute: "2-digit" });
}

function AccountStatus({ account }: { account: AuthAccount }) {
  if (account.status === "invalid") return <span className="inline-flex items-center gap-1 rounded-full bg-destructive/10 px-2 py-0.5 text-[11px] font-semibold text-destructive"><span className="h-1.5 w-1.5 rounded-full bg-destructive" />已失效</span>;
  if (account.disabled) return <span className="inline-flex items-center gap-1 rounded-full bg-muted px-2 py-0.5 text-[11px] font-semibold text-muted-foreground"><span className="h-1.5 w-1.5 rounded-full bg-muted-foreground" />已停用</span>;
  if (account.quota?.exceeded) return <span className="inline-flex items-center gap-1 rounded-full bg-warning/15 px-2 py-0.5 text-[11px] font-semibold text-warning"><span className="h-1.5 w-1.5 rounded-full bg-warning" />已踢出路由</span>;
  return <span className="inline-flex items-center gap-1 rounded-full bg-success/10 px-2 py-0.5 text-[11px] font-semibold text-success"><span className="h-1.5 w-1.5 rounded-full bg-success" />正常</span>;
}

export function AccountCard({ account, pending, onEdit, onToggle, onDelete, onRefresh, onSync, onWriteBack, onRelogin }: { account: AuthAccount; pending: boolean; onEdit: () => void; onToggle: () => void; onDelete: () => void; onRefresh: () => void; onSync: () => void; onWriteBack: () => void; onRelogin: () => void }) {
  const invalid = account.status === "invalid";
  const [showModels, setShowModels] = useState(false);
  return <article className="surface rounded-[24px] p-5 transition-shadow hover:shadow-lg" aria-label={`${account.label} 认证账号`}>
    <header className="flex items-start gap-3"><div className="flex h-11 w-11 shrink-0 items-center justify-center rounded-2xl bg-success text-lg font-bold text-white shadow-sm">⌘</div><div className="min-w-0 flex-1"><div className="flex flex-wrap items-center gap-2"><h2 className="truncate font-semibold">{account.label}</h2><AccountStatus account={account} />{account.plan_type && <span className="rounded-full border border-border bg-muted px-2 py-0.5 text-[11px] text-muted-foreground">{account.plan_type}</span>}</div><p className="mt-1 truncate text-xs text-muted-foreground">{account.email || account.account_id}</p></div><div className="flex shrink-0 items-center gap-1"><button onClick={onEdit} disabled={pending} title="编辑账号" aria-label="编辑账号" className="rounded-lg p-1.5 text-muted-foreground hover:bg-muted hover:text-foreground"><Edit3 size={16} /></button><button onClick={onToggle} disabled={pending} title={account.disabled ? "启用账号" : "停用账号"} aria-label={account.disabled ? "启用账号" : "停用账号"} className="rounded-lg p-1.5 text-muted-foreground hover:bg-muted hover:text-foreground"><Power size={16} /></button><button onClick={onDelete} disabled={pending} title="删除账号" aria-label="删除账号" className="rounded-lg p-1.5 text-destructive hover:bg-destructive/10"><Trash2 size={16} /></button></div></header>
    <div className="mt-4 space-y-3 border-y border-border py-4">{invalid ? <div className="rounded-xl border border-destructive/25 bg-destructive/10 p-3 text-sm"><p className="font-semibold text-destructive">令牌已失效 · 需重新登录</p><p className="mt-1 text-xs text-muted-foreground">自动刷新未成功；重新登录后才能恢复为路由候选。</p></div> : account.quota && <QuotaBlock quota={account.quota} />}</div>
    <section className="mt-4"><div className="flex items-center justify-between gap-3"><p className="text-xs font-medium">◎ 可用模型</p><span className="text-[11px] text-muted-foreground">登录/12h 自动同步 · 全量支持</span></div><div className="mt-2 flex flex-wrap items-center gap-1.5">{account.models.slice(0, 4).map(model => <ModelChip key={model.id} id={model.id} />)}{account.models.length > 4 && <button onClick={() => setShowModels(true)} className={`${chipBase} transition-colors hover:bg-muted/60 hover:text-foreground`} title="查看全部模型" aria-label="查看全部模型">+{account.models.length - 4}</button>}{account.models.length === 0 && <span className="text-xs text-muted-foreground">尚无模型快照，不参与路由</span>}</div></section>
    <div className="mt-4 flex flex-wrap gap-2 text-[11px] text-muted-foreground"><span className="rounded-full bg-muted px-2 py-1">P{account.priority} · W{account.weight}</span><span className="rounded-full bg-muted px-2 py-1">同步于 {formatTime(account.last_models_sync_at)}</span><span className="rounded-full bg-muted px-2 py-1">刷新 {formatTime(account.last_refreshed_at)}</span></div>
    <div className="mt-4 grid grid-cols-3 gap-2 border-t border-border pt-4">{invalid ? <button onClick={onRelogin} disabled={pending} className="col-span-3 action-primary justify-center"><KeyRound size={15} />重新登录</button> : null}<button onClick={onRefresh} disabled={pending} className="action-secondary justify-center px-2 py-2 text-xs">{pending ? <Loader2 size={14} className="animate-spin" /> : <RefreshCw size={14} />}刷新令牌</button><button onClick={onSync} disabled={pending} className="action-secondary justify-center px-2 py-2 text-xs"><RotateCw size={14} />同步模型</button><button onClick={onWriteBack} disabled={pending} className="action-secondary justify-center px-2 py-2 text-xs"><Download size={14} />写回 CLI</button></div>
    {showModels && <ModelsPopup models={account.models} onClose={() => setShowModels(false)} />}
  </article>;
}

function ModelChip({ id, maxWidth = "max-w-[10rem]" }: { id: string; maxWidth?: string }) {
  const [copied, setCopied] = useState(false);
  const copy = () => {
    navigator.clipboard
      .writeText(id)
      .then(() => { setCopied(true); window.setTimeout(() => setCopied(false), 1200); })
      .catch(() => {});
  };
  return (
    <button type="button" onClick={() => void copy()} title={id} aria-label={`复制模型 id ${id}`}
      className={`${chipBase} ${maxWidth} ${copied ? "bg-success/10 text-success" : "hover:bg-muted/60 hover:text-foreground"}`}>
      <span className="min-w-0 truncate">{id}</span>
      {copied && <Check size={11} className="shrink-0" />}
    </button>
  );
}

function ModelsPopup({ models, onClose }: { models: AuthModelState[]; onClose: () => void }) {
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => { if (event.key === "Escape") onClose(); };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);
  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center bg-foreground/35 p-4" role="dialog" aria-modal="true" aria-labelledby="models-popup-title" onClick={onClose}>
      <div className="surface w-full max-w-md rounded-[24px] p-6 shadow-2xl" onClick={(event) => event.stopPropagation()}>
        <div className="flex items-start justify-between gap-3">
          <h2 id="models-popup-title" className="text-lg font-semibold">全部模型 ({models.length})</h2>
          <button onClick={onClose} aria-label="关闭全部模型弹窗" className="rounded-lg p-1 text-muted-foreground hover:bg-muted"><X size={18} /></button>
        </div>
        <div className="mt-4 flex max-h-[60vh] flex-wrap gap-1.5 overflow-y-auto">
          {models.map((model) => <ModelChip key={model.id} id={model.id} maxWidth="max-w-full" />)}
        </div>
      </div>
    </div>
  );
}
