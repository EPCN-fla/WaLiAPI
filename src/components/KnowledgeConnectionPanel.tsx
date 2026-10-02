import { useEffect, useRef, useState } from "react";
import { Link } from "react-router-dom";
import { apiKeyApi, type KnowledgeHealthTest } from "../lib/api";
import type { ApiKey } from "../types";
import { writeClipboard } from "../lib/runtime";

export function KnowledgeConnectionPanel({ kbId }: { kbId: string }) {
  const [keys, setKeys] = useState<ApiKey[]>([]);
  const [keyId, setKeyId] = useState("");
  const [loading, setLoading] = useState(true);
  const [testing, setTesting] = useState(false);
  const [message, setMessage] = useState("");
  const [error, setError] = useState("");
  const [model, setModel] = useState("");
  const [question, setQuestion] = useState("");
  const [searchMode, setSearchMode] = useState("hybrid");
  const [health, setHealth] = useState<KnowledgeHealthTest | null>(null);
  const generation = useRef(0);
  const load = async () => {
    const current = ++generation.current;
    setLoading(true); setError(""); setMessage("");
    setHealth(null);
    try {
      const all = await apiKeyApi.getAll();
      const grants = await Promise.all(all.map(key => apiKeyApi.getKnowledgeAccess(key.id)));
      const eligible = all.filter((key, i) => key.status === 1 && grants[i].includes(kbId));
      if (current !== generation.current) return;
      setKeys(eligible); setKeyId(eligible[0]?.id || "");
    } catch (e) { if (current === generation.current) setError(String(e)); }
    finally { if (current === generation.current) setLoading(false); }
  };
  useEffect(() => { void load(); return () => { generation.current++; }; }, [kbId]);
  const test = async () => {
    const current = generation.current;
    setTesting(true); setMessage(""); setError("");
    try {
      const result = await apiKeyApi.testKnowledgeAccess(keyId, kbId);
      if (current !== generation.current) return;
      setMessage(`REST：${result.rest_ok ? "授权通过" : `未通过（HTTP ${result.rest_status}）`}；MCP：${result.mcp_ok ? "授权通过" : `未通过（HTTP ${result.mcp_status}，请检查 MCP 开关及权限）`}`);
    } catch (e) { if (current === generation.current) setError(String(e)); }
    finally { if (current === generation.current) setTesting(false); }
  };
  const testHealth = async () => {
    const current = generation.current;
    setTesting(true); setMessage(""); setError(""); setHealth(null);
    try {
      const result = await apiKeyApi.testKnowledgeHealth(keyId, kbId, model.trim(), question.trim(), searchMode);
      if (current === generation.current) setHealth(result);
    } catch (e) { if (current === generation.current) setError(String(e)); }
    finally { if (current === generation.current) setTesting(false); }
  };
  const copy = async () => {
    try { await writeClipboard(await apiKeyApi.getFull(keyId)); setMessage("API Key 已复制，请将它配置为客户端的 Bearer Token。"); }
    catch (e) { setError(String(e)); }
  };
  return (
    <div className="space-y-3 rounded-xl border border-slate-200 bg-white p-4 text-sm">
      <p className="font-semibold">API Key 授权</p>
      <p className="text-xs text-slate-500">新建密钥默认授权全部现有 RAG，新建 RAG 默认授权全部已有密钥。可在<Link to="/api-keys" className="text-blue-600 underline">密钥 → 知识库查询权限</Link>中调整，再使用已授权的 API Key 连接 REST 或 MCP，无需设置环境变量 Token。</p>
      <div className="flex flex-wrap gap-2">
        <select aria-label="已授权的 API Key" value={keyId} disabled={loading || testing} onChange={e => { setKeyId(e.target.value); setMessage(""); setError(""); setHealth(null); }} className="min-w-0 flex-1 rounded-lg border border-slate-200 px-3 py-2">
          {!keys.length && <option value="">{loading ? "正在加载…" : "尚无已授权的 API Key"}</option>}
          {keys.map(key => <option key={key.id} value={key.id}>{key.name} · {key.key}</option>)}
        </select>
        <button className="action-secondary" onClick={load} disabled={loading || testing}>刷新</button>
        <button className="action-secondary" onClick={copy} disabled={!keyId || loading || testing}>复制 API Key</button>
        <button className="action-primary disabled:opacity-50" onClick={test} disabled={!keyId || loading || testing}>{testing ? "检查中…" : "测试授权连接"}</button>
      </div>
      <p className="text-xs text-slate-500">连接测试只验证本机 REST / MCP 的查询授权，不调用模型。向量检索还需授权 {"Embedding"} 模型，问答还需授权生成模型并保有额度。</p>
      <div className="space-y-3 border-t border-slate-200 pt-3">
        <p className="font-semibold">RAG 健康检测</p>
        <p className="text-xs text-slate-500">使用上面选择的 API Key 发起真实问答，会消耗模型额度。请填写此知识库中有依据的问题；结果检查检索、回答和来源是否有效，不代表答案事实正确。</p>
        <div className="flex flex-wrap gap-2">
          <input aria-label="检测回答模型" placeholder="客户端实际使用的回答模型名称" value={model} disabled={testing} onChange={e => { setModel(e.target.value); setHealth(null); }} className="min-w-0 flex-1 rounded-lg border border-slate-200 px-3 py-2" />
          <select aria-label="检测检索模式" value={searchMode} disabled={testing} onChange={e => { setSearchMode(e.target.value); setHealth(null); }} className="rounded-lg border border-slate-200 px-3 py-2">
            <option value="hybrid">混合检索（含 Embedding）</option>
            <option value="vector">向量检索（含 Embedding）</option>
            <option value="keyword">关键词检索（跳过 Embedding）</option>
          </select>
        </div>
        <textarea aria-label="检测问题" placeholder="输入知识库中有依据的测试问题" rows={2} value={question} disabled={testing} onChange={e => { setQuestion(e.target.value); setHealth(null); }} className="w-full rounded-lg border border-slate-200 px-3 py-2" />
        <button className="action-primary disabled:opacity-50" onClick={testHealth} disabled={testing || loading || !keyId || !model.trim() || !question.trim()}>{testing ? "检查中…" : "运行真实 RAG 检测（消耗额度）"}</button>
        {health && <div role="status" className="space-y-2 rounded-lg bg-slate-50 p-3 text-xs">
          <p className={health.ok ? "font-semibold text-emerald-700" : "font-semibold text-red-600"}>{health.ok ? "RAG 链路通过" : "RAG 链路未通过"} · HTTP {health.status} · {(health.elapsed_ms / 1000).toFixed(2)} 秒</p>
          {health.diagnostics && <ul className="space-y-1">{health.diagnostics.stages.map((stage, index) => <li key={`${stage.stage}-${index}`}>
            {({ permission: "知识库授权", embedding: "向量化", retrieval: "检索片段", answer: "回答模型", validation: "答案与来源" } as Record<string, string>)[stage.stage] || stage.stage}：{({ passed: "通过", failed: "失败", skipped: "跳过" } as Record<string, string>)[stage.status] || stage.status} · {(stage.elapsed_ms / 1000).toFixed(2)} 秒
          </li>)}</ul>}
          {health.error && <p className="break-words text-red-600">{health.error.message} {health.error.code && `（${health.error.code}）`}</p>}
          {health.answer && <p className="whitespace-pre-wrap break-words">回答：{health.answer}</p>}
          {health.sources.map((source, index) => <p key={index} className="break-words">来源：{source.filename} — {source.snippet}</p>)}
          {(health.diagnostics?.request_id || health.error?.request_id) && <p className="break-all text-slate-500">请求编号：{health.diagnostics?.request_id || health.error?.request_id}</p>}
        </div>}
      </div>
      {message && <p role="status" className="text-xs text-slate-700">{message}</p>}
      {error && <p role="alert" className="text-xs text-red-600">{error}</p>}
    </div>
  );
}
