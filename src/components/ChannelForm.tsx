import { useState, useMemo, useEffect, useRef } from "react";
import { channelApi } from "../lib/api";
import type {
  Channel, CreateChannelInput, UpdateChannelInput,
  ChannelProtocol, ChannelProvider, ChannelEndpoint, ChannelAuthScheme,
  ChannelPreset, ChannelProtocolPresetGroup,
  DraftChannelTestResult, DraftChannelTestInput,
} from "../types";
import {
  CHANNEL_CATEGORIES, CHANNEL_PROVIDER_ICONS, PROTOCOL_LABELS, ENDPOINT_LABELS,
} from "../lib/constants";
import { X, Plus, Check, RefreshCw, KeyRound, Undo, Loader2 } from "lucide-react";
import { MappingRow } from "./channel-form/MappingRow";
import { DraftTestModal } from "./channel-form/DraftTestModal";
import { ConfirmSwitchDialog } from "./channel-form/ConfirmSwitchDialog";

// ─── 协议级结构（UI 结构常量，非厂商模板副本）────────────────────────────────
// 这些描述的是「协议本身的语义」（设计 3.2）：OpenAI 有两个可选端点，
// Anthropic 固定 Messages，Ollama 固定 /api/chat。厂商 URL/模型模板唯一来源
// 是后端 registry（get_channel_presets）。
const PROTOCOLS: ChannelProtocol[] = ["openai", "anthropic", "ollama"];

const PROTOCOL_ENDPOINT_OPTIONS: Record<ChannelProtocol, ChannelEndpoint[]> = {
  openai: ["chat_completions", "responses"],
  anthropic: ["messages"],
  ollama: ["api_chat"],
};

const PROTOCOL_BASE_URL_HINTS: Record<ChannelProtocol, string> = {
  openai: "不包含端点路径；通常以 /v1 或兼容服务根路径结束",
  anthropic: "不包含 /v1/messages；不要以斜杠结尾",
  ollama: "本机或远程 Ollama 的主机与端口（例如 http://localhost:11434）",
};

const PROTOCOL_DEFAULT_AUTH: Record<ChannelProtocol, ChannelAuthScheme> = {
  openai: "bearer",
  anthropic: "x_api_key",
  ollama: "optional_bearer",
};

const isProtocol = (v: unknown): v is ChannelProtocol =>
  v === "openai" || v === "anthropic" || v === "ollama";

/** 全部已知端点（含能力端点 count_tokens/embeddings），用于编辑回填保真（F2）。 */
const ALL_ENDPOINTS: ChannelEndpoint[] = [
  "chat_completions", "responses", "messages", "count_tokens", "embeddings", "api_chat",
];

const isEndpoint = (v: unknown): v is ChannelEndpoint =>
  typeof v === "string" && (ALL_ENDPOINTS as string[]).includes(v);

/** 协议 custom option 的默认勾选端点（与后端 custom_preset 一致）。 */
function defaultEndpointsFor(protocol: ChannelProtocol): ChannelEndpoint[] {
  switch (protocol) {
    case "openai": return ["chat_completions"];
    case "anthropic": return ["messages"];
    case "ollama": return ["api_chat"];
  }
}

/** 应用预设时写入 form 的端点集合（F1）：
 *  Anthropic 的能力端点是固定的（固定 Messages + 模板声明的 count_tokens），
 *  必须持久化全量 native_endpoints 供路由命中；OpenAI/Ollama 端点可勾选，
 *  以 default_checked（决定 UI 勾选态）为准。 */
function endpointsForPreset(preset: ChannelPreset): ChannelEndpoint[] {
  if (preset.protocol === "anthropic") return [...preset.native_endpoints];
  return [...preset.default_checked_endpoints];
}

/** 自定义预设（legacy_base_url 为空）时，从 native 根推导旧代码兼容根（F6）。
 *  旧适配器在 base_url 后追加 /chat/completions（openai）或 /messages（claude），
 *  因此 anthropic/ollama 需要 /v1 根；openai 保留用户输入的根（通常已含 /v1）。 */
function deriveLegacyBaseUrl(protocol: ChannelProtocol, native: string): string {
  const root = native.trim().replace(/\/+$/, "");
  if (!root) return "";
  if (protocol === "openai") return root;
  return root.endsWith("/v1") ? root : `${root}/v1`;
}

function sameEndpoints(a: string[], b: string[]): boolean {
  if (a.length !== b.length) return false;
  return [...a].sort().join(",") === [...b].sort().join(",");
}

interface FormState {
  name: string;
  protocol: ChannelProtocol;
  provider: ChannelProvider;
  native_base_url: string;
  api_key: string;
  models: string[];
  native_endpoints: ChannelEndpoint[];
  model_mapping: Record<string, string | string[]>;
  priority: number;
  weight: number;
  timeout_secs: number;
  preset_revision: string | null;
  legacy_executor_override?: string;
}

type PendingSwitch =
  | { kind: "protocol"; protocol: ChannelProtocol }
  | { kind: "provider"; provider: ChannelProvider };

