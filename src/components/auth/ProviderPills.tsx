import { Command } from "lucide-react";

export function ProviderPills() {
  return (
    <div className="flex flex-wrap items-center gap-2" role="group" aria-label="Auth 提供商">
      <button
        type="button"
        className="inline-flex items-center gap-1.5 rounded-full bg-success px-3 py-1.5 text-xs font-semibold text-white shadow-sm"
        aria-pressed="true"
      >
        <Command size={13} /> Codex
        <span className="h-1.5 w-1.5 rounded-full bg-white/90" aria-hidden="true" />
      </button>
      <span className="mx-1 h-5 w-px bg-border" aria-hidden="true" />
      {[
        ["Claude", "claude"],
        ["Kiro", "kiro"],
        ["Kimi", "kimi"],
      ].map(([name, key]) => (
        <span key={key} className="inline-flex items-center gap-1.5 rounded-full border border-border bg-muted px-3 py-1.5 text-xs text-muted-foreground">
          {name}
          <span className="rounded-full bg-card px-1.5 py-0.5 text-[10px]">规划中</span>
        </span>
      ))}
    </div>
  );
}
