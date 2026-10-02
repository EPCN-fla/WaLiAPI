import { AlertCircle, Check, ExternalLink, Loader2, Ticket, X } from "lucide-react";
import { useEffect, useMemo, useState } from "react";
import type { AuthAccount, AuthResetCredit, AuthResetCreditsSnapshot, AuthResetOperationResult } from "../../types";

type Step = "select" | "confirm" | "success" | "error";

interface Props {
  account: AuthAccount;
  onClose: () => void;
  listCredits: (id: string) => Promise<AuthResetCreditsSnapshot>;
  consumeCredit: (id: string, creditId: string) => Promise<AuthResetOperationResult>;
  onCompleted: () => Promise<void> | void;
}

function localDate(value: string | null) {
  if (value === null) return "未提供";
  const numeric = Number(value);
  const date = new Date(Number.isFinite(numeric) && /^\d+$/.test(value) ? numeric * 1000 : value);
  return Number.isNaN(date.getTime()) ? "未知" : date.toLocaleString("zh-CN", { hour12: false });
}

function availableCredit(credit: AuthResetCredit) {
  if (credit.status.toLowerCase() !== "available" || credit.resetType !== "codex_rate_limits") return false;
  if (credit.expiresAt === null) return true;
  const numeric = Number(credit.expiresAt);
  const timestamp = Number.isFinite(numeric) && /^\d+$/.test(credit.expiresAt) ? numeric * 1000 : Date.parse(credit.expiresAt);
  return Number.isFinite(timestamp) && timestamp > Date.now();
}

function outcomeText(code: string | null) {
  if (code === "nothing_to_reset") return "当前账号没有可重置的额度窗口。";
  if (code === "no_credit") return "这张重置卡已不可用，卡列表已刷新。";
  if (code === "already_redeemed") return "该重置卡已经使用过，额度状态已重新读取。";
  return "重置卡消费没有完成，请打开官方用量页核对。";
}