function initForm(editing: Channel | null): FormState {
  if (editing) {
    const protocol = isProtocol(editing.protocol) ? editing.protocol : "openai";
    const endpoints = (editing.native_endpoints ?? []).filter(isEndpoint);
    return {
      name: editing.name,
      protocol,
      provider: (editing.provider as ChannelProvider) || "custom",
      native_base_url: editing.native_base_url || editing.base_url,
      api_key: "",
      models: editing.models ?? [],
      native_endpoints: endpoints.length > 0 ? endpoints : defaultEndpointsFor(protocol),
      model_mapping: editing.model_mapping ?? {},
      priority: editing.priority ?? 0,
      weight: editing.weight ?? 1,
      timeout_secs: editing.timeout_secs ?? 60,
      preset_revision: editing.preset_revision ?? null,
      legacy_executor_override: editing.legacy_executor_override ?? undefined,
    };
  }
  return {
    name: "",
    protocol: "openai",
    provider: "custom",
    native_base_url: "",
    api_key: "",
    models: [],
    native_endpoints: defaultEndpointsFor("openai"),
    model_mapping: {},
    priority: 0,
    weight: 1,
    timeout_secs: 60,
    preset_revision: null,
  };
}

function initMappings(editing: Channel | null): { from: string; to: string }[] {
  const raw = editing?.model_mapping || {};
  const result: { from: string; to: string }[] = [];
  for (const [from, val] of Object.entries(raw)) {
    if (Array.isArray(val)) {
      for (const to of val) result.push({ from, to });
    } else {
      result.push({ from, to: val });
    }
  }
  return result;
}

