import { useEffect, useState, useCallback } from "react";

// ── types ────────────────────────────────────────────────────────────────
export interface MappingPair {
  from: string;
  to: string;
}

export type ModelMapping = Record<string, string | string[]>;

// ── serialize / deserialize ──────────────────────────────────────────────
export function mappingToPairs(mapping?: ModelMapping | null): MappingPair[] {
  if (!mapping) return [];
  return Object.entries(mapping).flatMap(([from, to]) => {
    const targets = Array.isArray(to) ? to : [to];
    return targets.map(t => ({ from, to: t }));
  });
}

export function pairsToMapping(pairs: MappingPair[]): ModelMapping {
  const obj: ModelMapping = {};
  pairs.forEach(m => {
    if (m.from.trim() && m.to.trim()) {
      const from = m.from.trim();
      const to = m.to.trim();
      if (obj[from] !== undefined) {
        const existing = obj[from];
        if (Array.isArray(existing)) {
          if (!existing.includes(to)) existing.push(to);
        } else {
          obj[from] = existing !== to ? [existing, to] : existing;
        }
      } else {
        obj[from] = to;
      }
    }
  });
  return obj;
}

// ── hook: useModelMappings ───────────────────────────────────────────────
export function useModelMappings(initial?: ModelMapping | null) {
  const [mappings, setMappings] = useState<MappingPair[]>(() => mappingToPairs(initial));

  // Keep mappings in sync if the initial value changes (e.g. when switching editing target)
  useEffect(() => {
    setMappings(mappingToPairs(initial));
  }, [initial]); // eslint-disable-line react-hooks/exhaustive-deps

  const addMapping = useCallback((defaultTo: string) => {
    setMappings(prev => [...prev, { from: "", to: defaultTo }]);
  }, []);

  const removeMapping = useCallback((idx: number) => {
    setMappings(prev => prev.filter((_, i) => i !== idx));
  }, []);

  const removeByTarget = useCallback((target: string) => {
    setMappings(prev => prev.filter(m => m.to !== target));
  }, []);

  const updateMapping = useCallback((idx: number, field: "from" | "to", value: string) => {
    setMappings(prev => prev.map((m, i) => (i === idx ? { ...m, [field]: value } : m)));
  }, []);

  const existingFroms = Array.from(new Set(mappings.map(m => m.from).filter(Boolean))).sort();

  return { mappings, addMapping, removeMapping, removeByTarget, updateMapping, existingFroms };
}

// ── hook: useGlobalFroms ─────────────────────────────────────────────────
// Aggregates mapping names from all channels + auth accounts for dropdown suggestions.
import { channelApi } from "../lib/api";
import { authApi } from "../lib/api";

export function useGlobalFroms() {
  const [globalFroms, setGlobalFroms] = useState<string[]>([]);

  useEffect(() => {
    const names = new Set<string>();

    Promise.all([
      channelApi.getAll().catch(() => []),
      authApi.accountsList().catch(() => []),
    ]).then(([channels, accounts]) => {
      for (const ch of channels as { model_mapping?: Record<string, unknown> }[]) {
        if (ch.model_mapping && typeof ch.model_mapping === "object") {
          for (const key of Object.keys(ch.model_mapping)) if (key) names.add(key);
        }
      }
      for (const acc of accounts as { model_mapping?: Record<string, unknown> }[]) {
        if (acc.model_mapping && typeof acc.model_mapping === "object") {
          for (const key of Object.keys(acc.model_mapping)) if (key) names.add(key);
        }
      }
      setGlobalFroms(Array.from(names).sort());
    }).catch(() => {});
  }, []); // eslint-disable-line react-hooks/exhaustive-deps

  return globalFroms;
}
