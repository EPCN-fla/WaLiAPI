import { Clock3 } from "lucide-react";
import type { AuthQuotaLimit, AuthQuotaWindow } from "../../types";

function resetLabel(resetAt: string | null) {
  if (!resetAt) return "恢复时间待上游返回";
  const milliseconds = new Date(resetAt).getTime() - Date.now();
  if (!Number.isFinite(milliseconds) || milliseconds <= 0) return "即将恢复";
  const minutes = Math.ceil(milliseconds / 60_000);
  if (minutes < 60) return `${minutes} 分钟后`;
  if (minutes < 48 * 60) return `${Math.ceil(minutes / 60)} 小时后`;
  return `${Math.ceil(minutes / 1_440)} 天后`;
}

function windowLabel(window: AuthQuotaWindow, fallback: string) {
  if (!window.window_minutes) return fallback;
  if (window.window_minutes >= 10_080) return "周窗口";
  if (window.window_minutes >= 60) return `${Math.round(window.window_minutes / 60)}小时窗口`;
  return `${window.window_minutes}分钟窗口`;
}

function QuotaWindow({ limit, window, fallback }: { limit: AuthQuotaLimit; window: AuthQuotaWindow; fallback: string }) {
  const used = Math.max(0, Math.min(100, window.used_percent ?? 0));
  const exhausted = used >= 100;
  const barColor = exhausted ? "bg-destructive" : used >= 70 ? "bg-warning" : "bg-success";
  return (
    <div className="rounded-xl border border-border bg-muted/45 p-3">
      <div className="flex items-center justify-between gap-3 text-xs">
        <span className="flex items-center gap-1.5 font-medium"><Clock3 size={13} className="text-muted-foreground" />限额 · {limit.limit_name || windowLabel(window, fallback)}</span>
        <span className={exhausted ? "font-semibold text-destructive" : "font-semibold text-muted-foreground"}>{used.toFixed(0)}% 已用</span>
      </div>
      <div className="mt-2 h-1.5 overflow-hidden rounded-full bg-card">
        <div className={`h-full rounded-full ${barColor}`} style={{ width: `${used}%` }} />
      </div>
      <p className="mt-2 text-[11px] text-muted-foreground">重置 {resetLabel(window.reset_at)} · {limit.limit_id}</p>
    </div>
  );
}

export function QuotaBlock({ quota }: { quota: NonNullable<import("../../types").AuthAccount["quota"]> }) {
  const windows = quota.limits.flatMap(limit => [
    limit.primary && { limit, window: limit.primary, fallback: "主窗口" },
    limit.secondary && { limit, window: limit.secondary, fallback: "次窗口" },
  ].filter(Boolean) as { limit: AuthQuotaLimit; window: AuthQuotaWindow; fallback: string }[]);

  if (quota.exceeded && windows.length === 0) {
    return <div className="rounded-xl border border-destructive/25 bg-destructive/10 px-3 py-2.5 text-xs font-medium text-destructive">已踢出路由 · {quota.reason || "订阅限额已耗尽"}</div>;
  }

  return <div className="space-y-2">{windows.map(({ limit, window, fallback }, index) => <QuotaWindow key={`${limit.limit_id}-${index}`} limit={limit} window={window} fallback={fallback} />)}</div>;
}
