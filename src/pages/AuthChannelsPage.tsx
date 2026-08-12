import { useCallback, useEffect, useState } from "react";
import { CircleAlert, KeyRound, Loader2, Upload, X } from "lucide-react";
import { authApi } from "../lib/api";
import type { AuthAccount, AuthMutationResult } from "../types";
import { AccountCard } from "../components/auth/AccountCard";
import { EditModal } from "../components/auth/EditModal";
import { LoginModal } from "../components/auth/LoginModal";
import { ModelSyncModal } from "../components/auth/ModelSyncModal";
import { ProviderPills } from "../components/auth/ProviderPills";
import { ChannelTabs } from "../components/layout/ChannelTabs";

type Confirmation = { kind: "delete"; account: AuthAccount };

function exportFileName(account: AuthAccount) {
  const base = (account.label || account.email || account.account_id || "codex-auth")
    .replace(/[\\/:*?"<>|]/g, "-")
    .trim();
  return `${base || "codex-auth"}.json`;
}

function EmptyAccountSlot({ onLogin, onImport, busy }: { onLogin: () => void; onImport: () => void; busy: boolean }) {
  return <section className="flex min-h-80 flex-col items-center justify-center rounded-[24px] border border-dashed border-border bg-card/50 p-6 text-center"><div className="flex h-12 w-12 items-center justify-center rounded-2xl bg-success/10 text-xl font-bold text-success">⌘</div><h2 className="mt-4 font-semibold">＋ 登录 Codex 账号</h2><p className="mt-2 max-w-xs text-sm leading-6 text-muted-foreground">浏览器 OAuth 登录（PKCE）或从本机 ~/.codex/auth.json 导入</p><div className="mt-5 flex flex-wrap justify-center gap-2"><button onClick={onLogin} disabled={busy} className="action-primary"><KeyRound size={16} />登录</button><button onClick={onImport} disabled={busy} className="action-secondary"><Upload size={16} />导入</button></div></section>;
}

function ConfirmationDialog({ confirmation, pending, onCancel, onConfirm }: { confirmation: Confirmation; pending: boolean; onCancel: () => void; onConfirm: () => void }) {
  return <div className="fixed inset-0 z-50 flex items-center justify-center bg-foreground/35 p-4" role="dialog" aria-modal="true" aria-labelledby="auth-confirm-title"><div className="surface w-full max-w-md rounded-[24px] p-6 shadow-2xl"><div className="flex items-start justify-between gap-3"><div><h2 id="auth-confirm-title" className="text-lg font-semibold">删除 Auth 账号</h2><p className="mt-1 text-sm text-muted-foreground">{confirmation.account.label}</p></div><button onClick={onCancel} aria-label="关闭确认弹窗" className="rounded-lg p-1 text-muted-foreground hover:bg-muted"><X size={18} /></button></div><p className="mt-5 text-sm leading-6 text-muted-foreground">是否删除该账号？删除后此账号不再参与路由。仅从本应用移除，不影响本机 Codex CLI 登录态。</p><div className="mt-6 flex flex-wrap justify-end gap-2"><button onClick={onCancel} className="action-secondary">取消</button><button disabled={pending} onClick={onConfirm} className="inline-flex items-center gap-2 rounded-xl bg-destructive px-4 py-2.5 text-sm font-semibold text-destructive-foreground">{pending ? <Loader2 size={16} className="animate-spin" /> : null}确认删除</button></div></div></div>;
}

export function AuthChannelsPage() {
  const [accounts, setAccounts] = useState<AuthAccount[]>([]);
  const [loading, setLoading] = useState(true);
  const [pendingId, setPendingId] = useState<string | null>(null);
  const [showLogin, setShowLogin] = useState(false);
  const [editAccount, setEditAccount] = useState<AuthAccount | null>(null);
  const [syncAccount, setSyncAccount] = useState<AuthAccount | null>(null);
  const [confirmation, setConfirmation] = useState<Confirmation | null>(null);
  const [notice, setNotice] = useState<{ kind: "success" | "error" | "warning"; message: string } | null>(null);

  const load = useCallback(async () => {
    setLoading(true);
    try { setAccounts(await authApi.accountsList()); }
    catch (_) { setNotice({ kind: "error", message: "Auth 账号加载失败，请检查本地服务后重试。" }); }
    finally { setLoading(false); }
  }, []);
  useEffect(() => { void load(); }, [load]);

  const runFor = async (id: string, success: string, work: () => Promise<void>) => {
    setPendingId(id);
    try { await work(); setNotice({ kind: "success", message: success }); await load(); }
    catch (_) { setNotice({ kind: "error", message: "操作失败，请稍后重试。" }); }
    finally { setPendingId(null); }
  };

  const completeLogin = (result: AuthMutationResult) => {
    setShowLogin(false);
    setNotice(result.warning ? { kind: "warning", message: "账号已保存但暂不参与路由：模型同步失败。" } : { kind: "success", message: "Codex 账号登录完成。" });
    void load();
  };
  const importAuth = async () => {
    let path: string | null = null;
    try {
      const { open } = await import("@tauri-apps/plugin-dialog");
      let defaultPath: string | undefined;
      try {
        defaultPath = await authApi.defaultImportPath();
      } catch {
        // 默认路径解析失败(无 home)时回退为不带 defaultPath 弹框,仍可手动选文件
      }
      path = await open({
        title: "选择 Codex auth.json 文件",
        filters: [{ name: "Codex auth", extensions: ["json"] }],
        multiple: false,
        defaultPath,
      });
    } catch {
      // 对话框不可用,忽略(保持旧行为直接读默认路径)
    }
    if (path === null) return; // 用户取消 → 静默 no-op
    const label = path; // 实际选中路径
    setPendingId("import"); setNotice({ kind: "success", message: `正在读取 ${label} …` });
    try {
      const result = await authApi.loginImport("codex", path);
      setNotice(result.warning ? { kind: "warning", message: "账号已保存但暂不参与路由：模型同步失败。" } : { kind: "success", message: result.notice || `已从 ${label} 导入账号。` });
      await load();
    } catch (_) {
      setNotice({ kind: "error", message: "导入失败，请确认 auth.json 可读且字段完整。" });
    } finally { setPendingId(null); }
  };
  const confirmAction = async () => {
    if (!confirmation) return;
    const { account } = confirmation;
    setPendingId(account.id);
    try {
      await authApi.logout(account.id);
      setNotice({ kind: "success", message: "账号已删除。" });
      setConfirmation(null);
      await load();
    } catch (_) {
      setNotice({ kind: "error", message: "操作失败，请稍后重试。" });
    } finally {
      setPendingId(null);
    }
  };
  const exportAuth = async (account: AuthAccount) => {
    let path: string | null = null;
    try {
      const { save } = await import("@tauri-apps/plugin-dialog");
      path = await save({
        title: "导出 Codex auth JSON",
        filters: [{ name: "Codex auth JSON", extensions: ["json"] }],
        defaultPath: exportFileName(account),
      });
    } catch {
      setNotice({ kind: "error", message: "无法打开保存位置选择器，请稍后重试。" });
      return;
    }
    if (path === null) return;
    setPendingId(account.id);
    try {
      const result = await authApi.exportJson(account.id, path);
      const backup = result.backup_path ? `；已备份原文件：${result.backup_path}` : "";
      setNotice({ kind: "success", message: `已导出到 ${result.path}${backup}` });
      await load();
    } catch (_) {
      setNotice({ kind: "error", message: "导出失败，请稍后重试。" });
    } finally {
      setPendingId(null);
    }
  };

  return <div className="page-shell space-y-3"><div className="page-header sticky top-0 z-30 -mx-7 -mt-7 mb-2 flex-col bg-card/90 px-7 pt-3 backdrop-blur-md"><div className="flex w-full items-start justify-between gap-4 pb-1.5"><div><h1 className="page-title">渠道管理</h1><p className="page-subtitle mt-0.5">登录各厂商订阅账号，作为上游路由候选</p></div><div className="flex items-center gap-2"><button onClick={() => setShowLogin(true)} disabled={pendingId === "import"} className="action-primary"><KeyRound size={16} />登录账号</button><button onClick={importAuth} disabled={pendingId === "import"} className="action-secondary">{pendingId === "import" ? <Loader2 size={16} className="animate-spin" /> : <Upload size={16} />}从 auth.json 导入</button></div></div><ChannelTabs /></div>
    {notice && <div role="status" className={`flex items-center justify-between gap-3 rounded-2xl border px-4 py-3 text-sm ${notice.kind === "error" ? "border-destructive/25 bg-destructive/10 text-destructive" : notice.kind === "warning" ? "border-warning/25 bg-warning/10 text-warning" : "border-success/25 bg-success/10 text-success"}`}><span>{notice.message}</span><button onClick={() => setNotice(null)} aria-label="关闭提示"><X size={16} /></button></div>}
    <ProviderPills /><p className="text-sm text-muted-foreground">登录后作为路由候选并消耗订阅额度；开启 Auth 账号优先后，将优先使用。</p>
    <div className="flex gap-2 rounded-2xl border border-destructive/25 bg-destructive/10 px-4 py-3 text-xs leading-5 text-destructive"><CircleAlert className="mt-0.5 shrink-0" size={16} /><p>⚠️ 风险提示：此提供商使用的订阅 / OAuth 会话未获官方授权用于代理 / 路由器使用。账户可能被限制或封禁。使用风险自负。</p></div>
    {loading ? <div className="flex min-h-64 items-center justify-center gap-2 text-sm text-muted-foreground"><Loader2 size={18} className="animate-spin" />加载 Auth 账号…</div> : <div className="grid grid-cols-1 gap-5 xl:grid-cols-2">{accounts.map(account => <AccountCard key={account.id} account={account} pending={pendingId === account.id} onEdit={() => setEditAccount(account)} onToggle={() => void runFor(account.id, account.disabled ? "账号已启用。" : "账号已停用。", () => authApi.toggle(account.id, !account.disabled).then(() => undefined))} onDelete={() => setConfirmation({ kind: "delete", account })} onRefresh={() => void runFor(account.id, "令牌刷新完成。", () => authApi.refreshToken(account.id).then(() => undefined))} onSync={() => setSyncAccount(account)} onExport={() => void exportAuth(account)} onRelogin={() => setShowLogin(true)} />)}<EmptyAccountSlot onLogin={() => setShowLogin(true)} onImport={() => void importAuth()} busy={pendingId === "import"} /></div>}
    {showLogin && <LoginModal onClose={() => setShowLogin(false)} onCompleted={completeLogin} />}
    {editAccount && <EditModal account={editAccount} pending={pendingId === editAccount.id} onClose={() => setEditAccount(null)} onSave={async input => { await runFor(input.id, "账号配置已保存。", () => authApi.update(input).then(() => undefined)); setEditAccount(null); }} />}
    {syncAccount && <ModelSyncModal account={syncAccount} onClose={() => setSyncAccount(null)} onSynced={() => { void load(); setNotice({ kind: "success", message: "模型同步完成。" }); }} />}
    {confirmation && <ConfirmationDialog confirmation={confirmation} pending={pendingId === confirmation.account.id} onCancel={() => setConfirmation(null)} onConfirm={() => void confirmAction()} />}
  </div>;
}