export function ChannelForm({ editing, onClose, onSaved }: {
  editing: Channel | null;
  onClose: () => void;
  onSaved: () => void;
}) {
  const [form, setForm] = useState<FormState>(() => initForm(editing));
  const [modelInput, setModelInput] = useState("");
  const [mappings, setMappings] = useState<{ from: string; to: string }[]>(() => initMappings(editing));

  // Global mapping names from all channels (for from dropdown suggestions)
  const [globalFroms, setGlobalFroms] = useState<string[]>([]);
  useEffect(() => {
    channelApi.getAll().then(channels => {
      const names = new Set<string>();
      for (const ch of channels) {
        const mm = ch.model_mapping;
        if (mm && typeof mm === "object") {
          for (const key of Object.keys(mm)) if (key) names.add(key);
        }
      }
      setGlobalFroms(Array.from(names).sort());
    }).catch(() => {});
  }, []);

  // ── presets（T01）────────────────────────────────────────────────────────
  const [presetGroups, setPresetGroups] = useState<ChannelProtocolPresetGroup[]>([]);
  const [presetsLoading, setPresetsLoading] = useState(true);
  const [presetsError, setPresetsError] = useState<string | null>(null);
  useEffect(() => {
    let alive = true;
    channelApi.getPresets()
      .then(groups => { if (alive) { setPresetGroups(groups); setPresetsLoading(false); } })
      .catch(e => { if (alive) { setPresetsError(String(e)); setPresetsLoading(false); } });
    return () => { alive = false; };
  }, []);

  // ── 连接参数 / 测试 receipt 状态 ─────────────────────────────────────────
  const [connEdited, setConnEdited] = useState(false);
  // 挂载时的连接字段初始值：连接参数恢复原状时清除 dirty 标记（Q6）。
  const [initialConn] = useState(() => {
    const f = initForm(editing);
    return { url: f.native_base_url, models: f.models, endpoints: f.native_endpoints };
  });
  // 编辑态下已保存的渠道名视为「用户已命名」，切换预设不自动改名。
  const [nameTouched, setNameTouched] = useState(!!editing);
  const autoNameRef = useRef<string | null>(null);
  const [pendingSwitch, setPendingSwitch] = useState<PendingSwitch | null>(null);
  const [receipt, setReceipt] = useState<DraftChannelTestResult | null>(null);
  const [testPhase, setTestPhase] = useState<"idle" | "running" | "failed">("idle");
  const [testResult, setTestResult] = useState<DraftChannelTestResult | null>(null);
  const [saving, setSaving] = useState(false);
  const [clearKeyRequested, setClearKeyRequested] = useState(false);
  const [localError, setLocalError] = useState<string | null>(null);
  const [saveError, setSaveError] = useState<string | null>(null);

  const currentPreset = useMemo(() => {
    const group = presetGroups.find(g => g.protocol === form.protocol);
    return group?.presets.find(p => p.provider === form.provider) ?? null;
  }, [presetGroups, form.protocol, form.provider]);

  const customPreset = useMemo(() => {
    const group = presetGroups.find(g => g.protocol === form.protocol);
    return group?.presets.find(p => p.provider === "custom") ?? null;
  }, [presetGroups, form.protocol]);

  const authScheme: ChannelAuthScheme = currentPreset?.auth_scheme ?? PROTOCOL_DEFAULT_AUTH[form.protocol];
  const keyRequired = authScheme !== "optional_bearer";

  // Sync mappings back to form.model_mapping whenever they change
  useEffect(() => {
    const obj: Record<string, string | string[]> = {};
    mappings.forEach(m => {
      if (m.from && m.to) {
        if (obj[m.from] !== undefined) {
          const existing = obj[m.from];
          if (Array.isArray(existing)) existing.push(m.to);
          else obj[m.from] = [existing, m.to];
        } else {
          obj[m.from] = m.to;
        }
      }
    });
    setForm(prev => ({ ...prev, model_mapping: obj }));
  }, [mappings]);

  // ── receipt 失效规则（T07）：protocol/provider/URL/Key/模型/端点/timeout 变更即失效；
  //    name/priority/weight/映射 变更不失效。 ───────────────────────────────
  function invalidateReceipt() {
    setReceipt(null);
    setTestPhase("idle");
    setTestResult(null);
    setSaveError(null);
  }

  /** 连接参数是否已恢复到挂载时初始值；是则清除 dirty（Q6），否则保持 dirty。 */
  function syncConnDirty(nextUrl: string, nextModels: string[], nextEndpoints: ChannelEndpoint[]) {
    const restored =
      nextUrl === initialConn.url &&
      nextModels.length === initialConn.models.length &&
      nextModels.every((m, i) => m === initialConn.models[i]) &&
      sameEndpoints(nextEndpoints, initialConn.endpoints);
    setConnEdited(!restored);
  }

  const hasConnectionValues = connEdited || form.native_base_url.trim() !== "" || form.models.length > 0;

  function findPreset(protocol: ChannelProtocol, provider: ChannelProvider): ChannelPreset | null {
    const group = presetGroups.find(g => g.protocol === protocol);
    return group?.presets.find(p => p.provider === provider) ?? null;
  }

  function applyPreset(preset: ChannelPreset, apply: boolean) {
    setForm(prev => {
      let name = prev.name;
      if (apply && preset.provider !== "custom" && !nameTouched) {
        // 仅当名称为空或仍等于上次自动名称时，更新为厂商展示名
        if (!name || name === autoNameRef.current) {
          name = preset.display_name;
          autoNameRef.current = preset.display_name;
        }
      }
      return {
        ...prev,
        name,
        protocol: preset.protocol,
        provider: preset.provider,
        preset_revision: preset.preset_revision,
        ...(apply ? {
          native_base_url: preset.native_base_url,
          native_endpoints: endpointsForPreset(preset),
          models: preset.model_suggestions.map(m => m.id),
        } : {}),
      };
    });
    setConnEdited(false);
    invalidateReceipt();
  }

  function applyProtocolDefaults(protocol: ChannelProtocol) {
    setForm(prev => ({
      ...prev,
      protocol,
      provider: "custom",
      preset_revision: null,
      native_base_url: "",
      native_endpoints: defaultEndpointsFor(protocol),
      models: [],
    }));
    setConnEdited(false);
    invalidateReceipt();
  }

  function requestProtocolSwitch(protocol: ChannelProtocol) {
    if (protocol === form.protocol || saving) return;
    if (hasConnectionValues) {
      setPendingSwitch({ kind: "protocol", protocol });
    } else {
      const custom = findPreset(protocol, "custom");
      if (custom) applyPreset(custom, true);
      else applyProtocolDefaults(protocol);
    }
  }

  function requestProviderSwitch(provider: ChannelProvider) {
    if (provider === form.provider || saving) return;
    const target = findPreset(form.protocol, provider);
    if (hasConnectionValues) {
      setPendingSwitch({ kind: "provider", provider });
    } else {
      if (target) applyPreset(target, true);
    }
  }

  function onConfirmApply() {
    if (!pendingSwitch) return;
    if (pendingSwitch.kind === "protocol") {
      const custom = findPreset(pendingSwitch.protocol, "custom");
      if (custom) applyPreset(custom, true);
      else applyProtocolDefaults(pendingSwitch.protocol);
    } else {
      const target = findPreset(form.protocol, pendingSwitch.provider);
      if (target) applyPreset(target, true);
    }
    setPendingSwitch(null);
  }

  function onConfirmKeep() {
    if (!pendingSwitch) return;
    if (pendingSwitch.kind === "protocol") {
      const newProtocol = pendingSwitch.protocol;
      const providerExists = presetGroups
        .find(g => g.protocol === newProtocol)
        ?.presets.some(p => p.provider === form.provider) ?? false;
      const targetProvider: ChannelProvider = providerExists ? form.provider : "custom";
      // 「保留当前连接参数」：URL/模型/Key/映射/名称/priority/weight/timeout 保留；
      // 端点按新协议结构重算（Anthropic 固定 Messages、Ollama /api/chat），
      // 避免把 OpenAI 端点带入 Anthropic 造成无法修复的非法配置。
      const targetDefaultEps = findPreset(newProtocol, targetProvider)?.default_checked_endpoints
        ?? defaultEndpointsFor(newProtocol);
      setForm(prev => ({
        ...prev,
        protocol: newProtocol,
        provider: targetProvider,
        native_endpoints: [...targetDefaultEps],
        preset_revision: findPreset(newProtocol, targetProvider)?.preset_revision ?? null,
      }));
    } else {
      const target = findPreset(form.protocol, pendingSwitch.provider);
      setForm(prev => ({
        ...prev,
        provider: pendingSwitch.provider,
        preset_revision: target?.preset_revision ?? null,
      }));
    }
    setReceipt(null); // protocol/provider 已变 → receipt 失效
    setTestPhase("idle");
    setTestResult(null);
    setPendingSwitch(null);
  }

  // ── 连接字段变更 ─────────────────────────────────────────────────────────
  function onUrlChange(v: string) {
    setForm(prev => ({ ...prev, native_base_url: v }));
    syncConnDirty(v, form.models, form.native_endpoints);
    invalidateReceipt();
  }
  function onKeyChange(v: string) {
    setForm(prev => ({ ...prev, api_key: v }));
    if (v.trim() !== "") setClearKeyRequested(false);
    invalidateReceipt();
  }
  function onTimeoutChange(v: number) {
    setForm(prev => ({ ...prev, timeout_secs: v }));
    invalidateReceipt();
  }
  function onModelListChange(nextModels: string[]) {
    setForm(prev => ({ ...prev, models: nextModels }));
    syncConnDirty(form.native_base_url, nextModels, form.native_endpoints);
    invalidateReceipt();
  }
  function toggleEndpoint(ep: ChannelEndpoint, checked: boolean) {
    const has = form.native_endpoints.includes(ep);
    const next = checked
      ? (has ? form.native_endpoints : [...form.native_endpoints, ep])
      : form.native_endpoints.filter(e => e !== ep);
    // OpenAI：至少保留一个端点
    if (!checked && form.protocol === "openai" && next.length === 0) return;
    setForm(prev => ({ ...prev, native_endpoints: next }));
    syncConnDirty(form.native_base_url, form.models, next);
    invalidateReceipt();
  }
  function requestClearKey() {
    setForm(prev => ({ ...prev, api_key: "" }));
    setClearKeyRequested(true);
    invalidateReceipt();
  }
  function undoClearKey() {
    setClearKeyRequested(false);
  }

  const isLastEndpoint = (ep: ChannelEndpoint) =>
    form.protocol === "openai" && form.native_endpoints.length === 1 && form.native_endpoints[0] === ep;

  // ── 模型列表 ────────────────────────────────────────────────────────────
  function addModel() {
    const m = modelInput.trim();
    if (!m) return;
    if (!form.models.includes(m)) onModelListChange([...form.models, m]);
    setModelInput("");
  }
  function removeModel(m: string) {
    onModelListChange(form.models.filter(x => x !== m));
    setMappings(prev => prev.filter(map => map.from !== m));
  }

  // ── 模型映射 ────────────────────────────────────────────────────────────
  function addMapping() {
    if (form.models.length > 0) setMappings(prev => [...prev, { from: "", to: form.models[0] }]);
  }
  function updateMapping(idx: number, field: "from" | "to", value: string) {
    setMappings(prev => prev.map((m, i) => i === idx ? { ...m, [field]: value } : m));
  }
  function removeMapping(idx: number) {
    setMappings(prev => prev.filter((_, i) => i !== idx));
  }

  // ── legacy type/base_url 兼容字段 ────────────────────────────────────────
  function legacyType(): string {
    // 旧 Gemini 原生配置保留 type=gemini（后端 new_to_legacy 同规则）。
    if (form.legacy_executor_override === "gemini_native") return "gemini";
    return currentPreset?.legacy_type ?? (form.protocol === "anthropic" ? "claude" : "openai");
  }
  function legacyBaseUrl(): string {
    // 旧 Gemini 原生配置：保持原始 native 根（后端 new_to_legacy 同规则）。
    if (form.legacy_executor_override === "gemini_native") return form.native_base_url || "";
    if (currentPreset?.legacy_base_url) return currentPreset.legacy_base_url;
    // 自定义预设 legacy_base_url 为空：按后端 T02 推导约定生成旧代码兼容根（F6）。
    return deriveLegacyBaseUrl(form.protocol, form.native_base_url);
  }

  function buildDraftInput(): DraftChannelTestInput {
    return {
      id: editing?.id,
      name: form.name,
      type: legacyType(),
      base_url: legacyBaseUrl(),
      api_key: form.api_key,
      // 让草稿测试在后端与保存路径解析出相同的有效 Key：
      // 编辑留空未清除 → 沿用已存 Key；显式清除 → 空 Key。
      clear_api_key: clearKeyRequested || undefined,
      models: form.models,
      priority: form.priority,
      weight: form.weight,
      model_mapping: form.model_mapping,
      timeout_secs: form.timeout_secs,
      protocol: form.protocol,
      provider: form.provider,
      native_base_url: form.native_base_url,
      native_endpoints: form.native_endpoints,
      preset_revision: form.preset_revision || undefined,
      legacy_executor_override: form.legacy_executor_override,
    };
  }

  type ReceiptFields = { test_run_id: string; draft_fingerprint: string; force_save: boolean };

  function receiptFields(result: DraftChannelTestResult | null, forceSave: boolean): ReceiptFields | null {
    if (!result) return null;
    return {
      test_run_id: result.test_run_id,
      draft_fingerprint: result.draft_fingerprint,
      force_save: forceSave,
    };
  }

  function buildCreateInput(rf: ReceiptFields | null): CreateChannelInput {
    return {
      name: form.name,
      type: legacyType(),
      base_url: legacyBaseUrl(),
      api_key: form.api_key,
      models: form.models,
      priority: form.priority,
      weight: form.weight,
      model_mapping: form.model_mapping,
      timeout_secs: form.timeout_secs,
      protocol: form.protocol,
      provider: form.provider,
      native_base_url: form.native_base_url,
      native_endpoints: form.native_endpoints,
      preset_revision: form.preset_revision || undefined,
      legacy_executor_override: form.legacy_executor_override,
      ...(rf ?? {}),
    };
  }

  function buildUpdateInput(rf: ReceiptFields | null): UpdateChannelInput {
    return {
      id: editing!.id,
      name: form.name,
      models: form.models,
      priority: form.priority,
      weight: form.weight,
      model_mapping: form.model_mapping,
      timeout_secs: form.timeout_secs,
      // F3：始终写回解析后的身份（type/base_url/protocol/provider/native_*）。
      // 对 legacy（identity_revision 0）渠道，保存即迁移；对已迁移渠道为幂等写。
      // 配合 F2（isEndpoint 保留 count_tokens/embeddings），编辑保存不再剥离能力端点。
      type: legacyType(),
      base_url: legacyBaseUrl(),
      protocol: form.protocol,
      provider: form.provider,
      native_base_url: form.native_base_url,
      native_endpoints: form.native_endpoints,
      preset_revision: form.preset_revision || undefined,
      legacy_executor_override: form.legacy_executor_override,
      // 编辑留空 = 不修改；显式清除走 clear_api_key 标记。
      ...(form.api_key.trim() !== "" ? { api_key: form.api_key } : {}),
      ...(clearKeyRequested ? { clear_api_key: true } : {}),
      ...(rf ?? {}),
    };
  }

  // ── 保存流程：本地校验 → 草稿测试 → 全过自动保存 / 失败弹窗强制保存 ───────
  function validate(): string | null {
    if (!form.name.trim()) return "名称不能为空";
    if (!/^https?:\/\//i.test(form.native_base_url.trim())) {
      return "Base URL 必须是 http(s) 地址";
    }
    if (form.protocol === "openai" && form.native_endpoints.length === 0) {
      return "OpenAI 协议至少勾选一个端点（Chat Completions 或 Responses）";
    }
    if (form.protocol === "anthropic" && !form.native_endpoints.includes("messages")) {
      return "Anthropic 协议必须包含 /v1/messages 端点";
    }
    if (form.protocol === "ollama" && !form.native_endpoints.includes("api_chat")) {
      return "Ollama 协议必须包含 /api/chat 端点";
    }
    if (!editing && keyRequired && !form.api_key.trim()) {
      return "API Key 不能为空";
    }
    return null;
  }

  async function doSave(result: DraftChannelTestResult | null, forceSave: boolean) {
    if (saving) return;
    setSaving(true);
    setSaveError(null);
    try {
      const rf = receiptFields(result, forceSave);
      if (editing) await channelApi.update(buildUpdateInput(rf));
      else await channelApi.create(buildCreateInput(rf));
      onSaved();
    } catch (e) {
      const msg = `保存失败：${String(e)}`;
      setSaveError(msg);
      setLocalError(msg);
      setSaving(false);
      // 自动保存（全过）失败：回到 idle 关闭测试弹窗，展示表单错误。
      // 强制保存失败：保持 failed 弹窗，展示 saveError 供重试。
      setTestPhase(prev => (prev === "running" ? "idle" : prev));
    }
  }

  async function handleSubmit(e: React.FormEvent) {
    e.preventDefault();
    if (saving || testPhase === "running") return;
    const err = validate();
    if (err) { setLocalError(err); return; }
    setLocalError(null);
    setSaveError(null);
    setTestPhase("running");
    setTestResult(null);
    try {
      const result = await channelApi.testDraft(buildDraftInput());
      setReceipt(result);
      const allPassed = result.results.length > 0 && result.results.every(r => r.status === "passed");
      if (allPassed) {
        await doSave(result, false);
      } else {
        setTestResult(result);
        setTestPhase("failed");
      }
    } catch (e) {
      setTestPhase("failed");
      setTestResult(null);
      setLocalError(`连通性测试失败：${String(e)}`);
    }
  }

  async function handleForceSave() {
    await doSave(receipt ?? testResult, true);
  }

  // ── 渲染用派生数据 ───────────────────────────────────────────────────────
  const groupedPresets = useMemo(() => {
    const group = presetGroups.find(g => g.protocol === form.protocol);
    const vendors = group?.presets.filter(p => p.provider !== "custom") ?? [];
    const order = ["international", "domestic", "local"] as const;
    return order
      .map(region => ({ region, presets: vendors.filter(p => p.region === region) }))
      .filter(g => g.presets.length > 0);
  }, [presetGroups, form.protocol]);

  function onTabKeyDown(e: React.KeyboardEvent, idx: number) {
    if (e.key === "ArrowRight") { e.preventDefault(); requestProtocolSwitch(PROTOCOLS[(idx + 1) % PROTOCOLS.length]); }
    else if (e.key === "ArrowLeft") { e.preventDefault(); requestProtocolSwitch(PROTOCOLS[(idx - 1 + PROTOCOLS.length) % PROTOCOLS.length]); }
    else if (e.key === "Home") { e.preventDefault(); requestProtocolSwitch(PROTOCOLS[0]); }
    else if (e.key === "End") { e.preventDefault(); requestProtocolSwitch(PROTOCOLS[PROTOCOLS.length - 1]); }
  }

  const editingLegacy = editing !== null && editing.identity_revision === 0;

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/60 p-4 backdrop-blur-sm" onClick={onClose}>
      <div className="surface w-full max-w-2xl max-h-[92vh] overflow-auto rounded-[28px]" onClick={e => e.stopPropagation()}>
        <div className="flex items-center justify-between border-b border-border px-5 py-4 sticky top-0 bg-inherit z-20">
          <h2 className="text-lg font-semibold">{editing ? "编辑渠道" : "新建渠道"}</h2>
          <button onClick={onClose} disabled={saving} className="action-secondary px-3 py-2"><X size={18} /></button>
        </div>

        <form
          onSubmit={handleSubmit}
          className="space-y-5 p-5"
          onKeyDown={e => { if (e.key === "Enter" && (e.nativeEvent.isComposing || e.keyCode === 229)) e.preventDefault(); }}
        >
          {/* 协议 Tab */}
          <div>
            <label className="mb-2 block text-sm font-medium">协议</label>
            <div role="tablist" aria-label="协议" className="grid grid-cols-3 gap-2 rounded-2xl bg-muted p-1.5">
              {PROTOCOLS.map((p, idx) => {
                const active = form.protocol === p;
                return (
                  <button
                    key={p}
                    type="button"
                    role="tab"
                    id={`protocol-tab-${p}`}
                    aria-selected={active}
                    aria-controls={`protocol-panel-${p}`}
                    tabIndex={active ? 0 : -1}
                    onClick={() => requestProtocolSwitch(p)}
                    onKeyDown={e => onTabKeyDown(e, idx)}
                    className={`rounded-xl px-4 py-2.5 text-sm font-semibold transition-all ${
                      active
                        ? "bg-white text-primary shadow-sm"
                        : "text-muted-foreground hover:text-foreground"
                    }`}
                  >
                    {PROTOCOL_LABELS[p]}
                  </button>
                );
              })}
            </div>
          </div>

          {editingLegacy && (
            <div className="rounded-xl border border-amber-200 bg-amber-50 px-3 py-2 text-xs text-amber-700">
              来自旧配置：该渠道身份由旧 type/base_url 推导；保存后才写入新的 protocol/provider 字段。
            </div>
          )}

          {/* 名称 */}
          <div>
            <label className="mb-2 block text-sm font-medium">名称</label>
            <input
              value={form.name}
              onChange={e => { setNameTouched(true); autoNameRef.current = null; setForm(prev => ({ ...prev, name: e.target.value })); }}
              className="w-full rounded-2xl border border-border bg-background/70 px-4 py-3 text-sm"
              placeholder="渠道名称"
              required
            />
          </div>

          {/* 渠道提供商选择器 */}
          <div>
            <label className="mb-2 block text-sm font-medium">渠道提供商</label>
            {presetsLoading ? (
              <div className="flex items-center gap-2 rounded-2xl border border-dashed border-border bg-background/40 px-4 py-5 text-sm text-muted-foreground">
                <Loader2 size={15} className="animate-spin" /> 正在加载提供商模板…
              </div>
            ) : presetsError ? (
              <div className="rounded-2xl border border-red-200 bg-red-50 px-4 py-3 text-sm text-red-700">
                提供商模板加载失败（{presetsError}）。已禁用厂商预设，可继续使用自定义配置手动填写；恢复后刷新重试。
              </div>
            ) : (
              <div className="space-y-3">
                {/* 顶部固定「自定义配置」整行卡片（registry 的 custom 预设，恒存在） */}
                {customPreset && (
                  <PresetCard
                    preset={customPreset}
                    selected={form.provider === "custom"}
                    onSelect={() => requestProviderSwitch("custom")}
                  />
                )}
                {groupedPresets.map(group => (
                  <div key={group.region}>
                    <div className="mb-1.5 flex items-center gap-1.5 px-1 text-xs font-semibold text-muted-foreground">
                      <span>{CHANNEL_CATEGORIES[group.region]?.icon}</span>
                      <span>{CHANNEL_CATEGORIES[group.region]?.label}</span>
                    </div>
                    <div className="grid grid-cols-1 gap-2 sm:grid-cols-2">
                      {group.presets.map(p => (
                        <PresetCard
                          key={p.id}
                          preset={p}
                          selected={form.provider === p.provider}
                          onSelect={() => requestProviderSwitch(p.provider)}
                        />
                      ))}
                    </div>
                  </div>
                ))}
              </div>
            )}
          </div>

          {/* 协议配置区 */}
          <div id={`protocol-panel-${form.protocol}`} role="tabpanel" aria-labelledby={`protocol-tab-${form.protocol}`}>
            {/* Base URL */}
            <div>
              <label className="mb-2 block text-sm font-medium">Base URL</label>
              <input
                value={form.native_base_url}
                onChange={e => onUrlChange(e.target.value)}
                className="w-full rounded-2xl border border-border bg-background/70 px-4 py-3 text-sm font-mono"
                placeholder={form.protocol === "ollama" ? "http://localhost:11434" : "https://api.example.com"}
                required
              />
              <p className="mt-1.5 text-xs text-muted-foreground">{PROTOCOL_BASE_URL_HINTS[form.protocol]}</p>
            </div>

            {/* 端点 */}
            <div className="mt-4">
              <label className="mb-2 block text-sm font-medium">端点</label>
              {form.protocol === "openai" && (
                <div className="flex flex-wrap gap-3">
                  {PROTOCOL_ENDPOINT_OPTIONS.openai.map(ep => (
                    <label key={ep} className={`flex items-center gap-2 rounded-2xl border px-4 py-3 text-sm transition-all ${form.native_endpoints.includes(ep) ? "border-primary/40 bg-primary/8 font-medium text-primary" : "border-border bg-background/40 hover:border-primary/30"}`}>
                      <input
                        type="checkbox"
                        checked={form.native_endpoints.includes(ep)}
                        disabled={isLastEndpoint(ep)}
                        onChange={e => toggleEndpoint(ep, e.target.checked)}
                        className="h-4 w-4 accent-[#2f6fed]"
                      />
                      <span>
                        <span className="block font-medium">{ENDPOINT_LABELS[ep]}</span>
                        <span className="block text-xs text-muted-foreground">{ep === "chat_completions" ? "/chat/completions" : "/responses"}</span>
                      </span>
                    </label>
                  ))}
                </div>
              )}
              {form.protocol === "anthropic" && (
                <div className="space-y-2">
                  <FixedEndpoint label="Messages" path="/v1/messages" note="固定" />
                  {form.native_endpoints.includes("count_tokens") && (
                    <FixedEndpoint label="Count Tokens" path="/v1/messages/count_tokens" note="模板声明能力" />
                  )}
                </div>
              )}
              {form.protocol === "ollama" && (
                <FixedEndpoint label="Chat" path="/api/chat" note="固定" />
              )}
            </div>

            {/* API Key */}
            <div className="mt-4">
              <label className="mb-2 block text-sm font-medium">API Key</label>
              <div className="flex gap-2">
                <input
                  type="password"
                  value={form.api_key}
                  onChange={e => onKeyChange(e.target.value)}
                  className="flex-1 rounded-2xl border border-border bg-background/70 px-4 py-3 text-sm font-mono"
                  placeholder={editing ? (clearKeyRequested ? "将清除已保存的 Key" : "留空则不修改") : keyRequired ? "sk-..." : "可留空（本地/自管 Ollama）"}
                />
                {editing && !clearKeyRequested && (
                  <button type="button" onClick={requestClearKey} title="清除已保存的 API Key" className="action-secondary shrink-0 px-3">
                    <KeyRound size={15} /> 清除 Key
                  </button>
                )}
                {editing && clearKeyRequested && (
                  <button type="button" onClick={undoClearKey} className="action-secondary shrink-0 px-3" title="撤销清除，保留原 Key">
                    <Undo size={15} /> 撤销
                  </button>
                )}
              </div>
              {form.protocol === "ollama" && (
                <p className="mt-1.5 text-xs text-muted-foreground">Ollama 本地默认无 API Key，可留空；远程反向代理可填写。</p>
              )}
              {!keyRequired && form.protocol !== "ollama" && (
                <p className="mt-1.5 text-xs text-muted-foreground">该提供商为可选鉴权（如 Ollama 兼容层），API Key 可留空。</p>
              )}
            </div>
          </div>

          {/* 模型列表 */}
          <div>
            <label className="mb-2 block text-sm font-medium">模型列表</label>
            <div className="mb-3 flex flex-wrap gap-2">
              <input
                value={modelInput}
                onChange={e => setModelInput(e.target.value)}
                onKeyDown={e => { if (e.key === "Enter") { e.preventDefault(); if (!e.nativeEvent.isComposing && e.keyCode !== 229) addModel(); } }}
                className="min-w-[200px] flex-1 rounded-2xl border border-border bg-background/70 px-4 py-3 text-sm"
                placeholder="输入模型名称，回车添加"
              />
              <button type="button" onClick={addModel} className="action-secondary px-4 py-3"><Plus size={16} /></button>
              <button type="button" disabled title="上游模型同步接口尚未开放（后端提供）；失败时不会覆盖已有模型列表" className="action-secondary px-3 py-3 disabled:opacity-40 disabled:cursor-not-allowed">
                <RefreshCw size={14} /> 同步上游模型
              </button>
            </div>
            <div className="flex flex-wrap gap-2">
              {form.models.map(m => (
                <span key={m} className="inline-flex items-center gap-1 rounded-full bg-primary/12 px-3 py-1.5 text-xs text-primary">
                  {m}
                  <button type="button" onClick={() => removeModel(m)} className="hover:text-red-300"><X size={12} /></button>
                </span>
              ))}
            </div>
            {form.models.length === 0 && (
              <p className="mt-1.5 text-xs text-muted-foreground">空模型列表表示「接受所有模型」（通配）。</p>
            )}
          </div>

          {/* 模型映射 */}
          <div>
            <div className="mb-2 flex items-center justify-between">
              <label className="text-sm font-medium">模型映射</label>
              <span className="text-xs text-muted-foreground">左侧填映射名（客户端请求用），右侧选渠道实际模型</span>
            </div>
            {mappings.length === 0 ? (
              <div className="rounded-2xl border border-dashed border-border bg-background/40 px-4 py-6 text-center">
                <p className="text-sm text-muted-foreground mb-3">尚未配置模型映射</p>
                <button type="button" onClick={addMapping} disabled={form.models.length === 0} className="action-secondary inline-flex items-center gap-1.5 disabled:opacity-40 disabled:cursor-not-allowed">
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
                    availableTargets={form.models}
                    existingFroms={Array.from(new Set([...globalFroms, ...mappings.map(m => m.from).filter(Boolean)])).sort()}
                    onRemove={() => removeMapping(idx)}
                    onChange={(field, value) => updateMapping(idx, field, value)}
                  />
                ))}
                <button type="button" onClick={addMapping} disabled={form.models.length === 0} className="action-secondary inline-flex items-center gap-1.5 disabled:opacity-40 disabled:cursor-not-allowed">
                  <Plus size={14} /> 添加映射
                </button>
              </div>
            )}
          </div>

          {/* 优先级 + 权重 */}
          <div className="grid grid-cols-1 gap-4 md:grid-cols-2">
            <div>
              <label className="mb-2 block text-sm font-medium">优先级</label>
              <input
                type="number"
                value={form.priority}
                onChange={e => setForm(prev => ({ ...prev, priority: parseInt(e.target.value) || 0 }))}
                className="w-full rounded-2xl border border-border bg-background/70 px-4 py-3 text-sm"
              />
              <p className="mt-1.5 text-xs text-muted-foreground">数字越大优先级越高，相同映射名的请求会优先路由到高优先级渠道</p>
            </div>
            <div>
              <label className="mb-2 block text-sm font-medium">权重</label>
              <input
                type="number"
                value={form.weight}
                onChange={e => setForm(prev => ({ ...prev, weight: parseInt(e.target.value) || 1 }))}
                className="w-full rounded-2xl border border-border bg-background/70 px-4 py-3 text-sm"
              />
              <p className="mt-1.5 text-xs text-muted-foreground">同优先级渠道间的负载均衡比例，数值越大分配的请求越多</p>
            </div>
          </div>

          {/* 超时 */}
          <div>
            <label className="mb-2 block text-sm font-medium">请求超时时间（秒）</label>
            <input
              type="number"
              min={1}
              max={600}
              value={form.timeout_secs}
              onChange={e => onTimeoutChange(Math.max(1, parseInt(e.target.value) || 60))}
              className="w-full rounded-2xl border border-border bg-background/70 px-4 py-3 text-sm"
            />
            <p className="mt-1.5 text-xs text-muted-foreground">该渠道请求的超时时间，默认 60 秒。流式请求也受此限制。超时后会自动重试下一个渠道</p>
          </div>

          {localError && (
            <div className="flex items-center justify-between rounded-2xl border border-red-200 bg-red-50 px-4 py-3 text-sm text-red-700">
              <span>{localError}</span>
              <button type="button" onClick={() => setLocalError(null)} className="ml-3 shrink-0 text-red-400 transition-colors hover:text-red-600"><X size={16} /></button>
            </div>
          )}

          {/* Actions */}
          <div className="flex items-center justify-between gap-3 pt-2">
            <p className="text-xs leading-5 text-muted-foreground">
              保存前会逐端点发送最小推理请求验证（<span className="font-medium">可能产生极少上游费用</span>）；失败时可选择「仍然保存」。
            </p>
            <div className="flex shrink-0 gap-2">
              <button type="button" onClick={onClose} disabled={saving} className="action-secondary">取消</button>
              <button type="submit" disabled={saving || testPhase === "running"} className="action-primary">
                {saving ? <Loader2 size={16} className="animate-spin" /> : <Check size={16} />}
                {saving ? "保存中…" : "保存"}
              </button>
            </div>
          </div>
        </form>
      </div>

      {/* 草稿测试弹窗 */}
      {(testPhase === "running" || (testPhase === "failed" && testResult)) && (
        <DraftTestModal
          phase={testPhase}
          result={testResult}
          saving={saving}
          saveError={saveError}
          onModify={() => { setTestPhase("idle"); setTestResult(null); setSaveError(null); }}
          onForceSave={handleForceSave}
        />
      )}

      {/* 切换确认弹窗 */}
      {pendingSwitch && !saving && (
        <ConfirmSwitchDialog
          onApply={onConfirmApply}
          onKeep={onConfirmKeep}
          onCancel={() => setPendingSwitch(null)}
        />
      )}
    </div>
  );
}