export function ResetCreditDialog({ account, onClose, listCredits, consumeCredit, onCompleted }: Props) {
  const [step, setStep] = useState<Step>("select");
  const [snapshot, setSnapshot] = useState<AuthResetCreditsSnapshot | null>(null);
  const [selected, setSelected] = useState<AuthResetCredit | null>(null);
  const [result, setResult] = useState<AuthResetOperationResult | null>(null);
  const [loading, setLoading] = useState(true);
  const [consuming, setConsuming] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [errorStage, setErrorStage] = useState<"list" | "consume" | null>(null);

  useEffect(() => {
    let disposed = false;
    setLoading(true);
    listCredits(account.id)
      .then((value) => { if (!disposed) setSnapshot(value); })
      .catch((reason) => { if (!disposed) { setErrorStage("list"); setError(typeof reason === "string" ? reason : "重置卡查询失败，请打开官方用量页核对。"); setStep("error"); } })
      .finally(() => { if (!disposed) setLoading(false); });
    return () => { disposed = true; };
  }, [account.id, listCredits]);

  const credits = useMemo(() => (snapshot?.credits ?? []).filter(availableCredit), [snapshot]);
  const close = () => { if (!consuming) onClose(); };

  const confirm = async () => {
    if (!selected || consuming) return;
    setConsuming(true);
    setError(null);
    try {
      const next = await consumeCredit(account.id, selected.id);
      setResult(next);
      if (next.code === "reset" || next.code === "already_redeemed") {
        setStep("success");
        await onCompleted();
      } else {
        setErrorStage("consume");
        setStep("error");
      }
    } catch (reason) {
      setErrorStage("consume");
      setError(typeof reason === "string" ? reason : "重置请求结果未知，请稍后查看操作状态，不要重复提交。");
      setStep("error");
    } finally {
      setConsuming(false);
    }
  };

  return <div className="fixed inset-0 z-50 flex items-center justify-center bg-slate-950/55 p-4" role="dialog" aria-modal="true" aria-labelledby="reset-credit-title">
    <div className="w-full max-w-[640px] rounded-[24px] border border-white/70 bg-white p-6 shadow-2xl">
      <div className="flex items-start justify-between gap-4">
        <div><h2 id="reset-credit-title" className="text-xl font-semibold text-slate-900">{step === "select" ? "选择重置卡" : step === "confirm" ? "确认使用重置卡" : step === "success" ? "重置成功" : "重置未完成"}</h2><p className="mt-1 text-sm text-slate-500">账号：{account.email || account.label || account.account_id}</p></div>
        <button onClick={close} disabled={consuming} aria-label="关闭重置卡弹窗" className="rounded-lg p-1.5 text-slate-400 hover:bg-slate-100 disabled:opacity-40"><X size={20} /></button>
      </div>

      {step === "select" && <>
        <p className="mt-5 text-sm text-slate-500">从该账号下的可用重置卡中选择一张使用。</p>
        {loading ? <div className="flex min-h-40 items-center justify-center text-sm text-slate-500"><Loader2 size={18} className="mr-2 animate-spin" />正在查询重置卡…</div> : credits.length === 0 ? <div className="mt-5 rounded-2xl border border-dashed border-slate-200 p-7 text-center text-sm text-slate-500">当前没有可用的 Codex 重置卡。<a className="mt-2 inline-flex items-center gap-1 text-emerald-600 hover:underline" href={snapshot?.fallbackUrl} target="_blank" rel="noreferrer">打开官方用量页<ExternalLink size={13} /></a></div> : <div className="mt-5 space-y-2">{credits.map((credit) => <button key={credit.id} type="button" onClick={() => setSelected(credit)} className={`flex w-full items-center gap-3 rounded-2xl border p-3 text-left transition ${selected?.id === credit.id ? "border-emerald-400 bg-emerald-50/60" : "border-slate-200 hover:border-emerald-200 hover:bg-slate-50"}`}><span className={`flex h-10 w-10 items-center justify-center rounded-xl ${selected?.id === credit.id ? "bg-emerald-100 text-emerald-600" : "bg-sky-50 text-sky-600"}`}><Ticket size={20} /></span><span className="min-w-0 flex-1"><span className="block text-sm font-semibold text-slate-800">{credit.title || "Codex 重置卡"}</span><span className="mt-1 block text-xs text-slate-500">适用于 Codex · {credit.description || "重置当前额度窗口"}</span></span><span className="text-right text-xs text-slate-500">{credit.expiresAt ? `${localDate(credit.expiresAt)} 过期` : "长期有效"}</span>{selected?.id === credit.id && <Check size={18} className="text-emerald-600" />}</button>)}</div>}
        <div className="mt-6 flex justify-end gap-2"><button onClick={close} className="action-secondary">取消</button><button onClick={() => selected && setStep("confirm")} disabled={!selected || loading} className="action-primary disabled:opacity-45">下一步</button></div>
      </>}

      {step === "confirm" && selected && <>
        <div className="mt-5 rounded-2xl bg-slate-50 p-4"><div className="flex items-center gap-3"><span className="flex h-10 w-10 items-center justify-center rounded-xl bg-emerald-100 text-emerald-600"><Ticket size={20} /></span><div><p className="font-semibold text-slate-800">{selected.title || "Codex 重置卡"}</p><p className="text-xs text-slate-500">适用于 Codex · 剩余 1 次</p></div></div><dl className="mt-4 grid grid-cols-[90px_1fr] gap-y-2 text-sm"><dt className="text-slate-500">过期时间</dt><dd className="text-slate-800">{localDate(selected.expiresAt)}</dd><dt className="text-slate-500">获得时间</dt><dd className="text-slate-800">{localDate(selected.grantedAt)}</dd></dl></div><div className="mt-4 flex items-start gap-2 rounded-xl bg-amber-50 px-3 py-2.5 text-sm text-amber-700"><AlertCircle size={17} className="mt-0.5 shrink-0" />使用后会立即重置该账号的 Codex 额度，请确认使用。</div><div className="mt-6 flex justify-end gap-2"><button onClick={() => setStep("select")} disabled={consuming} className="action-secondary">返回</button><button onClick={() => void confirm()} disabled={consuming} className="action-primary min-w-28 justify-center">{consuming && <Loader2 size={16} className="animate-spin" />}确认使用</button></div></>}

      {step === "success" && result && <div className="py-8 text-center"><div className="mx-auto flex h-16 w-16 items-center justify-center rounded-full bg-emerald-100 text-emerald-600"><Check size={32} /></div><h3 className="mt-4 text-xl font-semibold text-slate-900">额度已重置</h3><p className="mt-2 text-sm text-slate-500">该账号可以继续使用 Codex。</p>{result.quotaRefreshStatus === "failed" && <p className="mt-2 text-xs text-amber-600">额度回读暂未完成，可稍后点击“刷新额度”。</p>}<button onClick={close} className="action-primary mt-7 min-w-28 justify-center">我知道了</button></div>}

      {step === "error" && <div className="py-7 text-center"><div className="mx-auto flex h-14 w-14 items-center justify-center rounded-full bg-amber-100 text-amber-600"><AlertCircle size={28} /></div><h3 className="mt-4 text-lg font-semibold text-slate-900">{result ? outcomeText(result.code) : errorStage === "list" ? "重置卡查询失败" : "重置未完成"}</h3><p className="mx-auto mt-2 max-w-md text-sm leading-6 text-slate-500">{error || outcomeText(result?.code ?? null)}</p>{(result?.fallbackUrl || snapshot?.fallbackUrl) && <a className="mt-3 inline-flex items-center gap-1 text-sm text-emerald-600 hover:underline" href={result?.fallbackUrl || snapshot?.fallbackUrl} target="_blank" rel="noreferrer">打开官方用量页<ExternalLink size={13} /></a>}<div className="mt-7 flex justify-center"><button onClick={close} className="action-secondary">关闭</button></div></div>}
    </div>
  </div>;
}
