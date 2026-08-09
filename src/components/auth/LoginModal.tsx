import { useEffect, useState } from "react";
import { Check, CircleAlert, ExternalLink, Loader2, X } from "lucide-react";
import { authApi } from "../../lib/api";
import type { AuthLoginSessionStatus, AuthMutationResult } from "../../types";

const steps = [
  "启动本地回调服务器",
  "打开浏览器授权",
  "等待浏览器完成登录",
  "换令牌并入库",
  "同步模型列表",
];

export function LoginModal({ onClose, onCompleted }: { onClose: () => void; onCompleted: (result: AuthMutationResult) => void }) {
  const [state, setState] = useState<"idle" | "running" | "done" | "error">("idle");
  const [currentStep, setCurrentStep] = useState(-1);
  const [error, setError] = useState<string | null>(null);
  const [sessionId, setSessionId] = useState<string | null>(null);

  useEffect(() => {
    if (!sessionId || state !== "running") return;
    let disposed = false;
    const apply = (status: AuthLoginSessionStatus) => {
      if (disposed) return;
      const step = { listener: 0, browser: 1, callback: 2, saving: 3, syncing: 4 }[status.step ?? "listener"];
      setCurrentStep(step);
      if (status.state === "succeeded" && status.result) {
        setState("done");
        onCompleted(status.result);
      } else if (status.state === "cancelled") {
        setState("idle"); setSessionId(null); setCurrentStep(-1);
        setError(status.error ?? "登录已取消，可以重新开始。");
      } else if (status.state === "failed") {
        setState("error"); setSessionId(null); setError(status.error ?? "登录未完成，请重试。");
      }
    };
    const poll = async () => {
      try { apply(await authApi.loginStatus(sessionId)); }
      catch (_) { if (!disposed) { setState("error"); setSessionId(null); setError("无法查询登录状态，请重新开始登录。"); } }
    };
    void poll();
    const interval = window.setInterval(() => void poll(), 350);
    return () => { disposed = true; window.clearInterval(interval); };
  }, [sessionId, state, onCompleted]);

  const login = async () => {
    setState("running"); setCurrentStep(0); setError(null);
    try {
      const session = await authApi.loginStart("codex");
      setSessionId(session.sessionId);
    } catch (_) {
      setState("error"); setError("无法启动登录，请重试。");
    }
  };

  const cancel = async () => {
    const activeSession = sessionId;
    // Return to a retryable state immediately; the server tombstone prevents a
    // late callback from persisting credentials after this request.
    setState("idle"); setSessionId(null); setCurrentStep(-1); setError("登录已取消，可以重新开始。");
    if (activeSession) {
      try {
        const status = await authApi.loginCancel(activeSession);
        // Once the commit gate has opened, cancellation cannot honestly claim
        // that no account will be written. Resume status tracking instead.
        if (status.state === "saving" || status.state === "syncing") {
          setSessionId(activeSession); setState("running");
          setError("账号正在保存，无法安全取消；请等待当前操作完成。");
        }
      }
      catch (_) { setError("取消请求未确认，请稍后重新打开登录窗口。"); }
    }
  };

  return <div className="fixed inset-0 z-50 flex items-center justify-center bg-foreground/35 p-4" role="dialog" aria-modal="true" aria-labelledby="login-auth-title">
    <div className="surface w-full max-w-lg rounded-[24px] p-6 shadow-2xl"><div className="flex items-start justify-between"><div><h2 id="login-auth-title" className="text-lg font-semibold">登录 Codex 账号</h2><p className="mt-1 text-sm text-muted-foreground">浏览器 OAuth 授权 · PKCE · 消耗订阅额度</p></div><button onClick={onClose} disabled={state === "running"} aria-label="关闭登录弹窗" className="rounded-lg p-1 text-muted-foreground hover:bg-muted"><X size={18} /></button></div>
      <ol className="mt-5 space-y-3" aria-label="登录步骤">{steps.map((step, index) => { const complete = state === "done" || index < currentStep; const active = state === "running" && index === currentStep; return <li key={step} className="flex items-center gap-3 rounded-xl border border-border bg-muted/35 px-3 py-2.5 text-sm"><span className={`flex h-6 w-6 shrink-0 items-center justify-center rounded-full text-xs font-semibold ${complete ? "bg-success text-white" : active ? "bg-primary text-primary-foreground" : "bg-card text-muted-foreground"}`}>{complete ? <Check size={14} /> : active ? <Loader2 size={14} className="animate-spin" /> : index + 1}</span><span className={active ? "font-medium" : "text-muted-foreground"}>{step}</span></li>; })}</ol>
      {state === "running" && <p className="mt-4 flex items-center gap-2 rounded-xl bg-primary/10 px-3 py-2.5 text-sm text-primary"><ExternalLink size={15} />已在浏览器打开授权页，请在浏览器完成登录</p>}
      {error && <p role="alert" className="mt-4 flex items-center gap-2 rounded-xl bg-destructive/10 px-3 py-2.5 text-sm text-destructive"><CircleAlert size={15} />{error}</p>}
      {state === "done" && <p className="mt-4 rounded-xl bg-success/10 px-3 py-2.5 text-sm text-success">账号已保存。{currentStep === 4 ? "模型同步已完成。" : ""}</p>}
      <div className="mt-6 flex justify-end gap-2">{state === "running" ? <button onClick={() => void cancel()} className="action-secondary">取消登录</button> : state !== "done" && <button onClick={onClose} className="action-secondary">取消</button>}{state === "done" ? <button onClick={onClose} className="action-primary">完成</button> : <button onClick={() => void login()} disabled={state === "running"} className="action-primary">{state === "running" ? "登录中…" : "开始登录"}</button>}</div>
    </div>
  </div>;
}