// ─── PresetCard ─────────────────────────────────────────────────────────────

function PresetCard({
  preset,
  selected,
  onSelect,
}: {
  preset: ChannelPreset;
  selected: boolean;
  onSelect: () => void;
}) {
  return (
    <button
      type="button"
      onClick={onSelect}
      aria-pressed={selected}
      className={`flex items-center gap-2.5 rounded-2xl border px-4 py-3 text-left transition-all w-full ${
        selected
          ? "border-primary/40 bg-primary/8 text-primary shadow-sm"
          : "border-border bg-white text-foreground hover:border-primary/30 hover:bg-muted/40"
      }`}
    >
      <span className="text-lg">{CHANNEL_PROVIDER_ICONS[preset.icon_key] ?? "❓"}</span>
      <span className="min-w-0 flex-1">
        <span className="block truncate text-sm font-medium">{preset.display_name}</span>
        {preset.description && <span className="block truncate text-xs text-muted-foreground">{preset.description}</span>}
      </span>
      {selected && <Check size={14} className="shrink-0 text-primary" />}
    </button>
  );
}

// ─── FixedEndpoint ──────────────────────────────────────────────────────────

function FixedEndpoint({ label, path, note }: { label: string; path: string; note: string }) {
  return (
    <div className="flex items-center gap-2 rounded-2xl border border-border bg-background/40 px-4 py-3 text-sm">
      <span className="flex h-4 w-4 items-center justify-center rounded border border-primary/40 bg-primary/10 text-[9px] font-bold text-primary">✓</span>
      <span className="shrink-0 font-semibold">{label}</span>
      <span className="font-mono text-muted-foreground">{path}</span>
      <span className="ml-auto shrink-0 rounded-full bg-muted px-2 py-0.5 text-xs text-muted-foreground">{note}</span>
    </div>
  );
}

