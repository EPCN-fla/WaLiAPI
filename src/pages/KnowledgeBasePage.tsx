import { useEffect, useState, useCallback } from "react";
import { useLocation } from "react-router-dom";
import {
  KnowledgeBase,
  KbDocument,
  KbSearchResult,
  KbRagAnswer,
  KbRetrievalDetail,
  KbSource,
  KbIndexMeta,
  KbTag,
  ConversationMessage,
  kbApi,
  channelApi,
  serviceApi,
  serverApi,
  type ServiceStatus,
} from "../lib/api";
import type { Channel } from "../types";
import {
  BookOpen,
  Plus,
  Trash2,
  Upload,
  Search,
  MessageCircle,
  RefreshCw,
  FileText,
  CheckCircle2,
  Loader2,
  XCircle,
  Clock,
  Hash,
  ChevronRight,
  ChevronDown,
  Check,
  Settings as SettingsIcon,
  Terminal,
  Server,
  Wifi,
  Copy,
  Layers,
  GitBranch,
  Link,
  FolderOpen,
  FolderInput,
  Sparkles,
  Database,
  Tag,
  Sliders,
  ChevronUp,
  Code,
  ExternalLink,
  Package,
  Rocket,
  Puzzle,
} from "lucide-react";

type ServiceTab = "knowledge" | "mcp" | "skills";
type KbTab = "documents" | "sources" | "search" | "ask" | "settings" | "index" | "mcp";

export function KnowledgeBasePage() {
  const location = useLocation();
  const initialTab: ServiceTab = location.pathname.includes("/mcp") ? "mcp" : location.pathname.includes("/skills") ? "skills" : "knowledge";
  const [serviceTab, setServiceTab] = useState<ServiceTab>(initialTab);

  const serviceTabs: { key: ServiceTab; label: string; icon: typeof BookOpen }[] = [
    { key: "knowledge", label: "知识库", icon: BookOpen },
    { key: "mcp", label: "MCP 服务", icon: Terminal },
    { key: "skills", label: "Skills 技能", icon: Puzzle },
  ];

  return (
    <div className="page-shell space-y-6">
      {/* Page Header */}
      <div className="page-header sticky top-0 z-30 -mx-7 -mt-7 mb-2 bg-white/90 px-7 py-5 backdrop-blur-md border-b border-slate-100">
        <div>
          <h1 className="page-title">服务</h1>
          <p className="page-subtitle">本地知识库 · MCP Server · 文档向量化 + HNSW 索引 · RAG 问答 · 支持 AI Agent 对接</p>
        </div>
        <div className="flex items-center gap-2">
          {serviceTabs.map(({ key, label, icon: Icon }) => (
            <button
              key={key}
              onClick={() => setServiceTab(key)}
              className={`flex items-center gap-2 rounded-xl px-4 py-2.5 text-sm font-medium transition-all ${
                serviceTab === key
                  ? "border border-blue-100 bg-white text-slate-900 shadow-[0_8px_18px_rgba(15,23,42,0.05)]"
                  : "text-slate-500 hover:bg-white/70 hover:text-slate-900"
              }`}
            >
              <Icon size={16} />
              {label}
            </button>
          ))}
        </div>
      </div>

      <div>
        {serviceTab === "knowledge" ? <KnowledgeBaseSection /> : serviceTab === "mcp" ? <McpSection /> : <SkillsSection />}
      </div>
    </div>
  );
}

// ─── MCP Service Section ─────────────────────────────────────────────────

const TOOL_ICONS: Record<string, typeof Terminal> = {
  search_knowledge_base: Search,
  list_knowledge_bases: BookOpen,
  read_document: FileText,
  ask_knowledge_base: MessageCircle,
  get_knowledge_base_stats: Database,
  create_knowledge_base: Plus,
  update_knowledge_base: SettingsIcon,
  delete_knowledge_base: Trash2,
  upload_document: Upload,
  delete_document: Trash2,
  list_documents: Layers,
  build_index: Sparkles,
  import_source: GitBranch,
};

function McpSection() {
  const [services, setServices] = useState<ServiceStatus[]>([]);
  const [loading, setLoading] = useState(true);
  const [copied, setCopied] = useState(false);

  useEffect(() => {
    serviceApi.getStatuses()
      .then(setServices)
      .catch(console.error)
      .finally(() => setLoading(false));
  }, []);

  const mcpService = services.find(s => s.id === "mcp");
  const kbService = services.find(s => s.id === "knowledge");
  const [serverUrl, setServerUrl] = useState("http://127.0.0.1:8777");

  useEffect(() => {
    serverApi.getStatus().then(s => {
      if (s.running) setServerUrl(`http://127.0.0.1:${s.port}`);
    }).catch(() => {});
  }, []);

  const baseUrl = serverUrl;
  const mcpEndpoint = `${baseUrl}/mcp`;
  const sseEndpoint = `${baseUrl}/mcp/sse`;
  const tools = (mcpService?.stats?.tools as { name: string; label: string; desc: string }[]) || [];

  const handleCopy = (text: string) => {
    navigator.clipboard.writeText(text);
    setCopied(true);
    setTimeout(() => setCopied(false), 2000);
  };

  if (loading) {
    return (
      <div className="flex items-center justify-center py-20">
        <Loader2 className="h-8 w-8 animate-spin text-slate-400" />
      </div>
    );
  }

  return (
    <div className="grid gap-4 lg:grid-cols-2">
      {/* Service Status */}
      <div className="surface data-card rounded-2xl">
        <div className="mb-4 flex items-center gap-2">
          <Server size={18} className="text-slate-700" />
          <h3 className="text-sm font-semibold text-slate-900">服务状态</h3>
        </div>
        <div className="space-y-3">
          {kbService && (
            <div className="rounded-xl border border-slate-100 bg-slate-50 p-4">
              <div className="flex items-center justify-between">
                <span className="text-sm font-medium text-slate-700">知识库服务</span>
                <span className={`flex items-center gap-1.5 text-xs ${kbService.running ? "text-emerald-600" : "text-red-500"}`}>
                  <Wifi size={12} /> {kbService.running ? "运行中" : "已停止"}
                </span>
              </div>
              <div className="mt-2 text-xs text-slate-500">
                知识库: {String(kbService.stats.knowledge_bases || 0)} · 文档: {String(kbService.stats.documents || 0)} · 切片: {String(kbService.stats.chunks || 0)}
              </div>
            </div>
          )}
          {mcpService && (
            <div className="rounded-xl border border-slate-100 bg-slate-50 p-4">
              <div className="flex items-center justify-between">
                <span className="text-sm font-medium text-slate-700">MCP 服务</span>
                <span className={`flex items-center gap-1.5 text-xs ${mcpService.running ? "text-emerald-600" : "text-red-500"}`}>
                  <Wifi size={12} /> {mcpService.running ? "运行中" : "已停止"}
                </span>
              </div>
              <div className="mt-2 text-xs text-slate-500">
                可用知识库: {String(mcpService.stats.available_knowledge_bases || 0)} · 工具: {tools.length}
              </div>
            </div>
          )}
        </div>
      </div>

      {/* MCP Endpoints */}
      <div className="surface data-card rounded-2xl">
        <div className="mb-4 flex items-center gap-2">
          <Terminal size={18} className="text-slate-700" />
          <h3 className="text-sm font-semibold text-slate-900">MCP 端点</h3>
        </div>
        <div className="space-y-3">
          <div>
            <label className="mb-1 block text-xs font-medium text-slate-500">JSON-RPC 端点（仅 POST，浏览器直接访问无效）</label>
            <div className="flex items-center gap-2">
              <code className="flex-1 truncate rounded-lg bg-slate-50 border border-slate-200 px-3 py-2 text-xs font-mono text-slate-800">{mcpEndpoint}</code>
              <button
                onClick={() => handleCopy(mcpEndpoint)}
                className="rounded-lg border border-slate-200 p-2 hover:bg-slate-50"
              >
                {copied ? <CheckCircle2 size={14} className="text-emerald-500" /> : <Copy size={14} className="text-slate-400" />}
              </button>
            </div>
          </div>
          <div>
            <label className="mb-1 block text-xs font-medium text-slate-500">SSE 端点（GET，可用于 EventSource）</label>
            <div className="flex items-center gap-2">
              <code className="flex-1 truncate rounded-lg bg-slate-50 border border-slate-200 px-3 py-2 text-xs font-mono text-slate-800">{sseEndpoint}</code>
              <button
                onClick={() => handleCopy(sseEndpoint)}
                className="rounded-lg border border-slate-200 p-2 hover:bg-slate-50"
              >
                {copied ? <CheckCircle2 size={14} className="text-emerald-500" /> : <Copy size={14} className="text-slate-400" />}
              </button>
            </div>
          </div>
          <div className="rounded-lg bg-amber-50 border border-amber-100 px-3 py-2 text-xs text-amber-700">
            ⚠️ MCP 端点仅接受 JSON-RPC POST 请求，浏览器直接打开会返回 405。请使用 curl 或 MCP 客户端调用。
          </div>
        </div>
      </div>

      {/* Available Tools */}
      <div className="surface data-card rounded-2xl">
        <div className="mb-4 flex items-center gap-2">
          <Terminal size={18} className="text-slate-700" />
          <h3 className="text-sm font-semibold text-slate-900">可用工具 ({tools.length})</h3>
        </div>
        <div className="grid grid-cols-1 gap-2 sm:grid-cols-2">
          {tools.map((tool) => {
            const icon = TOOL_ICONS[tool.name] || Terminal;
            const Icon = icon;
            return (
              <div key={tool.name} className="group flex items-start gap-3 rounded-xl border border-slate-100 bg-gradient-to-br from-white to-slate-50 px-3 py-2.5 transition-all hover:border-slate-200 hover:shadow-sm">
                <div className="mt-0.5 flex-shrink-0 rounded-lg bg-slate-100 p-1.5 text-slate-600 transition-colors group-hover:bg-slate-200">
                  <Icon size={14} />
                </div>
                <div className="min-w-0 flex-1">
                  <div className="flex items-center gap-2">
                    <span className="text-xs font-semibold text-slate-800">{tool.label}</span>
                    <code className="truncate text-[10px] font-normal text-slate-400">{tool.name}</code>
                  </div>
                  <p className="mt-0.5 text-[11px] leading-relaxed text-slate-500">{tool.desc}</p>
                </div>
              </div>
            );
          })}
        </div>
      </div>

      {/* Usage Example */}
      <div className="surface data-card rounded-2xl">
        <div className="mb-4 flex items-center gap-2">
          <Terminal size={18} className="text-slate-700" />
          <h3 className="text-sm font-semibold text-slate-900">调用示例</h3>
        </div>
        <pre className="overflow-x-auto rounded-xl bg-slate-50 border border-slate-200 p-4 text-xs"><code className="text-slate-800">{`curl -X POST ${mcpEndpoint} \\
  -H "Content-Type: application/json" \\
  -d '{
    "jsonrpc": "2.0",
    "id": 1,
    "method": "tools/list",
    "params": {}
  }'`}</code></pre>
      </div>
    </div>
  );
}

// ─── Skills Section ─────────────────────────────────────────────────────

function SkillsSection() {
  const [serverUrl, setServerUrl] = useState("http://127.0.0.1:8777");

  useEffect(() => {
    serverApi.getStatus().then(s => {
      if (s.running) setServerUrl(`http://127.0.0.1:${s.port}`);
    }).catch(() => {});
  }, []);

  const mcpEndpoint = `${serverUrl}/mcp`;

  return (
    <div className="space-y-4">
      {/* 头部介绍 */}
      <div className="surface data-card rounded-2xl">
        <div className="flex items-start gap-4">
          <div className="flex-shrink-0 rounded-2xl bg-gradient-to-br from-blue-500 to-indigo-600 p-3 text-white shadow-lg shadow-blue-500/20">
            <Puzzle size={24} />
          </div>
          <div className="flex-1">
            <div className="flex items-center gap-3">
              <h3 className="text-base font-semibold text-slate-900">WaLiAPI RAG Skills</h3>
              <span className="rounded-full bg-emerald-50 px-2 py-0.5 text-[11px] font-medium text-emerald-600 border border-emerald-100">v1.0.0</span>
            </div>
            <p className="mt-1 text-sm leading-relaxed text-slate-600">
              即装即用的 Agent Skill 技能包，通过 MCP 协议连接 WaLiAPI 本地知识库。安装后 AI Agent 可直接执行语义搜索、RAG 问答、文档管理等操作，无需手写提示词。
            </p>
            <div className="mt-3 flex flex-wrap items-center gap-2">
              <a
                href="https://github.com/fuzhengwei/waliapi-rag-skills"
                target="_blank"
                rel="noopener noreferrer"
                className="flex items-center gap-1.5 rounded-lg bg-slate-800 px-4 py-2 text-xs font-medium text-white transition-all hover:bg-slate-700"
              >
                <Code size={13} />
                GitHub 仓库
                <ExternalLink size={11} className="text-slate-300" />
              </a>
              <a
                href="https://github.com/fuzhengwei/waliapi-rag-skills#readme"
                target="_blank"
                rel="noopener noreferrer"
                className="flex items-center gap-1.5 rounded-lg border border-slate-200 px-4 py-2 text-xs font-medium text-slate-600 transition-all hover:border-slate-300 hover:bg-slate-50"
              >
                <FileText size={13} />
                使用文档
                <ExternalLink size={11} className="text-slate-400" />
              </a>
            </div>
          </div>
        </div>
      </div>

      {/* 安装步骤 + 使用方式 */}
      <div className="grid gap-4 lg:grid-cols-2">
        {/* 安装步骤 */}
        <div className="surface data-card rounded-2xl">
          <div className="mb-4 flex items-center gap-2">
            <Rocket size={18} className="text-slate-700" />
            <h3 className="text-sm font-semibold text-slate-900">安装步骤</h3>
          </div>
          <div className="space-y-2.5">
            <div className="flex items-start gap-3 rounded-lg border border-slate-100 bg-slate-50 px-3 py-2.5">
              <span className="flex h-5 w-5 flex-shrink-0 items-center justify-center rounded-full bg-slate-800 text-[10px] font-bold text-white">1</span>
              <div className="flex-1">
                <p className="text-xs font-medium text-slate-700">下载技能包</p>
                <p className="mt-0.5 text-[11px] text-slate-500">从 GitHub 仓库克隆到本地 skills 目录</p>
                <code className="mt-1.5 block rounded-lg border border-slate-200 bg-slate-900 px-2.5 py-1.5 text-[11px] font-mono text-slate-100">git clone https://github.com/fuzhengwei/waliapi-rag-skills.git ~/.qclaw/skills/waliapi-rag</code>
              </div>
            </div>
            <div className="flex items-start gap-3 rounded-lg border border-slate-100 bg-slate-50 px-3 py-2.5">
              <span className="flex h-5 w-5 flex-shrink-0 items-center justify-center rounded-full bg-slate-800 text-[10px] font-bold text-white">2</span>
              <div className="flex-1">
                <p className="text-xs font-medium text-slate-700">配置 MCP 服务地址</p>
                <p className="mt-0.5 text-[11px] text-slate-500">编辑配置文件，填入 WaLiAPI 的 MCP 服务地址</p>
                <pre className="mt-1.5 overflow-x-auto rounded-lg border border-slate-200 bg-slate-900 px-2.5 py-1.5 text-[11px] font-mono text-slate-100">{`cat > ~/.qclaw/skills/waliapi-rag/config.json << 'EOF'
{
  "mcp_url": "${mcpEndpoint}"
}
EOF`}</pre>
                <p className="mt-1.5 text-[11px] text-slate-400">💡 首次使用时 AI 也会自动询问 MCP 地址，可跳过手动配置</p>
              </div>
            </div>
            <div className="flex items-start gap-3 rounded-lg border border-slate-100 bg-slate-50 px-3 py-2.5">
              <span className="flex h-5 w-5 flex-shrink-0 items-center justify-center rounded-full bg-slate-800 text-[10px] font-bold text-white">3</span>
              <div className="flex-1">
                <p className="text-xs font-medium text-slate-700">重启 Agent 客户端</p>
                <p className="mt-0.5 text-[11px] text-slate-500">重启 QClaw / Claude Code / OpenClaw 等 Agent 客户端，技能会在启动时自动加载</p>
              </div>
            </div>
          </div>
        </div>

        {/* 使用方式 */}
        <div className="surface data-card rounded-2xl">
          <div className="mb-4 flex items-center gap-2">
            <Terminal size={18} className="text-slate-700" />
            <h3 className="text-sm font-semibold text-slate-900">使用方式</h3>
          </div>
          <div className="space-y-2.5">
            <div className="rounded-lg border border-slate-100 bg-gradient-to-br from-white to-slate-50 px-3 py-2.5">
              <div className="flex items-center gap-2">
                <Search size={14} className="text-blue-500" />
                <p className="text-xs font-semibold text-slate-800">语义搜索</p>
              </div>
              <p className="mt-1 text-[11px] text-slate-500">「搜索知识库中关于渠道配置的内容」</p>
              <p className="mt-0.5 text-[11px] text-slate-400">调用 search_knowledge_base，支持 hybrid/vector/keyword 三种模式</p>
            </div>
            <div className="rounded-lg border border-slate-100 bg-gradient-to-br from-white to-slate-50 px-3 py-2.5">
              <div className="flex items-center gap-2">
                <MessageCircle size={14} className="text-emerald-500" />
                <p className="text-xs font-semibold text-slate-800">RAG 问答</p>
              </div>
              <p className="mt-1 text-[11px] text-slate-500">「问一下知识库，WaLiAPI 支持哪些协议？」</p>
              <p className="mt-0.5 text-[11px] text-slate-400">调用 ask_knowledge_base，检索 + LLM 生成回答 + 来源引用</p>
            </div>
            <div className="rounded-lg border border-slate-100 bg-gradient-to-br from-white to-slate-50 px-3 py-2.5">
              <div className="flex items-center gap-2">
                <Upload size={14} className="text-amber-500" />
                <p className="text-xs font-semibold text-slate-800">文档管理</p>
              </div>
              <p className="mt-1 text-[11px] text-slate-500">「把这份 PDF 上传到知识库」</p>
              <p className="mt-0.5 text-[11px] text-slate-400">调用 upload_document，自动解析 → 分块 → 向量化 → 索引</p>
            </div>
            <div className="rounded-lg border border-slate-100 bg-gradient-to-br from-white to-slate-50 px-3 py-2.5">
              <div className="flex items-center gap-2">
                <GitBranch size={14} className="text-purple-500" />
                <p className="text-xs font-semibold text-slate-800">批量导入</p>
              </div>
              <p className="mt-1 text-[11px] text-slate-500">「把这个 Git 仓库导入知识库」</p>
              <p className="mt-0.5 text-[11px] text-slate-400">调用 import_source，支持 Git 仓库 / URL / 本地目录</p>
            </div>
          </div>
        </div>
      </div>

      {/* 技术细节 + 工具覆盖 */}
      <div className="grid gap-4 lg:grid-cols-2">
        {/* 技术细节 */}
        <div className="surface data-card rounded-2xl">
          <div className="mb-4 flex items-center gap-2">
            <Package size={18} className="text-slate-700" />
            <h3 className="text-sm font-semibold text-slate-900">技术细节</h3>
          </div>
          <div className="space-y-2.5">
            <div className="flex items-center justify-between rounded-lg border border-slate-100 bg-slate-50 px-3 py-2">
              <span className="text-xs font-medium text-slate-500">通信协议</span>
              <span className="text-xs text-slate-800">MCP JSON-RPC (SSE + POST)</span>
            </div>
            <div className="flex items-center justify-between rounded-lg border border-slate-100 bg-slate-50 px-3 py-2">
              <span className="text-xs font-medium text-slate-500">运行依赖</span>
              <span className="text-xs text-slate-800">Python 3.8+（零第三方依赖）</span>
            </div>
            <div className="flex items-center justify-between rounded-lg border border-slate-100 bg-slate-50 px-3 py-2">
              <span className="text-xs font-medium text-slate-500">兼容客户端</span>
              <span className="text-xs text-slate-800">QClaw · Claude Code · OpenClaw</span>
            </div>
            <div className="flex items-center justify-between rounded-lg border border-slate-100 bg-slate-50 px-3 py-2">
              <span className="text-xs font-medium text-slate-500">MCP 工具数</span>
              <span className="text-xs text-slate-800">13 个（5 只读 + 8 写入）</span>
            </div>
            <div className="flex items-center justify-between rounded-lg border border-slate-100 bg-slate-50 px-3 py-2">
              <span className="text-xs font-medium text-slate-500">许可证</span>
              <span className="text-xs text-slate-800">MIT</span>
            </div>
          </div>
        </div>

        {/* MCP 工具覆盖 */}
        <div className="surface data-card rounded-2xl">
          <div className="mb-4 flex items-center gap-2">
            <Terminal size={18} className="text-slate-700" />
            <h3 className="text-sm font-semibold text-slate-900">MCP 工具覆盖（13 个）</h3>
          </div>
          <div className="grid grid-cols-1 gap-1.5 sm:grid-cols-2">
            {[
              { name: "search_knowledge_base", label: "语义搜索", icon: Search, color: "text-blue-500" },
              { name: "ask_knowledge_base", label: "RAG 问答", icon: MessageCircle, color: "text-emerald-500" },
              { name: "list_knowledge_bases", label: "列出知识库", icon: BookOpen, color: "text-slate-500" },
              { name: "read_document", label: "读取文档", icon: FileText, color: "text-slate-500" },
              { name: "get_knowledge_base_stats", label: "知识库统计", icon: Database, color: "text-slate-500" },
              { name: "create_knowledge_base", label: "创建知识库", icon: Plus, color: "text-indigo-500" },
              { name: "update_knowledge_base", label: "更新知识库", icon: SettingsIcon, color: "text-indigo-500" },
              { name: "delete_knowledge_base", label: "删除知识库", icon: Trash2, color: "text-red-400" },
              { name: "upload_document", label: "上传文档", icon: Upload, color: "text-amber-500" },
              { name: "delete_document", label: "删除文档", icon: Trash2, color: "text-red-400" },
              { name: "list_documents", label: "文档列表", icon: Layers, color: "text-slate-500" },
              { name: "build_index", label: "构建索引", icon: Sparkles, color: "text-purple-500" },
              { name: "import_source", label: "导入源", icon: GitBranch, color: "text-purple-500" },
            ].map((tool) => {
              const Icon = tool.icon;
              return (
                <div key={tool.name} className="flex items-center gap-2 rounded-lg border border-slate-100 bg-white px-2.5 py-1.5">
                  <Icon size={12} className={tool.color} />
                  <span className="text-[11px] font-medium text-slate-700">{tool.label}</span>
                  <code className="ml-auto truncate text-[10px] text-slate-400">{tool.name}</code>
                </div>
              );
            })}
          </div>
        </div>
      </div>

      {/* 快速验证 */}
      <div className="surface data-card rounded-2xl">
        <div className="mb-4 flex items-center gap-2">
          <Terminal size={18} className="text-slate-700" />
          <h3 className="text-sm font-semibold text-slate-900">快速验证</h3>
        </div>
        <p className="mb-3 text-xs text-slate-500">安装完成后，在 Agent 客户端中发送以下消息验证技能是否生效：</p>
        <div className="space-y-2">
          <div className="rounded-lg border border-slate-100 bg-slate-50 px-3 py-2">
            <p className="text-[11px] font-medium text-slate-400">验证连接</p>
            <code className="text-xs text-slate-700">列出所有知识库</code>
          </div>
          <div className="rounded-lg border border-slate-100 bg-slate-50 px-3 py-2">
            <p className="text-[11px] font-medium text-slate-400">验证搜索</p>
            <code className="text-xs text-slate-700">搜索知识库中关于配置的内容</code>
          </div>
          <div className="rounded-lg border border-slate-100 bg-slate-50 px-3 py-2">
            <p className="text-[11px] font-medium text-slate-400">验证 RAG 问答</p>
            <code className="text-xs text-slate-700">问一下知识库，WaLiAPI 支持哪些协议？</code>
          </div>
        </div>
      </div>
    </div>
  );
}

// ─── Knowledge Base Section ──────────────────────────────────────────────

function KnowledgeBaseSection() {
  const [kbs, setKbs] = useState<KnowledgeBase[]>([]);
  const [selectedKb, setSelectedKb] = useState<KnowledgeBase | null>(null);
  const [kbTab, setKbTab] = useState<KbTab>("documents");
  const [loading, setLoading] = useState(false);
  const [showCreate, setShowCreate] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const fetchKbs = useCallback(async () => {
    setLoading(true);
    try {
      const data = await kbApi.getAll();
      setKbs(data);
    } catch (e) {
      setError(String(e));
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    fetchKbs();
  }, [fetchKbs]);

  const handleSelectKb = (kb: KnowledgeBase) => {
    setSelectedKb(kb);
    setKbTab("documents");
  };

  // Keep selectedKb in sync with kbs list (so counts refresh after upload/etc)
  useEffect(() => {
    if (selectedKb) {
      const updated = kbs.find((k) => k.id === selectedKb.id);
      if (updated && (updated.doc_count !== selectedKb.doc_count || updated.chunk_count !== selectedKb.chunk_count || updated.total_tokens !== selectedKb.total_tokens || updated.status !== selectedKb.status || updated.mcp_enabled !== selectedKb.mcp_enabled)) {
        setSelectedKb(updated);
      }
    }
  }, [kbs, selectedKb]);

  const handleDelete = async (id: string) => {
    if (!confirm("确定删除此知识库？所有文档和切片将一并删除。")) return;
    try {
      await kbApi.delete(id);
      await fetchKbs();
      if (selectedKb?.id === id) setSelectedKb(null);
    } catch (e) {
      setError(String(e));
    }
  };

  // Toggle KB status (enable/disable) from list view
  const handleToggleStatus = async (kb: KnowledgeBase, newStatus: number) => {
    try {
      await kbApi.update(kb.id, { status: newStatus });
      await fetchKbs();
    } catch (e) {
      setError(String(e));
    }
  };

  // Toggle MCP exposure from list view
  const handleToggleMcp = async (kb: KnowledgeBase, newMcp: number) => {
    try {
      await kbApi.update(kb.id, { mcp_enabled: newMcp });
      await fetchKbs();
    } catch (e) {
      setError(String(e));
    }
  };

  return (
    <>
      {error && (
        <div className="rounded-xl border border-red-200 bg-red-50 p-4 text-sm text-red-600">
          {error}
          <button onClick={() => setError(null)} className="ml-2 text-red-400 hover:text-red-600">✕</button>
        </div>
      )}

      {selectedKb ? (
        <KbDetail
          kb={selectedKb}
          tab={kbTab}
          setTab={setKbTab}
          onBack={() => { setSelectedKb(null); setKbTab("documents"); }}
          onRefresh={fetchKbs}
        />
      ) : (
        <KbList
          kbs={kbs}
          loading={loading}
          onSelect={handleSelectKb}
          onDelete={handleDelete}
          onCreate={() => setShowCreate(true)}
          onToggleStatus={handleToggleStatus}
          onToggleMcp={handleToggleMcp}
        />
      )}

      {showCreate && (
        <CreateKbModal
          onClose={() => setShowCreate(false)}
          onCreated={async () => {
            setShowCreate(false);
            await fetchKbs();
          }}
        />
      )}
    </>
  );
}

// ─── KB Tags Bar (high-frequency words) ─────────────────────────────

function KbTagsBar({ kbId, chunkCount }: { kbId: string; chunkCount: number }) {
  const [tags, setTags] = useState<KbTag[]>([]);
  const [loading, setLoading] = useState(false);

  useEffect(() => {
    if (chunkCount === 0) return;
    let active = true;
    setLoading(true);
    kbApi.getTags(kbId, 12)
      .then((data) => { if (active) setTags(data); })
      .catch(() => {})
      .finally(() => { if (active) setLoading(false); });
    return () => { active = false; };
  }, [kbId, chunkCount]);

  if (loading && tags.length === 0) {
    return (
      <div className="mt-2.5 flex items-center gap-1.5">
        {[...Array(5)].map((_, i) => (
          <div key={i} className="h-5 w-12 animate-pulse rounded-full bg-slate-100" />
        ))}
      </div>
    );
  }

  if (tags.length === 0) return null;

  // Color palette for tags - gradient blues/purples for visual appeal
  const tagColors = [
    "bg-blue-50 text-blue-600 border-blue-100",
    "bg-violet-50 text-violet-600 border-violet-100",
    "bg-emerald-50 text-emerald-600 border-emerald-100",
    "bg-amber-50 text-amber-600 border-amber-100",
    "bg-rose-50 text-rose-500 border-rose-100",
    "bg-cyan-50 text-cyan-600 border-cyan-100",
    "bg-indigo-50 text-indigo-600 border-indigo-100",
    "bg-teal-50 text-teal-600 border-teal-100",
  ];

  return (
    <div className="mt-2.5 flex flex-wrap items-center gap-1.5">
      <Tag size={11} className="text-slate-400 shrink-0" />
      {tags.map((tag, i) => (
        <span
          key={tag.word}
          className={`inline-flex items-center rounded-full border px-2 py-0.5 text-[10px] font-medium ${tagColors[i % tagColors.length]}`}
        >
          {tag.word}
        </span>
      ))}
    </div>
  );
}

// ─── KB List ────────────────────────────────────────────────────────────

function KbList({
  kbs,
  loading,
  onSelect,
  onDelete,
  onCreate,
  onToggleStatus,
  onToggleMcp,
}: {
  kbs: KnowledgeBase[];
  loading: boolean;
  onSelect: (kb: KnowledgeBase) => void;
  onDelete: (id: string) => void;
  onCreate: () => void;
  onToggleStatus: (kb: KnowledgeBase, newStatus: number) => void;
  onToggleMcp: (kb: KnowledgeBase, newMcp: number) => void;
}) {
  if (loading && kbs.length === 0) {
    return (
      <div className="surface empty-state">
        <Loader2 className="h-8 w-8 animate-spin text-slate-400" />
      </div>
    );
  }

  if (kbs.length === 0) {
    return (
      <div className="surface empty-state">
        <BookOpen className="h-12 w-12 text-slate-300" />
        <p className="text-sm text-slate-500">还没有知识库</p>
        <button onClick={onCreate} className="action-primary mt-2">
          <Plus size={16} />
          新建知识库
        </button>
      </div>
    );
  }

  return (
    <>
      <div className="flex justify-end mb-4">
        <button onClick={onCreate} className="action-primary">
          <Plus size={16} />
          新建知识库
        </button>
      </div>
      <div className="space-y-3">
        {kbs.map((kb) => (
          <div
            key={kb.id}
            className="surface group rounded-2xl p-5 transition-all hover:shadow-[0_8px_24px_rgba(15,23,42,0.06)] border border-slate-100"
          >
            <div className="flex items-start gap-4">
              {/* Icon */}
              <div className={`flex h-10 w-10 shrink-0 items-center justify-center rounded-xl ${kb.status === 1 ? "bg-blue-50" : "bg-slate-100"}`}>
                <BookOpen className={`h-5 w-5 ${kb.status === 1 ? "text-blue-600" : "text-slate-400"}`} />
              </div>

              {/* Main content - clickable */}
              <div
                className="min-w-0 flex-1 cursor-pointer"
                onClick={() => onSelect(kb)}
              >
                <div className="flex items-center gap-2">
                  <h3 className="text-base font-semibold text-slate-900">{kb.name}</h3>
                  {kb.status === 1 ? (
                    <span className="rounded-full bg-emerald-50 px-2 py-0.5 text-[10px] font-medium text-emerald-600">活跃</span>
                  ) : (
                    <span className="rounded-full bg-slate-100 px-2 py-0.5 text-[10px] font-medium text-slate-500">已禁用</span>
                  )}
                </div>
                <p className="mt-0.5 text-xs text-slate-500 line-clamp-1">
                  {kb.description || "暂无描述"}
                </p>
                <div className="mt-2 flex items-center gap-4 text-xs text-slate-500">
                  <span className="flex items-center gap-1">
                    <FileText size={12} /> {kb.doc_count} 文档
                  </span>
                  <span className="flex items-center gap-1">
                    <Hash size={12} /> {kb.chunk_count} 切片
                  </span>
                  {kb.embedding_model && (
                    <span className="truncate" title={kb.embedding_model}>
                      {kb.embedding_model}
                    </span>
                  )}
                </div>
                {/* Tags */}
                <KbTagsBar kbId={kb.id} chunkCount={kb.chunk_count} />
              </div>

              {/* Right side: toggles + actions */}
              <div className="flex flex-col items-end gap-2 shrink-0">
                <div className="flex items-center gap-3">
                  {/* MCP toggle */}
                  <button
                    onClick={(e) => {
                      e.stopPropagation();
                      onToggleMcp(kb, kb.mcp_enabled === 1 ? 0 : 1);
                    }}
                    className={`flex items-center gap-1.5 rounded-lg px-2 py-1 text-[10px] font-medium transition-colors ${
                      kb.mcp_enabled === 1
                        ? "bg-violet-50 text-violet-600 hover:bg-violet-100"
                        : "bg-slate-100 text-slate-400 hover:bg-slate-200"
                    }`}
                    title="MCP 暴露开关"
                  >
                    <Terminal size={11} />
                    MCP {kb.mcp_enabled === 1 ? "已暴露" : "未暴露"}
                  </button>

                  {/* Status toggle */}
                  <button
                    onClick={(e) => {
                      e.stopPropagation();
                      onToggleStatus(kb, kb.status === 1 ? 0 : 1);
                    }}
                    className={`relative inline-flex h-5 w-9 shrink-0 items-center rounded-full transition-colors ${
                      kb.status === 1 ? "bg-emerald-500" : "bg-slate-300"
                    }`}
                    title="知识库开关"
                  >
                    <span
                      className={`inline-block h-3.5 w-3.5 transform rounded-full bg-white shadow transition-transform ${
                        kb.status === 1 ? "translate-x-4" : "translate-x-1"
                      }`}
                    />
                  </button>

                  {/* Delete */}
                  <button
                    onClick={(e) => { e.stopPropagation(); onDelete(kb.id); }}
                    className="rounded-lg p-1.5 text-slate-400 opacity-0 transition-opacity group-hover:opacity-100 hover:bg-red-50 hover:text-red-500"
                  >
                    <Trash2 className="h-4 w-4" />
                  </button>
                </div>
                <ChevronRight size={16} className="text-slate-300 group-hover:text-blue-500" />
              </div>
            </div>
          </div>
        ))}
      </div>
    </>
  );
}

// ─── KB Detail ───────────────────────────────────────────────────────────

function KbDetail({
  kb,
  tab,
  setTab,
  onBack,
  onRefresh,
}: {
  kb: KnowledgeBase;
  tab: KbTab;
  setTab: (t: KbTab) => void;
  onBack: () => void;
  onRefresh: () => void;
}) {
  const tabs: { key: KbTab; label: string; icon: typeof FileText }[] = [
    { key: "documents", label: "文档", icon: FileText },
    { key: "sources", label: "来源", icon: GitBranch },
    { key: "search", label: "检索", icon: Search },
    { key: "ask", label: "问答", icon: MessageCircle },
    { key: "index", label: "索引", icon: Database },
    { key: "settings", label: "设置", icon: SettingsIcon },
    { key: "mcp", label: "MCP", icon: Terminal },
  ];

  return (
    <div className="space-y-4">
      <div className="flex items-center gap-3">
        <button
          onClick={onBack}
          className="flex items-center gap-1 rounded-lg px-3 py-1.5 text-sm text-slate-500 hover:bg-slate-100"
        >
          ← 返回
        </button>
        <div className="h-4 w-px bg-slate-200" />
        <h2 className="text-lg font-semibold text-slate-900">{kb.name}</h2>
        <span className="rounded-full bg-emerald-50 px-2 py-0.5 text-[10px] font-medium text-emerald-600">
          {kb.doc_count} 文档 · {kb.chunk_count} 切片
        </span>
      </div>

      {/* Tabs */}
      <div className="flex gap-1 border-b border-slate-200">
        {tabs.map(({ key, label, icon: Icon }) => (
          <button
            key={key}
            onClick={() => setTab(key)}
            className={`flex items-center gap-2 border-b-2 px-4 py-2.5 text-sm transition-colors ${
              tab === key
                ? "border-blue-600 text-blue-600"
                : "border-transparent text-slate-500 hover:text-slate-700"
            }`}
          >
            <Icon size={15} />
            {label}
          </button>
        ))}
      </div>

      {tab === "documents" && <DocumentsTab kb={kb} onRefresh={onRefresh} />}
      {tab === "sources" && <SourcesTab kb={kb} onRefresh={onRefresh} />}
      {tab === "search" && <SearchTab kb={kb} />}
      {tab === "ask" && <AskTab kb={kb} />}
      {tab === "index" && <IndexTab kb={kb} />}
      {tab === "settings" && <SettingsTab kb={kb} onRefresh={onRefresh} />}
      {tab === "mcp" && <McpTab kb={kb} />}
    </div>
  );
}

// ─── Sources Tab (Multi-source import) ────────────────────────────────

function SourcesTab({ kb, onRefresh }: { kb: KnowledgeBase; onRefresh: () => void }) {
  const [sources, setSources] = useState<KbSource[]>([]);
  const [loading, setLoading] = useState(false);
  const [showImport, setShowImport] = useState(false);
  const [progressMap, setProgressMap] = useState<Record<string, { progress: number; detail: string }>>({});

  const fetchSources = useCallback(async () => {
    setLoading(true);
    try {
      const data = await kbApi.getSources(kb.id);
      setSources(data);
    } catch (e) {
      console.error(e);
    } finally {
      setLoading(false);
    }
  }, [kb.id]);

  useEffect(() => {
    fetchSources();
    const interval = setInterval(fetchSources, 3000);
    return () => clearInterval(interval);
  }, [fetchSources]);

  // Listen for import progress
  useEffect(() => {
    let unlisten: (() => void) | undefined;
    let active = true;
    (async () => {
      const { listen } = await import("@tauri-apps/api/event");
      unlisten = await listen<{ kb_id: string; source_id: string; progress: number; detail: string }>(
        "kb-import-progress",
        (event) => {
          if (!active) return;
          const p = event.payload;
          if (p.kb_id !== kb.id) return;
          if (p.progress >= 100) {
            setProgressMap((prev) => {
              const next = { ...prev };
              delete next[p.source_id];
              return next;
            });
            fetchSources();
            onRefresh();
          } else {
            setProgressMap((prev) => ({
              ...prev,
              [p.source_id]: { progress: p.progress, detail: p.detail },
            }));
          }
        }
      );
    })();
    return () => {
      active = false;
      if (unlisten) unlisten();
    };
  }, [kb.id, fetchSources, onRefresh]);

  const handleDelete = async (sourceId: string) => {
    if (!confirm("删除此来源？关联的文档将保留但不再标记来源。")) return;
    try {
      await kbApi.deleteSource(sourceId, kb.id);
      await fetchSources();
      onRefresh();
    } catch (e) {
      alert(`删除失败: ${e}`);
    }
  };

  return (
    <div className="space-y-4">
      <div className="flex justify-end">
        <button onClick={() => setShowImport(true)} className="action-primary">
          <Plus size={16} />
          导入来源
        </button>
      </div>

      {loading && sources.length === 0 ? (
        <div className="flex items-center justify-center py-12">
          <Loader2 className="h-6 w-6 animate-spin text-slate-400" />
        </div>
      ) : sources.length === 0 ? (
        <div className="surface empty-state rounded-2xl">
          <GitBranch className="h-8 w-8 text-slate-300" />
          <p className="text-sm text-slate-500">暂无导入来源</p>
          <p className="text-xs text-slate-400 mt-1">从 Git 仓库、URL 或本地目录导入文档</p>
        </div>
      ) : (
        <div className="space-y-2">
          {sources.map((src) => {
            const prog = progressMap[src.id];
            return (
              <div key={src.id} className="surface flex items-center gap-3 rounded-xl px-4 py-3">
                <SourceIcon type={src.source_type} />
                <div className="min-w-0 flex-1">
                  <div className="flex items-center gap-2">
                    <span className="truncate text-sm font-medium text-slate-900">
                      {src.source_url || src.source_path || src.source_type}
                    </span>
                    {src.branch && src.source_type === "git" && (
                      <span className="rounded bg-slate-100 px-1.5 py-0.5 text-[10px] text-slate-500">
                        {src.branch}
                      </span>
                    )}
                  </div>
                  {prog ? (
                    <div className="mt-1.5">
                      <div className="flex items-center gap-2">
                        <div className="h-1.5 flex-1 overflow-hidden rounded-full bg-slate-200">
                          <div
                            className="h-full rounded-full bg-blue-500 transition-all duration-300"
                            style={{ width: `${prog.progress}%` }}
                          />
                        </div>
                        <span className="shrink-0 text-[11px] text-blue-600">
                          {prog.detail} · {prog.progress}%
                        </span>
                      </div>
                    </div>
                  ) : (
                    <div className="mt-1 flex items-center gap-3 text-xs text-slate-500">
                      <SourceStatusBadge status={src.status} />
                      {src.file_count > 0 && <span>{src.file_count} 文件</span>}
                      {src.error && (
                        <span className="text-red-500 truncate" title={src.error}>
                          {src.error.slice(0, 60)}
                        </span>
                      )}
                    </div>
                  )}
                </div>
                <button
                  onClick={() => handleDelete(src.id)}
                  className="rounded-lg p-1.5 text-slate-400 hover:bg-red-50 hover:text-red-500"
                  title="删除"
                >
                  <Trash2 size={15} />
                </button>
              </div>
            );
          })}
        </div>
      )}

      {showImport && (
        <ImportSourceModal
          kbId={kb.id}
          onClose={() => setShowImport(false)}
          onImported={async () => {
            setShowImport(false);
            await fetchSources();
            onRefresh();
          }}
        />
      )}
    </div>
  );
}

function SourceIcon({ type }: { type: string }) {
  const cls = "h-5 w-5 shrink-0 text-slate-400";
  switch (type) {
    case "git":
      return <GitBranch className={cls} />;
    case "url":
      return <Link className={cls} />;
    case "local_dir":
      return <FolderOpen className={cls} />;
    default:
      return <FileText className={cls} />;
  }
}

function SourceStatusBadge({ status }: { status: string }) {
  switch (status) {
    case "done":
      return <span className="flex items-center gap-1 text-emerald-600"><CheckCircle2 size={12} /> 完成</span>;
    case "processing":
      return <span className="flex items-center gap-1 text-blue-600"><Loader2 size={12} className="animate-spin" /> 处理中</span>;
    case "error":
      return <span className="flex items-center gap-1 text-red-500"><XCircle size={12} /> 失败</span>;
    default:
      return <span className="flex items-center gap-1 text-slate-400"><Clock size={12} /> 等待中</span>;
  }
}

function IndexStatusBadge({ status }: { status: string }) {
  switch (status) {
    case "ready":
      return <span className="flex items-center gap-1 text-emerald-600"><CheckCircle2 size={12} /> 就绪</span>;
    case "building":
      return <span className="flex items-center gap-1 text-blue-600"><Loader2 size={12} className="animate-spin" /> 构建中</span>;
    case "error":
      return <span className="flex items-center gap-1 text-red-500"><XCircle size={12} /> 失败</span>;
    case "none":
      return <span className="flex items-center gap-1 text-slate-400"><Clock size={12} /> 未构建</span>;
    default:
      return <span className="flex items-center gap-1 text-slate-400"><Clock size={12} /> {status}</span>;
  }
}

function ImportSourceModal({
  kbId,
  onClose,
  onImported,
}: {
  kbId: string;
  onClose: () => void;
  onImported: () => void;
}) {
  const [sourceType, setSourceType] = useState<"git" | "url" | "local_dir">("git");
  const [repoUrl, setRepoUrl] = useState("");
  const [branch, setBranch] = useState("main");
  const [token, setToken] = useState("");
  const [url, setUrl] = useState("");
  const [dirPath, setDirPath] = useState("");
  const [excludedDirs, setExcludedDirs] = useState("");
  const [includedFiles, setIncludedFiles] = useState("");
  const [maxFileSize, setMaxFileSize] = useState(1);
  const [importing, setImporting] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const handleImport = async () => {
    setImporting(true);
    setError(null);
    try {
      const input: Record<string, unknown> = {
        source_type: sourceType,
        excluded_dirs: excludedDirs ? excludedDirs.split(",").map((s) => s.trim()) : [],
        included_files: includedFiles ? includedFiles.split(",").map((s) => s.trim()) : [],
        max_file_size: maxFileSize * 1024 * 1024,
      };

      if (sourceType === "git") {
        if (!repoUrl.trim()) { setError("请输入仓库 URL"); setImporting(false); return; }
        input.repo_url = repoUrl.trim();
        input.branch = branch.trim() || "main";
        if (token.trim()) input.token = token.trim();
      } else if (sourceType === "url") {
        if (!url.trim()) { setError("请输入 URL"); setImporting(false); return; }
        input.url = url.trim();
      } else if (sourceType === "local_dir") {
        if (!dirPath.trim()) { setError("请输入目录路径"); setImporting(false); return; }
        input.dir_path = dirPath.trim();
      }

      await kbApi.importSource(kbId, input as Parameters<typeof kbApi.importSource>[1]);
      onImported();
    } catch (e) {
      setError(String(e));
    } finally {
      setImporting(false);
    }
  };

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/30" onClick={onClose}>
      <div className="w-full max-w-lg rounded-2xl bg-white p-6 shadow-xl" onClick={(e) => e.stopPropagation()}>
        <h3 className="text-lg font-semibold text-slate-900">导入来源</h3>

        {/* Source type tabs */}
        <div className="mt-4 flex gap-2">
          {([
            { key: "git" as const, label: "Git 仓库", icon: GitBranch },
            { key: "url" as const, label: "URL", icon: Link },
            { key: "local_dir" as const, label: "本地目录", icon: FolderOpen },
          ]).map(({ key, label, icon: Icon }) => (
            <button
              key={key}
              onClick={() => setSourceType(key)}
              className={`flex items-center gap-2 rounded-xl px-4 py-2.5 text-sm font-medium transition-all ${
                sourceType === key
                  ? "border border-blue-100 bg-white text-slate-900 shadow-sm"
                  : "text-slate-500 hover:bg-white/70"
              }`}
            >
              <Icon size={15} />
              {label}
            </button>
          ))}
        </div>

        <div className="mt-4 space-y-4">
          {sourceType === "git" && (
            <>
              <div>
                <label className="mb-1 block text-sm font-medium text-slate-700">仓库 URL</label>
                <input
                  type="text"
                  value={repoUrl}
                  onChange={(e) => setRepoUrl(e.target.value)}
                  placeholder="https://github.com/owner/repo"
                  className="w-full rounded-xl border border-slate-200 px-3 py-2 text-sm outline-none focus:border-blue-400 focus:ring-2 focus:ring-blue-100"
                />
              </div>
              <div className="grid grid-cols-2 gap-3">
                <div>
                  <label className="mb-1 block text-sm font-medium text-slate-700">分支</label>
                  <input
                    type="text"
                    value={branch}
                    onChange={(e) => setBranch(e.target.value)}
                    placeholder="main"
                    className="w-full rounded-xl border border-slate-200 px-3 py-2 text-sm outline-none focus:border-blue-400 focus:ring-2 focus:ring-blue-100"
                  />
                </div>
                <div>
                  <label className="mb-1 block text-sm font-medium text-slate-700">Access Token（可选）</label>
                  <input
                    type="password"
                    value={token}
                    onChange={(e) => setToken(e.target.value)}
                    placeholder="私有仓库需要"
                    className="w-full rounded-xl border border-slate-200 px-3 py-2 text-sm outline-none focus:border-blue-400 focus:ring-2 focus:ring-blue-100"
                  />
                </div>
              </div>
            </>
          )}

          {sourceType === "url" && (
            <div>
              <label className="mb-1 block text-sm font-medium text-slate-700">URL</label>
              <input
                type="text"
                value={url}
                onChange={(e) => setUrl(e.target.value)}
                placeholder="https://example.com/doc.md"
                className="w-full rounded-xl border border-slate-200 px-3 py-2 text-sm outline-none focus:border-blue-400 focus:ring-2 focus:ring-blue-100"
              />
            </div>
          )}

          {sourceType === "local_dir" && (
            <div>
              <label className="mb-1 block text-sm font-medium text-slate-700">目录路径</label>
              <div className="flex gap-2">
                <input
                  type="text"
                  value={dirPath}
                  onChange={(e) => setDirPath(e.target.value)}
                  placeholder="/path/to/project/docs"
                  className="flex-1 rounded-xl border border-slate-200 px-3 py-2 text-sm outline-none focus:border-blue-400 focus:ring-2 focus:ring-blue-100"
                />
                <button
                  type="button"
                  onClick={async () => {
                    try {
                      const { open } = await import("@tauri-apps/plugin-dialog");
                      const selected = await open({
                        directory: true,
                        multiple: false,
                        title: "选择导入目录",
                      });
                      if (typeof selected === "string") {
                        setDirPath(selected);
                      }
                    } catch {
                      // 对话框取消或不可用，忽略
                    }
                  }}
                  className="flex items-center gap-1.5 rounded-xl border border-slate-200 bg-slate-50 px-3 py-2 text-sm font-medium text-slate-600 transition-colors hover:bg-slate-100 hover:text-slate-900"
                >
                  <FolderInput size={15} />
                  浏览
                </button>
              </div>
            </div>
          )}

          {/* Common filter options */}
          <div className="rounded-xl bg-slate-50 p-3 space-y-3">
            <div className="text-xs font-semibold text-slate-500">过滤选项（可选）</div>
            <div className="grid grid-cols-2 gap-3">
              <div>
                <label className="mb-1 block text-xs text-slate-600">排除目录（逗号分隔）</label>
                <input
                  type="text"
                  value={excludedDirs}
                  onChange={(e) => setExcludedDirs(e.target.value)}
                  placeholder="tests, examples, docs"
                  className="w-full rounded-lg border border-slate-200 px-2.5 py-1.5 text-xs outline-none focus:border-blue-400"
                />
              </div>
              <div>
                <label className="mb-1 block text-xs text-slate-600">包含文件类型（逗号分隔，空=全部）</label>
                <input
                  type="text"
                  value={includedFiles}
                  onChange={(e) => setIncludedFiles(e.target.value)}
                  placeholder="md, rs, ts, py"
                  className="w-full rounded-lg border border-slate-200 px-2.5 py-1.5 text-xs outline-none focus:border-blue-400"
                />
              </div>
            </div>
            <div>
              <label className="mb-1 block text-xs text-slate-600">最大文件大小 (MB)</label>
              <input
                type="number"
                value={maxFileSize}
                onChange={(e) => setMaxFileSize(Number(e.target.value) || 1)}
                min={0.1}
                step={0.1}
                className="w-24 rounded-lg border border-slate-200 px-2.5 py-1.5 text-xs outline-none focus:border-blue-400"
              />
            </div>
          </div>

          {error && (
            <div className="rounded-lg bg-red-50 p-3 text-sm text-red-600">{error}</div>
          )}
        </div>

        <div className="mt-6 flex justify-end gap-2">
          <button onClick={onClose} className="rounded-xl px-4 py-2 text-sm text-slate-500 hover:bg-slate-100">
            取消
          </button>
          <button
            onClick={handleImport}
            disabled={importing}
            className="action-primary disabled:opacity-50"
          >
            {importing ? "导入中..." : "开始导入"}
          </button>
        </div>
      </div>
    </div>
  );
}

// ─── Index Tab ─────────────────────────────────────────────────────────

function IndexTab({ kb }: { kb: KnowledgeBase }) {
  const [indexMeta, setIndexMeta] = useState<KbIndexMeta | null>(null);
  const [loading, setLoading] = useState(true);
  const [building, setBuilding] = useState(false);
  const [buildMsg, setBuildMsg] = useState("");
  const [buildProgress, setBuildProgress] = useState(0);

  const fetchIndex = useCallback(async () => {
    setLoading(true);
    try {
      const data = await kbApi.getIndexStatus(kb.id);
      setIndexMeta(data);
      // Sync building state with DB status
      if (data?.status === "building") setBuilding(true);
      else setBuilding(false);
    } catch (e) {
      console.error(e);
    } finally {
      setLoading(false);
    }
  }, [kb.id]);

  useEffect(() => {
    fetchIndex();

    // Listen for real-time index build progress
    let unlisten: (() => void) | undefined;
    (async () => {
      const { listen } = await import("@tauri-apps/api/event");
      unlisten = await listen<{ kb_id: string; status: string; message: string; progress?: number; current?: number; total?: number }>(
        "kb-index-progress",
        (event) => {
          const payload = event.payload;
          if (payload.kb_id !== kb.id) return;

          setBuildMsg(payload.message);

          if (payload.status === "ready") {
            setBuilding(false);
            setBuildProgress(100);
            setBuildMsg("");
            fetchIndex();
          } else if (payload.status === "error") {
            setBuilding(false);
            setBuildProgress(0);
            // Keep error message visible
          } else if (payload.status === "building") {
            setBuilding(true);
            setBuildProgress(payload.progress ?? 0);
          }
        }
      );
    })();

    return () => {
      if (unlisten) unlisten();
    };
  }, [fetchIndex, kb.id]);

  const handleBuild = async () => {
    setBuilding(true);
    setBuildProgress(0);
    setBuildMsg("正在构建 HNSW 向量索引…");
    try {
      await kbApi.buildIndex(kb.id);
      // Progress will come via Tauri event listener
    } catch (e) {
      setBuilding(false);
      setBuildMsg("");
      alert(`构建失败: ${e}`);
    }
  };

  const handleDrop = async () => {
    if (!confirm("确定删除索引？删除后需重新构建。")) return;
    try {
      await kbApi.dropIndex(kb.id);
      await fetchIndex();
    } catch (e) {
      alert(`删除失败: ${e}`);
    }
  };

  if (loading) {
    return (
      <div className="flex items-center justify-center py-12">
        <Loader2 className="h-6 w-6 animate-spin text-slate-400" />
      </div>
    );
  }

  return (
    <div className="grid gap-4 lg:grid-cols-2">
      {/* Index status */}
      <div className="surface data-card rounded-2xl">
        <div className="mb-4 flex items-center gap-2">
          <Database size={18} className="text-slate-700" />
          <h3 className="text-sm font-semibold text-slate-900">索引状态</h3>
        </div>
        {indexMeta ? (
          <div className="space-y-3">
            <div className="grid grid-cols-2 gap-3">
              <div className="rounded-xl bg-slate-50 p-3">
                <div className="text-xs text-slate-500">索引类型</div>
                <div className="text-sm font-medium text-slate-900 mt-1">{indexMeta.index_type || "linear"}</div>
              </div>
              <div className="rounded-xl bg-slate-50 p-3">
                <div className="text-xs text-slate-500">状态</div>
                <div className="text-sm font-medium mt-1">
                  <IndexStatusBadge status={indexMeta.status} />
                </div>
              </div>
              <div className="rounded-xl bg-slate-50 p-3">
                <div className="text-xs text-slate-500">Embedding 维度</div>
                <div className="text-sm font-medium text-slate-900 mt-1">{indexMeta.embedding_dim || "未检测"}</div>
              </div>
              <div className="rounded-xl bg-slate-50 p-3">
                <div className="text-xs text-slate-500">切片数量</div>
                <div className="text-sm font-medium text-slate-900 mt-1">{indexMeta.chunk_count}</div>
              </div>
            </div>
            {indexMeta.built_at && (
              <div className="text-xs text-slate-400">构建时间: {indexMeta.built_at}</div>
            )}
          </div>
        ) : (
          <div className="text-sm text-slate-500">暂无索引信息</div>
        )}
      </div>

      {/* Actions */}
      <div className="surface data-card rounded-2xl">
        <div className="mb-4 flex items-center gap-2">
          <SettingsIcon size={18} className="text-slate-700" />
          <h3 className="text-sm font-semibold text-slate-900">索引操作</h3>
        </div>
        <div className="space-y-3">
          {building && buildMsg && (
            <div className="rounded-lg bg-blue-50 border border-blue-100 px-3 py-2.5 text-xs text-blue-600">
              <div className="flex items-center gap-2 mb-1.5">
                <Loader2 className="h-3 w-3 animate-spin shrink-0" />
                <span>{buildMsg}</span>
              </div>
              {buildProgress >= 0 && buildProgress < 100 && (
                <div className="mt-1 h-1.5 w-full rounded-full bg-blue-100 overflow-hidden">
                  <div
                    className="h-full bg-blue-500 rounded-full transition-all duration-300"
                    style={{ width: `${Math.max(buildProgress, 3)}%` }}
                  />
                </div>
              )}
            </div>
          )}
          {!building && buildMsg && (
            <div className="rounded-lg bg-red-50 border border-red-100 px-3 py-2 text-xs text-red-600">
              {buildMsg}
            </div>
          )}
          <button
            onClick={handleBuild}
            disabled={building}
            className="action-primary w-full disabled:opacity-50"
          >
            {building ? (
              <><Loader2 className="h-4 w-4 animate-spin" /> 构建中...</>
            ) : (
              <><Database size={16} /> 构建索引</>
            )}
          </button>
          <button
            onClick={handleDrop}
            disabled={!indexMeta || indexMeta.status === "none"}
            className="w-full rounded-xl border border-red-200 px-4 py-2 text-sm text-red-600 hover:bg-red-50 disabled:opacity-50"
          >
            <Trash2 size={16} />
            删除索引
          </button>
          <div className="rounded-lg bg-blue-50 border border-blue-100 px-3 py-2 text-xs text-blue-600">
            ℹ️ 使用 HNSW 图索引，平均查询复杂度 O(log n)。构建后自动用于检索加速。
          </div>
        </div>
      </div>
    </div>
  );
}

// ─── Documents Tab ────────────────────────────────────────────────────────

function DocumentsTab({ kb, onRefresh }: { kb: KnowledgeBase; onRefresh: () => void }) {
  const [docs, setDocs] = useState<KbDocument[]>([]);
  const [loading, setLoading] = useState(false);
  const [uploadingCount, setUploadingCount] = useState(0);
  const [uploadTotal, setUploadTotal] = useState(0);
  const [errorNotices, setErrorNotices] = useState<{ doc_id: string; filename: string; error: string }[]>([]);
  const [progressMap, setProgressMap] = useState<Record<string, { stage: string; progress: number; detail: string }>>({});

  const fetchDocs = useCallback(async () => {
    setLoading(true);
    try {
      const data = await kbApi.getDocuments(kb.id);
      setDocs(data);
    } catch (e) {
      console.error(e);
    } finally {
      setLoading(false);
    }
  }, [kb.id]);

  useEffect(() => {
    fetchDocs();
    const interval = setInterval(fetchDocs, 3000);
    return () => clearInterval(interval);
  }, [fetchDocs]);

  // Listen for document processing errors from backend
  useEffect(() => {
    let unlisten: (() => void) | undefined;
    let active = true;
    (async () => {
      const { listen } = await import("@tauri-apps/api/event");
      unlisten = await listen<{ doc_id: string; kb_id: string; filename: string; error: string }>(
        "kb-document-error",
        (event) => {
          if (!active) return;
          const payload = event.payload;
          if (payload.kb_id !== kb.id) return;
          setErrorNotices((prev) => [...prev, payload]);
          setProgressMap((prev) => {
            const next = { ...prev };
            delete next[payload.doc_id];
            return next;
          });
          setTimeout(() => {
            setErrorNotices((prev) => prev.filter((n) => n.doc_id !== payload.doc_id));
          }, 8000);
          fetchDocs();
          onRefresh();
        }
      );
    })();
    return () => {
      active = false;
      if (unlisten) unlisten();
    };
  }, [kb.id, fetchDocs, onRefresh]);

  // Listen for document processing progress from backend
  useEffect(() => {
    let unlisten: (() => void) | undefined;
    let active = true;
    (async () => {
      const { listen } = await import("@tauri-apps/api/event");
      unlisten = await listen<{ doc_id: string; kb_id: string; filename: string; stage: string; progress: number; detail: string }>(
        "kb-document-progress",
        (event) => {
          if (!active) return;
          const p = event.payload;
          if (p.kb_id !== kb.id) return;
          if (p.stage === "done") {
            setProgressMap((prev) => {
              const next = { ...prev };
              delete next[p.doc_id];
              return next;
            });
            fetchDocs();
            onRefresh();
          } else {
            setProgressMap((prev) => ({
              ...prev,
              [p.doc_id]: { stage: p.stage, progress: p.progress, detail: p.detail },
            }));
          }
        }
      );
    })();
    return () => {
      active = false;
      if (unlisten) unlisten();
    };
  }, [kb.id, fetchDocs, onRefresh]);

  const handleUploadBatch = async (files: File[]) => {
    if (files.length === 0) return;
    setUploadTotal(files.length);
    setUploadingCount(0);
    for (const file of files) {
      try {
        const content = await fileToBase64(file);
        await kbApi.uploadDocument({
          kb_id: kb.id,
          filename: file.name,
          content,
        });
      } catch (e) {
        console.error(`Upload failed for ${file.name}:`, e);
        alert(`上传失败 ${file.name}: ${e}`);
      }
      setUploadingCount(prev => prev + 1);
    }
    setUploadTotal(0);
    setUploadingCount(0);
    await fetchDocs();
    onRefresh();
  };

  const handleDelete = async (docId: string) => {
    if (!confirm("删除此文档？")) return;
    try {
      await kbApi.deleteDocument(docId, kb.id);
      await fetchDocs();
      onRefresh();
    } catch (e) {
      alert(`删除失败: ${e}`);
    }
  };

  const handleReindex = async (docId: string) => {
    try {
      await kbApi.reindexDocument(docId);
      await fetchDocs();
    } catch (e) {
      alert(`重新索引失败: ${e}`);
    }
  };

  return (
    <div className="space-y-4">
      {/* Upload zone */}
      <label className="flex cursor-pointer items-center justify-center rounded-2xl border-2 border-dashed border-slate-300 bg-white px-6 py-8 transition-colors hover:border-blue-400 hover:bg-blue-50/30">
        <input
          type="file"
          className="hidden"
          multiple
          accept=".md,.txt,.json,.yaml,.yml,.rs,.ts,.tsx,.js,.py,.go,.java,.c,.cpp,.h,.sh,.toml,.xml,.html,.css,.pdf"
          onChange={(e) => {
            const files = Array.from(e.target.files || []);
            if (files.length > 0) handleUploadBatch(files);
            e.target.value = "";
          }}
          disabled={uploadTotal > 0}
        />
        {uploadTotal > 0 ? (
          <div className="flex items-center gap-2 text-sm text-blue-600">
            <Loader2 className="h-5 w-5 animate-spin" />
            上传中 {uploadingCount}/{uploadTotal}...
          </div>
        ) : (
          <div className="flex flex-col items-center gap-2 text-sm text-slate-500">
            <Upload className="h-6 w-6" />
            <span>点击或拖拽上传文件到知识库（支持多选）</span>
            <span className="text-xs text-slate-400">支持 md/txt/code/json/yaml/pdf</span>
          </div>
        )}
      </label>

      {/* Error notices */}
      {errorNotices.length > 0 && (
        <div className="space-y-2">
          {errorNotices.map((notice) => (
            <div
              key={notice.doc_id}
              className="flex items-start gap-3 rounded-xl border border-red-200 bg-red-50 px-4 py-3"
            >
              <XCircle className="mt-0.5 h-5 w-5 shrink-0 text-red-500" />
              <div className="min-w-0 flex-1">
                <div className="text-sm font-medium text-red-800">
                  {notice.filename} 处理失败
                </div>
                <div className="mt-0.5 text-xs text-red-600">{notice.error}</div>
              </div>
              <button
                onClick={() =>
                  setErrorNotices((prev) =>
                    prev.filter((n) => n.doc_id !== notice.doc_id)
                  )
                }
                className="shrink-0 rounded-lg p-1 text-red-400 hover:bg-red-100 hover:text-red-600"
              >
                <Trash2 size={14} />
              </button>
            </div>
          ))}
        </div>
      )}

      {/* Documents list */}
      {loading && docs.length === 0 ? (
        <div className="flex items-center justify-center py-12">
          <Loader2 className="h-6 w-6 animate-spin text-slate-400" />
        </div>
      ) : docs.length === 0 ? (
        <div className="surface empty-state rounded-2xl">
          <FileText className="h-8 w-8 text-slate-300" />
          <p className="text-sm text-slate-500">暂无文档</p>
        </div>
      ) : (
        <div className="space-y-2">
          {docs.map((doc) => {
            const prog = progressMap[doc.id];
            return (
            <div
              key={doc.id}
              className="surface flex items-center gap-3 rounded-xl px-4 py-3"
            >
              <DocStatusIcon status={prog ? "processing" : doc.status} />

              <div className="min-w-0 flex-1">
                <div className="flex items-center gap-2">
                  <span className="truncate text-sm font-medium text-slate-900">
                    {doc.filename}
                  </span>
                  <span className="rounded bg-slate-100 px-1.5 py-0.5 text-[10px] text-slate-500">
                    {doc.file_type}
                  </span>
                </div>
                {prog ? (
                  <div className="mt-1.5">
                    <div className="flex items-center gap-2">
                      <div className="h-1.5 flex-1 overflow-hidden rounded-full bg-slate-200">
                        <div
                          className="h-full rounded-full bg-blue-500 transition-all duration-300"
                          style={{ width: `${prog.progress}%` }}
                        />
                      </div>
                      <span className="shrink-0 text-[11px] text-blue-600">
                        {prog.detail} · {prog.progress}%
                      </span>
                    </div>
                  </div>
                ) : (
                  <div className="mt-1 flex items-center gap-3 text-xs text-slate-500">
                    <span>{formatSize(doc.file_size)}</span>
                    {doc.chunk_count > 0 && <span>{doc.chunk_count} 切片</span>}
                    {doc.token_count > 0 && <span>{doc.token_count} tokens</span>}
                    {doc.error_message && (
                      <span className="text-red-500" title={doc.error_message}>
                        {doc.error_message.slice(0, 50)}
                      </span>
                    )}
                  </div>
                )}
              </div>

              <div className="flex items-center gap-1">
                <button
                  onClick={() => handleReindex(doc.id)}
                  className="rounded-lg p-1.5 text-slate-400 hover:bg-slate-100 hover:text-slate-600"
                  title="重新索引"
                >
                  <RefreshCw size={15} />
                </button>
                <button
                  onClick={() => handleDelete(doc.id)}
                  className="rounded-lg p-1.5 text-slate-400 hover:bg-red-50 hover:text-red-500"
                  title="删除"
                >
                  <Trash2 size={15} />
                </button>
              </div>
            </div>
            );
          })}
        </div>
      )}
    </div>
  );
}

// ─── Search Tab ──────────────────────────────────────────────────────────

function SearchTab({ kb }: { kb: KnowledgeBase }) {
  const [query, setQuery] = useState("");
  const [results, setResults] = useState<KbSearchResult[]>([]);
  const [searching, setSearching] = useState(false);
  const [searched, setSearched] = useState(false);
  const [tags, setTags] = useState<KbTag[]>([]);
  const [tagsLoading, setTagsLoading] = useState(false);

  // Load tags for preset search terms
  useEffect(() => {
    if (kb.chunk_count === 0) return;
    let active = true;
    setTagsLoading(true);
    kbApi.getTags(kb.id, 8)
      .then((data) => { if (active) setTags(data); })
      .catch(() => {})
      .finally(() => { if (active) setTagsLoading(false); });
    return () => { active = false; };
  }, [kb.id, kb.chunk_count]);

  const handleSearch = async (searchQuery?: string) => {
    const q = (searchQuery ?? query).trim();
    if (!q) return;
    if (searchQuery) setQuery(searchQuery);
    setSearching(true);
    setSearched(true);
    try {
      const data = await kbApi.search({ query: q, kb_id: kb.id, top_k: 10 });
      setResults(data);
    } catch (e) {
      console.error(e);
    } finally {
      setSearching(false);
    }
  };

  return (
    <div className="space-y-4">
      <div className="flex gap-2">
        <input
          type="text"
          value={query}
          onChange={(e) => setQuery(e.target.value)}
          onKeyDown={(e) => e.key === "Enter" && !e.nativeEvent.isComposing && handleSearch()}
          placeholder="输入搜索内容..."
          className="flex-1 rounded-xl border border-slate-200 bg-white px-4 py-2.5 text-sm outline-none focus:border-blue-400 focus:ring-2 focus:ring-blue-100"
        />
        <button
          onClick={() => handleSearch()}
          disabled={searching || !query.trim()}
          className="action-primary disabled:opacity-50"
        >
          {searching ? <Loader2 className="h-4 w-4 animate-spin" /> : <Search size={16} />}
          搜索
        </button>
      </div>

      {/* Preset search terms */}
      {(tagsLoading || tags.length > 0) && (
        <div className="flex flex-wrap items-center gap-2">
          <span className="flex items-center gap-1 text-[11px] font-medium text-slate-400">
            <Sparkles size={12} />
            快速检索
          </span>
          {tagsLoading ? (
            <>
              {[...Array(5)].map((_, i) => (
                <div key={i} className="h-6 w-16 animate-pulse rounded-full bg-slate-100" />
              ))}
            </>
          ) : (
            tags.map((tag) => (
              <button
                key={tag.word}
                onClick={() => setQuery(tag.word)}
                className="inline-flex items-center rounded-full border border-slate-200 bg-gradient-to-br from-slate-50 to-white px-3 py-1 text-xs font-medium text-slate-600 transition-all hover:border-blue-200 hover:bg-blue-50 hover:text-blue-600 hover:shadow-sm"
              >
                {tag.word}
              </button>
            ))
          )}
        </div>
      )}

      {searched && !searching && results.length === 0 && (
        <div className="surface empty-state rounded-2xl">
          <Search className="h-8 w-8 text-slate-300" />
          <p className="text-sm text-slate-500">未找到相关内容</p>
        </div>
      )}

      {results.length > 0 && (
        <div className="space-y-3">
          {results.map((r, i) => (
            <div key={r.chunk_id} className="surface rounded-xl p-4">
              <div className="mb-2 flex items-center gap-2">
                <span className="rounded bg-blue-50 px-2 py-0.5 text-[10px] font-medium text-blue-600">
                  #{i + 1}
                </span>
                <span className="text-xs font-medium text-slate-700">{r.filename}</span>
                <span className="text-xs text-slate-400">
                  相似度: {(r.score * 100).toFixed(1)}%
                </span>
              </div>
              <p className="text-sm text-slate-600 whitespace-pre-wrap line-clamp-6">
                {r.content}
              </p>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}

// ─── Ask Tab (RAG) ──────────────────────────────────────────────────────

function AskTab({ kb }: { kb: KnowledgeBase }) {
  const [question, setQuestion] = useState("");
  const [answer, setAnswer] = useState<KbRagAnswer | null>(null);
  const [asking, setAsking] = useState(false);
  const [channels, setChannels] = useState<Channel[]>([]);
  const [selectedChannelId, setSelectedChannelId] = useState<string>("");
  const [selectedModel, setSelectedModel] = useState<string>("");
  const [showChannelPicker, setShowChannelPicker] = useState(false);
  const [showModelPicker, setShowModelPicker] = useState(false);
  const [conversation, setConversation] = useState<Array<{ role: "user" | "assistant"; content: string; sources?: KbRagAnswer["sources"]; retrievalDetails?: KbRetrievalDetail[] | null }>>([]);
  const [deepResearch, setDeepResearch] = useState(false);
  const [showSearchConfig, setShowSearchConfig] = useState(false);
  const [searchMode, setSearchMode] = useState<"hybrid" | "vector" | "keyword">("hybrid");
  const [vectorWeight, setVectorWeight] = useState(0.7);
  const [keywordWeight, setKeywordWeight] = useState(0.3);
  const [topK, setTopK] = useState(5);
  const [showRetrievalDetails, setShowRetrievalDetails] = useState<number | null>(null);

  // Persistence key for this KB's ask preferences
  const storageKey = `kb_ask_prefs_${kb.id}`;

  useEffect(() => {
    channelApi.getAll().then((chs) => {
      const active = chs.filter((c) => c.status === 1);
      setChannels(active);

      // Load saved preferences from localStorage
      try {
        const saved = localStorage.getItem(storageKey);
        if (saved) {
          const prefs = JSON.parse(saved);
          // Validate that saved channel still exists and is active
          const savedCh = active.find(c => c.id === prefs.channelId);
          if (savedCh) {
            setSelectedChannelId(savedCh.id);
            // Validate saved model exists in that channel
            if (prefs.model && savedCh.models.includes(prefs.model)) {
              setSelectedModel(prefs.model);
            } else {
              setSelectedModel(savedCh.models[0] || "");
            }
            return;
          }
        }
      } catch {}

      // Fallback: auto-select first channel with models
      const first = active.find((c) => c.models.length > 0);
      if (first) {
        setSelectedChannelId(first.id);
        setSelectedModel(first.models[0]);
      }
    }).catch(console.error);
  }, [storageKey]);

  // Persist preferences when they change
  useEffect(() => {
    if (selectedChannelId && selectedModel) {
      localStorage.setItem(storageKey, JSON.stringify({
        channelId: selectedChannelId,
        model: selectedModel,
      }));
    }
  }, [storageKey, selectedChannelId, selectedModel]);

  // Models from selected channel
  const selectedChannel = channels.find((c) => c.id === selectedChannelId);
  const channelModels = selectedChannel?.models ?? [];

  const handleSelectChannel = (chId: string) => {
    setSelectedChannelId(chId);
    const ch = channels.find((c) => c.id === chId);
    if (ch && ch.models.length > 0) {
      setSelectedModel(ch.models[0]);
    } else {
      setSelectedModel("");
    }
    setShowChannelPicker(false);
  };

  const handleSelectModel = (model: string) => {
    setSelectedModel(model);
    setShowModelPicker(false);
  };

  const handleAsk = async () => {
    if (!question.trim()) return;
    setAsking(true);
    const userMsg = question;
    setQuestion("");
    setConversation((prev) => [...prev, { role: "user", content: userMsg }]);
    try {
      // Build history from current conversation (last 20 messages)
      const history: ConversationMessage[] = conversation.slice(-20).map((m) => ({
        role: m.role,
        content: m.content,
      }));

      const result = await kbApi.ask({
        question: userMsg,
        kb_id: kb.id,
        top_k: topK,
        model: selectedModel || undefined,
        history,
        deep_research: deepResearch,
        max_rounds: 5,
        vector_weight: searchMode === "hybrid" ? vectorWeight : undefined,
        keyword_weight: searchMode === "hybrid" ? keywordWeight : undefined,
        search_mode: searchMode,
      });
      setAnswer(result);
      setConversation((prev) => [
        ...prev,
        { role: "assistant", content: result.answer, sources: result.sources, retrievalDetails: result.retrieval_details },
      ]);
    } catch (e) {
      const errMsg = `请求失败: ${e}`;
      setAnswer({ answer: errMsg, sources: [], usage: null, retrieval_details: null });
      setConversation((prev) => [...prev, { role: "assistant", content: errMsg }]);
    } finally {
      setAsking(false);
    }
  };

  return (
    <div className="flex flex-col h-[calc(100vh-300px)] min-h-[360px]">
      {/* Model selector bar — top fixed */}
      <div className="flex items-center gap-3 border-b border-border bg-background/60 rounded-t-2xl px-4 py-3 shrink-0">
          {/* Channel selector */}
          <div className="relative">
            <button
              type="button"
              onClick={() => { setShowChannelPicker(!showChannelPicker); setShowModelPicker(false); }}
              className="flex items-center gap-2 rounded-xl border border-border bg-white px-3 py-2 text-xs font-medium transition-all hover:border-primary/40 hover:shadow-sm"
            >
              <span className="text-muted-foreground">渠道</span>
              <span className={selectedChannel ? "text-foreground truncate max-w-[120px]" : "text-muted-foreground"}>
                {selectedChannel?.name ?? "选择渠道"}
              </span>
              <ChevronDown size={13} className={`shrink-0 text-muted-foreground transition-transform ${showChannelPicker ? "rotate-180" : ""}`} />
            </button>

            {showChannelPicker && (
              <>
                <div className="fixed inset-0 z-40" onClick={() => setShowChannelPicker(false)} />
                <div className="absolute left-0 top-full z-50 mt-1.5 w-56 rounded-2xl border border-border bg-white p-2 shadow-xl max-h-[280px] overflow-auto">
                  <div className="px-2 py-1.5 text-[11px] font-semibold text-muted-foreground/70 uppercase tracking-wide">活跃渠道</div>
                  {channels.length === 0 ? (
                    <div className="px-3 py-2 text-xs text-muted-foreground">暂无可用渠道</div>
                  ) : channels.map((ch) => (
                    <button
                      key={ch.id}
                      type="button"
                      onClick={() => handleSelectChannel(ch.id)}
                      className={`flex w-full items-center justify-between rounded-xl px-3 py-2.5 text-sm transition-all ${
                        selectedChannelId === ch.id
                          ? "bg-primary/8 text-primary font-semibold"
                          : "text-foreground hover:bg-muted/60"
                      }`}
                    >
                      <div className="flex items-center gap-2 min-w-0">
                        <span className="truncate">{ch.name}</span>
                        <span className="rounded-full bg-muted px-1.5 py-0.5 text-[10px] text-muted-foreground shrink-0">
                          {ch.type}
                        </span>
                      </div>
                      {selectedChannelId === ch.id && <Check size={14} className="shrink-0" />}
                    </button>
                  ))}
                </div>
              </>
            )}
          </div>

          {/* Arrow */}
          <ChevronRight size={14} className="shrink-0 text-muted-foreground/40" />

          {/* Model selector */}
          <div className="relative">
            <button
              type="button"
              onClick={() => { setShowModelPicker(!showModelPicker); setShowChannelPicker(false); }}
              disabled={!selectedChannelId}
              className="flex items-center gap-2 rounded-xl border border-border bg-white px-3 py-2 text-xs font-medium transition-all hover:border-primary/40 hover:shadow-sm disabled:opacity-50 disabled:cursor-not-allowed"
            >
              <span className="text-muted-foreground">模型</span>
              <span className={selectedModel ? "text-foreground truncate max-w-[160px]" : "text-muted-foreground"}>
                {selectedModel || "选择模型"}
              </span>
              <ChevronDown size={13} className={`shrink-0 text-muted-foreground transition-transform ${showModelPicker ? "rotate-180" : ""}`} />
            </button>

            {showModelPicker && selectedChannelId && (
              <>
                <div className="fixed inset-0 z-40" onClick={() => setShowModelPicker(false)} />
                <div className="absolute left-0 top-full z-50 mt-1.5 w-56 rounded-2xl border border-border bg-white p-2 shadow-xl max-h-[280px] overflow-auto">
                  <div className="px-2 py-1.5 text-[11px] font-semibold text-muted-foreground/70 uppercase tracking-wide">
                    {selectedChannel?.name} 模型
                  </div>
                  {channelModels.length === 0 ? (
                    <div className="px-3 py-2 text-xs text-muted-foreground">该渠道未配置模型</div>
                  ) : channelModels.map((m) => (
                    <button
                      key={m}
                      type="button"
                      onClick={() => handleSelectModel(m)}
                      className={`flex w-full items-center justify-between rounded-xl px-3 py-2.5 text-sm font-mono transition-all ${
                        selectedModel === m
                          ? "bg-primary/8 text-primary font-semibold"
                          : "text-foreground hover:bg-muted/60"
                      }`}
                    >
                      <span className="truncate">{m}</span>
                      {selectedModel === m && <Check size={14} className="shrink-0" />}
                    </button>
                  ))}
                </div>
              </>
            )}
          </div>

          {/* Right side actions */}
          <div className="ml-auto flex items-center gap-2">
            {selectedModel && (
              <span className="hidden sm:inline-flex rounded-full bg-primary/8 px-2.5 py-1 text-[10px] font-medium text-primary">
                {selectedModel}
              </span>
            )}
            {/* Deep Research toggle */}
            <button
              onClick={() => setDeepResearch(!deepResearch)}
              className={`flex items-center gap-1.5 rounded-lg px-2.5 py-1.5 text-xs font-medium transition-colors ${
                deepResearch
                  ? "bg-violet-50 text-violet-600 hover:bg-violet-100"
                  : "bg-slate-100 text-slate-400 hover:bg-slate-200"
              }`
              }
              title="Deep Research: 多轮迭代检索+综合分析"
            >
              <Sparkles size={12} />
              Deep Research
            </button>
            {/* Search config toggle */}
            <button
              onClick={() => setShowSearchConfig(!showSearchConfig)}
              className={`flex items-center gap-1.5 rounded-lg px-2.5 py-1.5 text-xs font-medium transition-colors ${
                showSearchConfig
                  ? "bg-blue-50 text-blue-600 hover:bg-blue-100"
                  : "bg-slate-100 text-slate-400 hover:bg-slate-200"
              }`
              }
              title="检索配置: 模式/权重/top_k"
            >
              <Sliders size={12} />
              检索配置
            </button>
            {conversation.length > 0 && (
              <button
                onClick={() => { setConversation([]); setAnswer(null); }}
                className="rounded-lg px-2.5 py-1.5 text-xs text-muted-foreground hover:bg-muted hover:text-foreground transition-colors"
              >
                清空对话
              </button>
            )}
          </div>
        </div>

        {/* Search config panel */}
        {showSearchConfig && (
          <div className="border-b border-border bg-slate-50/50 px-4 py-3 space-y-3 shrink-0">
            <div className="flex items-center gap-4 flex-wrap">
              {/* Search mode */}
              <div className="flex items-center gap-2">
                <span className="text-xs font-medium text-muted-foreground">检索模式</span>
                <div className="flex rounded-lg border border-border overflow-hidden">
                  {(["hybrid", "vector", "keyword"] as const).map((m) => (
                    <button
                      key={m}
                      onClick={() => setSearchMode(m)}
                      className={`px-2.5 py-1 text-xs transition-colors ${
                        searchMode === m
                          ? "bg-primary text-white"
                          : "bg-white text-muted-foreground hover:bg-slate-100"
                      }`}
                    >
                      {m === "hybrid" ? "混合" : m === "vector" ? "向量" : "关键词"}
                    </button>
                  ))}
                </div>
              </div>
              {/* Top K */}
              <div className="flex items-center gap-2">
                <span className="text-xs font-medium text-muted-foreground">Top K</span>
                <input
                  type="number"
                  min={1}
                  max={20}
                  value={topK}
                  onChange={(e) => setTopK(Math.max(1, Math.min(20, Number(e.target.value) || 5)))}
                  className="w-14 rounded-lg border border-border bg-white px-2 py-1 text-xs text-center outline-none focus:border-primary"
                />
              </div>
            </div>
            {/* Weights (only for hybrid) */}
            {searchMode === "hybrid" && (
              <div className="flex items-center gap-4 flex-wrap">
                <div className="flex items-center gap-2">
                  <span className="text-xs font-medium text-muted-foreground">向量权重</span>
                  <input
                    type="range"
                    min={0}
                    max={1}
                    step={0.1}
                    value={vectorWeight}
                    onChange={(e) => {
                      const v = Number(e.target.value);
                      setVectorWeight(v);
                      setKeywordWeight(Math.round((1 - v) * 10) / 10);
                    }}
                    className="w-24 accent-primary"
                  />
                  <span className="text-xs text-muted-foreground w-8">{vectorWeight.toFixed(1)}</span>
                </div>
                <div className="flex items-center gap-2">
                  <span className="text-xs font-medium text-muted-foreground">关键词权重</span>
                  <input
                    type="range"
                    min={0}
                    max={1}
                    step={0.1}
                    value={keywordWeight}
                    onChange={(e) => {
                      const v = Number(e.target.value);
                      setKeywordWeight(v);
                      setVectorWeight(Math.round((1 - v) * 10) / 10);
                    }}
                    className="w-24 accent-primary"
                  />
                  <span className="text-xs text-muted-foreground w-8">{keywordWeight.toFixed(1)}</span>
                </div>
              </div>
            )}
          </div>
        )}

        {/* Conversation area — flexible middle, scrollable */}
        <div className="flex-1 min-h-0 overflow-y-auto px-4 py-4 space-y-4">
          {conversation.length === 0 ? (
            <div className="flex flex-col items-center justify-center py-12 text-muted-foreground">
              <MessageCircle className="h-10 w-10 text-muted-foreground/30" />
              <p className="mt-3 text-sm">向知识库提问，AI 将基于检索到的内容回答</p>
              <p className="mt-1 text-xs text-muted-foreground/70">
                {kb.doc_count} 文档 · {kb.chunk_count} 切片可供检索
              </p>
            </div>
          ) : (
            conversation.map((msg, i) => (
              <div key={i} className={`flex ${msg.role === "user" ? "justify-end" : "justify-start"}`}>
                <div
                  className={`max-w-[80%] rounded-2xl px-4 py-3 text-sm ${
                    msg.role === "user"
                      ? "bg-primary text-white"
                      : "bg-muted/50 text-foreground border border-border"
                  }`}
                >
                  <p className="whitespace-pre-wrap">{msg.content}</p>
                  {msg.sources && msg.sources.length > 0 && (
                    <div className="mt-3 space-y-1.5 border-t border-border/40 pt-3">
                      <div className="text-[10px] font-medium text-muted-foreground uppercase tracking-wide">引用来源</div>
                      {msg.sources.map((s, si) => (
                        <div key={si} className="rounded-lg bg-white/80 p-2 text-xs">
                          <div className="flex items-center justify-between">
                            <span className="font-medium text-foreground">{s.filename}</span>
                            <span className="text-muted-foreground">{(s.score * 100).toFixed(1)}%</span>
                          </div>
                          <p className="mt-0.5 text-muted-foreground line-clamp-2">{s.snippet}</p>
                        </div>
                      ))}
                    </div>
                  )}
                  {msg.retrievalDetails && msg.retrievalDetails.length > 0 && (
                    <div className="mt-2 border-t border-border/40 pt-2">
                      <button
                        onClick={() => setShowRetrievalDetails(showRetrievalDetails === i ? null : i)}
                        className="flex items-center gap-1 text-[10px] font-medium text-muted-foreground hover:text-foreground transition-colors"
                      >
                        {showRetrievalDetails === i ? <ChevronUp size={10} /> : <ChevronDown size={10} />}
                        检索详情 ({msg.retrievalDetails.length})
                      </button>
                      {showRetrievalDetails === i && (
                        <div className="mt-1.5 space-y-1">
                          {msg.retrievalDetails.map((rd, rdi) => (
                            <div key={rdi} className="rounded-lg bg-white/60 p-2 text-xs border border-border/40">
                              <div className="flex items-center justify-between gap-2">
                                <div className="flex items-center gap-1.5 min-w-0">
                                  <span className="font-medium text-foreground truncate">{rd.filename}</span>
                                  {rd.symbol_name && (
                                    <span className="shrink-0 rounded bg-primary/10 px-1 py-0.5 text-[9px] text-primary">
                                      {rd.symbol_name}
                                    </span>
                                  )}
                                </div>
                                <span className="shrink-0 text-muted-foreground">{(rd.score * 100).toFixed(1)}%</span>
                              </div>
                              <div className="mt-1 flex items-center gap-3 text-[9px] text-muted-foreground">
                                {rd.vector_score != null && (
                                  <span className="text-blue-500">向量: {(rd.vector_score * 100).toFixed(1)}%</span>
                                )}
                                {rd.keyword_score != null && (
                                  <span className="text-green-500">关键词: {(rd.keyword_score * 100).toFixed(1)}%</span>
                                )}
                              </div>
                              <p className="mt-0.5 text-muted-foreground line-clamp-2 text-[10px]">{rd.snippet}</p>
                            </div>
                          ))}
                        </div>
                      )}
                    </div>
                  )}
                </div>
              </div>
            ))
          )}
          {asking && (
            <div className="flex justify-start">
              <div className="rounded-2xl bg-muted/50 border border-border px-4 py-3">
                <div className="flex items-center gap-2 text-sm text-muted-foreground">
                  <Loader2 className="h-4 w-4 animate-spin" />
                  正在检索知识库并生成回答...
                </div>
              </div>
            </div>
          )}
        </div>

        {/* Input bar — bottom fixed */}
        <div className="border-t border-border bg-background/40 rounded-b-2xl px-4 py-3 shrink-0">
          <div className="flex items-end gap-2">
            <textarea
              value={question}
              onChange={(e) => setQuestion(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter" && !e.shiftKey && !e.nativeEvent.isComposing) {
                  e.preventDefault();
                  handleAsk();
                }
              }}
              placeholder="输入问题，Enter 发送，Shift+Enter 换行..."
              rows={1}
              className="flex-1 resize-none rounded-2xl border border-border bg-white px-3.5 py-2.5 text-sm outline-none focus:border-primary focus:ring-2 focus:ring-primary/20 max-h-32"
              style={{ minHeight: "42px" }}
              disabled={asking}
            />
            <button
              onClick={handleAsk}
              disabled={asking || !question.trim()}
              className="action-primary disabled:opacity-50 shrink-0"
            >
              {asking ? <Loader2 className="h-4 w-4 animate-spin" /> : <MessageCircle size={16} />}
              发送
            </button>
          </div>
          {/* Token usage */}
          {answer?.usage && (
            <div className="mt-2 flex items-center gap-3 text-[10px] text-muted-foreground">
              <span>Prompt: {answer.usage.prompt_tokens}</span>
              <span>Completion: {answer.usage.completion_tokens}</span>
              <span>Total: {answer.usage.total_tokens}</span>
            </div>
          )}
        </div>
    </div>
  );
}

// ─── Settings Tab ───────────────────────────────────────────────────────

function SettingsTab({ kb, onRefresh }: { kb: KnowledgeBase; onRefresh: () => void }) {
  const [channels, setChannels] = useState<Channel[]>([]);
  const [name, setName] = useState(kb.name);
  const [description, setDescription] = useState(kb.description || "");
  const [embeddingModel, setEmbeddingModel] = useState(kb.embedding_model || "text-embedding-3-small");
  const [embeddingChannelId, setEmbeddingChannelId] = useState(kb.embedding_channel_id || "");
  const [status, setStatus] = useState(kb.status);
  const [mcpEnabled, setMcpEnabled] = useState(kb.mcp_enabled ?? 1);
  const [chunkSize, setChunkSize] = useState(kb.chunk_size || 512);
  const [chunkOverlap, setChunkOverlap] = useState(kb.chunk_overlap || 64);
  const [embeddingBatchSize, setEmbeddingBatchSize] = useState(kb.embedding_batch_size || 32);
  const [excludedDirs, setExcludedDirs] = useState(kb.excluded_dirs || "");
  const [excludedFiles, setExcludedFiles] = useState(kb.excluded_files || "");
  const [includedFiles, setIncludedFiles] = useState(kb.included_files || "");
  const [saving, setSaving] = useState(false);
  const [saved, setSaved] = useState(false);
  const [showChannelPicker, setShowChannelPicker] = useState(false);

  useEffect(() => {
    channelApi.getAll().then(setChannels).catch(console.error);
  }, []);

  const activeChannels = channels.filter(c => c.status === 1);
  const selectedEmbeddingChannel = activeChannels.find(c => c.id === embeddingChannelId);

  const handleSave = async () => {
    setSaving(true);
    setSaved(false);
    try {
      await kbApi.update(kb.id, {
        name: name.trim(),
        description: description.trim() || undefined,
        embedding_model: embeddingModel.trim() || undefined,
        embedding_channel_id: embeddingChannelId || undefined,
        status,
        mcp_enabled: mcpEnabled,
        chunk_size: chunkSize,
        chunk_overlap: chunkOverlap,
        embedding_batch_size: embeddingBatchSize,
        excluded_dirs: excludedDirs,
        excluded_files: excludedFiles,
        included_files: includedFiles,
      });
      setSaved(true);
      onRefresh();
      setTimeout(() => setSaved(false), 2000);
    } catch (e) {
      alert(`保存失败: ${e}`);
    } finally {
      setSaving(false);
    }
  };

  return (
    <div className="grid gap-4 lg:grid-cols-2">
      {/* Basic */}
      <div className="surface data-card rounded-2xl">
        <h3 className="mb-4 text-sm font-semibold text-slate-900">基本信息</h3>
        <div className="space-y-4">
          <div>
            <label className="mb-1 block text-sm font-medium text-slate-700">名称</label>
            <input
              type="text"
              value={name}
              onChange={(e) => setName(e.target.value)}
              className="w-full rounded-xl border border-slate-200 px-3 py-2 text-sm outline-none focus:border-blue-400 focus:ring-2 focus:ring-blue-100"
            />
          </div>
          <div>
            <label className="mb-1 block text-sm font-medium text-slate-700">描述</label>
            <textarea
              value={description}
              onChange={(e) => setDescription(e.target.value)}
              rows={2}
              className="w-full rounded-xl border border-slate-200 px-3 py-2 text-sm outline-none focus:border-blue-400 focus:ring-2 focus:ring-blue-100"
            />
          </div>
          <div className="flex items-center gap-3">
            <label className="flex items-center gap-2 cursor-pointer">
              <input
                type="checkbox"
                checked={status === 1}
                onChange={(e) => setStatus(e.target.checked ? 1 : 0)}
                className="rounded"
              />
              <span className="text-sm text-slate-700">启用知识库</span>
            </label>
            <label className="flex items-center gap-2 cursor-pointer ml-4">
              <input
                type="checkbox"
                checked={mcpEnabled === 1}
                onChange={(e) => setMcpEnabled(e.target.checked ? 1 : 0)}
                className="rounded"
              />
              <span className="text-sm text-slate-700">MCP 暴露</span>
            </label>
          </div>
          <p className="text-xs text-slate-400">
            关闭 MCP 暴露后，该知识库不会出现在 MCP 工具的列表中，也不会被全局搜索命中。仍可通过显式指定 kb_id 访问。
          </p>
        </div>
      </div>

      {/* Embedding config */}
      <div className="surface data-card rounded-2xl">
        <h3 className="mb-4 text-sm font-semibold text-slate-900">Embedding 配置</h3>
        <div className="space-y-4">
          <div>
            <label className="mb-1 block text-sm font-medium text-slate-700">Embedding 模型</label>
            <input
              type="text"
              value={embeddingModel}
              onChange={(e) => setEmbeddingModel(e.target.value)}
              placeholder="text-embedding-3-small"
              className="w-full rounded-xl border border-slate-200 px-3 py-2 text-sm outline-none focus:border-blue-400 focus:ring-2 focus:ring-blue-100"
            />
            <p className="mt-1 text-xs text-slate-400">
              支持的模型取决于渠道，常见：text-embedding-3-small / text-embedding-3-large / text-embedding-ada-002
            </p>
          </div>

          <div>
            <label className="mb-1 block text-sm font-medium text-slate-700">绑定渠道（可选）</label>
            <div className="relative">
              <button
                type="button"
                onClick={() => setShowChannelPicker(!showChannelPicker)}
                className="flex w-full items-center justify-between rounded-xl border border-slate-200 bg-white px-3 py-2 text-sm outline-none focus:border-blue-400 focus:ring-2 focus:ring-blue-100"
              >
                <span className={selectedEmbeddingChannel ? "text-slate-900" : "text-slate-400"}>
                  {selectedEmbeddingChannel
                    ? `${selectedEmbeddingChannel.name} (${selectedEmbeddingChannel.type})`
                    : "自动选择（默认）"}
                </span>
                <ChevronDown size={15} className={`shrink-0 text-slate-400 transition-transform ${showChannelPicker ? "rotate-180" : ""}`} />
              </button>

              {showChannelPicker && (
                <>
                  <div className="fixed inset-0 z-40" onClick={() => setShowChannelPicker(false)} />
                  <div className="absolute left-0 top-full z-50 mt-1.5 w-full rounded-2xl border border-slate-200 bg-white p-2 shadow-xl max-h-[280px] overflow-auto">
                    <button
                      type="button"
                      onClick={() => {
                        setEmbeddingChannelId("");
                        setShowChannelPicker(false);
                      }}
                      className={`flex w-full items-center justify-between rounded-xl px-3 py-2.5 text-sm transition-all ${
                        embeddingChannelId === ""
                          ? "bg-blue-50 text-blue-600 font-semibold"
                          : "text-slate-700 hover:bg-slate-50"
                      }`}
                    >
                      <span>自动选择（默认）</span>
                      {embeddingChannelId === "" && <Check size={14} className="shrink-0" />}
                    </button>
                    {activeChannels.map((c) => (
                      <button
                        key={c.id}
                        type="button"
                        onClick={() => {
                          setEmbeddingChannelId(c.id);
                          setShowChannelPicker(false);
                        }}
                        className={`flex w-full items-center justify-between rounded-xl px-3 py-2.5 text-sm transition-all ${
                          embeddingChannelId === c.id
                            ? "bg-blue-50 text-blue-600 font-semibold"
                            : "text-slate-700 hover:bg-slate-50"
                        }`}
                      >
                        <div className="flex items-center gap-2 min-w-0">
                          <span className="truncate">{c.name}</span>
                          <span className="rounded-full bg-slate-100 px-1.5 py-0.5 text-[10px] text-slate-500 shrink-0">
                            {c.type}
                          </span>
                        </div>
                        {embeddingChannelId === c.id && <Check size={14} className="shrink-0" />}
                      </button>
                    ))}
                  </div>
                </>
              )}
            </div>
            <p className="mt-1 text-xs text-slate-400">
              指定后，embedding 请求会优先使用该渠道。不指定则自动调度。
            </p>
          </div>
        </div>
      </div>

      {/* Chunking & Filtering config */}
      <div className="surface data-card rounded-2xl">
        <h3 className="mb-4 text-sm font-semibold text-slate-900">分块与过滤</h3>
        <div className="space-y-4">
          <div className="grid grid-cols-2 gap-3">
            <div>
              <label className="mb-1 block text-sm font-medium text-slate-700">分块大小 (tokens)</label>
              <input
                type="number"
                value={chunkSize}
                onChange={(e) => setChunkSize(Number(e.target.value) || 512)}
                min={50}
                max={2000}
                step={50}
                className="w-full rounded-xl border border-slate-200 px-3 py-2 text-sm outline-none focus:border-blue-400 focus:ring-2 focus:ring-blue-100"
              />
              <p className="mt-1 text-xs text-slate-400">默认 512，越大上下文越完整但消耗更多 token</p>
            </div>
            <div>
              <label className="mb-1 block text-sm font-medium text-slate-700">分块重叠 (tokens)</label>
              <input
                type="number"
                value={chunkOverlap}
                onChange={(e) => setChunkOverlap(Number(e.target.value) || 64)}
                min={0}
                max={500}
                step={16}
                className="w-full rounded-xl border border-slate-200 px-3 py-2 text-sm outline-none focus:border-blue-400 focus:ring-2 focus:ring-blue-100"
              />
              <p className="mt-1 text-xs text-slate-400">默认 64，保持上下文连续性</p>
            </div>
          </div>
          <div className="grid grid-cols-2 gap-3">
            <div>
              <label className="mb-1 block text-sm font-medium text-slate-700">Embedding 批次大小</label>
              <input
                type="number"
                value={embeddingBatchSize}
                onChange={(e) => setEmbeddingBatchSize(Number(e.target.value) || 32)}
                min={1}
                max={100}
                step={1}
                className="w-full rounded-xl border border-slate-200 px-3 py-2 text-sm outline-none focus:border-blue-400 focus:ring-2 focus:ring-blue-100"
              />
              <p className="mt-1 text-xs text-slate-400">默认 32，单次 API 调用处理的最大 chunk 数量</p>
            </div>
          </div>
          <div>
            <label className="mb-1 block text-sm font-medium text-slate-700">排除目录（逗号分隔）</label>
            <input
              type="text"
              value={excludedDirs}
              onChange={(e) => setExcludedDirs(e.target.value)}
              placeholder="tests, examples, vendor"
              className="w-full rounded-xl border border-slate-200 px-3 py-2 text-sm outline-none focus:border-blue-400 focus:ring-2 focus:ring-blue-100"
            />
            <p className="mt-1 text-xs text-slate-400">导入 Git/本地目录时跳过这些目录（默认排除 .git, node_modules 等）</p>
          </div>
          <div className="grid grid-cols-2 gap-3">
            <div>
              <label className="mb-1 block text-sm font-medium text-slate-700">排除文件（逗号分隔）</label>
              <input
                type="text"
                value={excludedFiles}
                onChange={(e) => setExcludedFiles(e.target.value)}
                placeholder="*.lock, *.min.js"
                className="w-full rounded-xl border border-slate-200 px-3 py-2 text-sm outline-none focus:border-blue-400 focus:ring-2 focus:ring-blue-100"
              />
            </div>
            <div>
              <label className="mb-1 block text-sm font-medium text-slate-700">包含文件类型（逗号分隔，空=全部）</label>
              <input
                type="text"
                value={includedFiles}
                onChange={(e) => setIncludedFiles(e.target.value)}
                placeholder="md, rs, ts, py"
                className="w-full rounded-xl border border-slate-200 px-3 py-2 text-sm outline-none focus:border-blue-400 focus:ring-2 focus:ring-blue-100"
              />
            </div>
          </div>
        </div>
      </div>

      {/* Stats */}
      <div className="surface data-card rounded-2xl">
        <h3 className="mb-4 text-sm font-semibold text-slate-900">统计</h3>
        <div className="grid grid-cols-3 gap-4">
          <div className="rounded-xl bg-slate-50 p-3 text-center">
            <div className="text-2xl font-bold text-slate-900">{kb.doc_count}</div>
            <div className="text-xs text-slate-500">文档数</div>
          </div>
          <div className="rounded-xl bg-slate-50 p-3 text-center">
            <div className="text-2xl font-bold text-slate-900">{kb.chunk_count}</div>
            <div className="text-xs text-slate-500">切片数</div>
          </div>
          <div className="rounded-xl bg-slate-50 p-3 text-center">
            <div className="text-2xl font-bold text-slate-900">{kb.total_tokens}</div>
            <div className="text-xs text-slate-500">总 Tokens</div>
          </div>
        </div>
      </div>

      {/* Save */}
      <div className="surface data-card rounded-2xl flex items-center justify-end gap-3">
        <button
          onClick={handleSave}
          disabled={saving}
          className="action-primary disabled:opacity-50"
        >
          {saving ? <Loader2 className="h-4 w-4 animate-spin" /> : <SettingsIcon size={16} />}
          保存设置
        </button>
        {saved && (
          <span className="flex items-center gap-1 text-sm text-emerald-600">
            <CheckCircle2 size={16} /> 已保存
          </span>
        )}
      </div>
    </div>
  );
}

// ─── MCP Tab (per-KB) ───────────────────────────────────────────────────

function McpTab({ kb }: { kb: KnowledgeBase }) {
  const [serverUrl, setServerUrl] = useState("http://127.0.0.1:8777");
  const [copied, setCopied] = useState(false);

  useEffect(() => {
    serverApi.getStatus().then(s => {
      if (s.running) setServerUrl(`http://127.0.0.1:${s.port}`);
    }).catch(() => {});
  }, []);

  const baseUrl = serverUrl;
  const mcpEndpoint = `${baseUrl}/mcp`;

  const handleCopy = (text: string) => {
    navigator.clipboard.writeText(text);
    setCopied(true);
    setTimeout(() => setCopied(false), 2000);
  };

  const mcpTools = [
    { name: "search_knowledge_base", desc: "语义检索知识库，返回匹配文本片段和相似度评分", required: ["query"] },
    { name: "list_knowledge_bases", desc: "列出所有已暴露的知识库（ID/名称/文档数）", required: [] },
    { name: "ask_knowledge_base", desc: "RAG 问答，基于检索内容生成回答并返回来源引用", required: ["question"] },
    { name: "read_document", desc: "读取指定文档的完整内容", required: ["kb_id", "doc_id"] },
    { name: "get_knowledge_base_stats", desc: "获取知识库统计信息（文档数/切片数/token数）", required: ["kb_id"] },
  ];

  return (
    <div className="space-y-4">
      {/* 接入说明 */}
      <div className="surface data-card rounded-2xl">
        <div className="mb-4 flex items-center gap-2">
          <Terminal size={18} className="text-slate-700" />
          <h3 className="text-sm font-semibold text-slate-900">MCP 对接</h3>
          {kb.mcp_enabled === 1 ? (
            <span className="ml-auto rounded-full bg-emerald-50 px-2 py-0.5 text-xs font-medium text-emerald-600">已暴露</span>
          ) : (
            <span className="ml-auto rounded-full bg-slate-100 px-2 py-0.5 text-xs font-medium text-slate-500">未暴露</span>
          )}
        </div>

        <div className="space-y-3">
          {/* 端点地址 */}
          <div>
            <label className="mb-1 block text-xs font-medium text-slate-500">MCP 端点（JSON-RPC over HTTP）</label>
            <div className="flex items-center gap-2">
              <code className="flex-1 truncate rounded-lg bg-slate-50 border border-slate-200 px-3 py-2 text-xs font-mono text-slate-800">{mcpEndpoint}</code>
              <button onClick={() => handleCopy(mcpEndpoint)} className="rounded-lg border border-slate-200 p-2 hover:bg-slate-50">
                {copied ? <CheckCircle2 size={14} className="text-emerald-500" /> : <Copy size={14} className="text-slate-400" />}
              </button>
            </div>
          </div>

          {/* 协议说明 */}
          <div className="rounded-lg bg-blue-50 border border-blue-100 px-3 py-2.5 text-xs text-blue-700">
            <div className="font-medium mb-1">📡 MCP (Model Context Protocol) 对接</div>
            <div className="text-blue-600">
              其他 AI Agent / 工具可通过 MCP 协议接入此知识库。将上方端点配置到支持 MCP 的客户端（如 Claude Desktop、Cursor、自定义 Agent），即可让 AI 自动检索和问答你的私有知识库。
            </div>
          </div>

          {/* 未暴露提示 */}
          {kb.mcp_enabled !== 1 && (
            <div className="rounded-lg bg-amber-50 border border-amber-100 px-3 py-2 text-xs text-amber-700">
              ⚠️ 该知识库未开启 MCP 暴露。外部 Agent 无法检索到此知识库。请在「设置」中开启「MCP 暴露」。
            </div>
          )}
        </div>
      </div>

      {/* 可用工具列表 */}
      <div className="surface data-card rounded-2xl">
        <div className="mb-4 flex items-center gap-2">
          <Layers size={18} className="text-slate-700" />
          <h3 className="text-sm font-semibold text-slate-900">可用 MCP 工具</h3>
          <span className="ml-auto text-xs text-slate-400">{mcpTools.length} 个工具</span>
        </div>
        <div className="space-y-2">
          {mcpTools.map((tool) => (
            <div key={tool.name} className="flex items-start gap-3 rounded-xl bg-slate-50 px-3 py-2.5">
              <code className="shrink-0 rounded bg-slate-200 px-1.5 py-0.5 text-[11px] font-mono font-medium text-slate-700">{tool.name}</code>
              <div className="min-w-0">
                <p className="text-xs text-slate-600">{tool.desc}</p>
                {tool.required.length > 0 && (
                  <p className="mt-0.5 text-[10px] text-slate-400">
                    必填: {tool.required.join(", ")}
                  </p>
                )}
              </div>
            </div>
          ))}
        </div>
      </div>

      {/* 调用示例 */}
      <div className="surface data-card rounded-2xl">
        <div className="mb-4 flex items-center gap-2">
          <Server size={18} className="text-slate-700" />
          <h3 className="text-sm font-semibold text-slate-900">调用示例</h3>
        </div>

        <div className="space-y-3">
          <div>
            <label className="mb-1 block text-xs font-medium text-slate-500">1. 列出可用知识库</label>
            <pre className="overflow-x-auto rounded-xl bg-slate-50 border border-slate-200 p-3 text-[11px]"><code className="text-slate-800">{`curl -X POST ${mcpEndpoint} \\
  -H "Content-Type: application/json" \\
  -d '{
    "jsonrpc": "2.0",
    "id": 1,
    "method": "tools/call",
    "params": {
      "name": "list_knowledge_bases",
      "arguments": {}
    }
  }'`}</code></pre>
          </div>

          <div>
            <label className="mb-1 block text-xs font-medium text-slate-500">2. 语义检索</label>
            <pre className="overflow-x-auto rounded-xl bg-slate-50 border border-slate-200 p-3 text-[11px]"><code className="text-slate-800">{`curl -X POST ${mcpEndpoint} \\
  -H "Content-Type: application/json" \\
  -d '{
    "jsonrpc": "2.0",
    "id": 2,
    "method": "tools/call",
    "params": {
      "name": "search_knowledge_base",
      "arguments": {
        "query": "你的检索内容",
        "kb_id": "${kb.id}",
        "top_k": 5
      }
    }
  }'`}</code></pre>
          </div>

          <div>
            <label className="mb-1 block text-xs font-medium text-slate-500">3. RAG 问答</label>
            <pre className="overflow-x-auto rounded-xl bg-slate-50 border border-slate-200 p-3 text-[11px]"><code className="text-slate-800">{`curl -X POST ${mcpEndpoint} \\
  -H "Content-Type: application/json" \\
  -d '{
    "jsonrpc": "2.0",
    "id": 3,
    "method": "tools/call",
    "params": {
      "name": "ask_knowledge_base",
      "arguments": {
        "question": "你的问题",
        "kb_id": "${kb.id}"
      }
    }
  }'`}</code></pre>
          </div>
        </div>

        <div className="mt-3 rounded-lg bg-slate-50 border border-slate-200 px-3 py-2 text-xs text-slate-500">
          ℹ️ 仅接受 POST 请求。所有工具遵循 MCP JSON-RPC 2.0 规范。
        </div>
      </div>
    </div>
  );
}

// ─── Create KB Modal ────────────────────────────────────────────────────

function CreateKbModal({
  onClose,
  onCreated,
}: {
  onClose: () => void;
  onCreated: () => void;
}) {
  const [name, setName] = useState("");
  const [description, setDescription] = useState("");
  const [embeddingModel, setEmbeddingModel] = useState("text-embedding-3-small");
  const [creating, setCreating] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const handleCreate = async () => {
    if (!name.trim()) {
      setError("请输入知识库名称");
      return;
    }
    setCreating(true);
    setError(null);
    try {
      await kbApi.create({
        name: name.trim(),
        description: description.trim() || undefined,
        embedding_model: embeddingModel || undefined,
      });
      onCreated();
    } catch (e) {
      setError(String(e));
    } finally {
      setCreating(false);
    }
  };

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/30" onClick={onClose}>
      <div
        className="w-full max-w-md rounded-2xl bg-white p-6 shadow-xl"
        onClick={(e) => e.stopPropagation()}
      >
        <h3 className="text-lg font-semibold text-slate-900">新建知识库</h3>

        <div className="mt-4 space-y-4">
          <div>
            <label className="mb-1 block text-sm font-medium text-slate-700">名称</label>
            <input
              type="text"
              value={name}
              onChange={(e) => setName(e.target.value)}
              placeholder="例如：项目文档库"
              className="w-full rounded-xl border border-slate-200 px-3 py-2 text-sm outline-none focus:border-blue-400 focus:ring-2 focus:ring-blue-100"
            />
          </div>

          <div>
            <label className="mb-1 block text-sm font-medium text-slate-700">描述（可选）</label>
            <textarea
              value={description}
              onChange={(e) => setDescription(e.target.value)}
              placeholder="知识库用途描述..."
              rows={2}
              className="w-full rounded-xl border border-slate-200 px-3 py-2 text-sm outline-none focus:border-blue-400 focus:ring-2 focus:ring-blue-100"
            />
          </div>

          <div>
            <label className="mb-1 block text-sm font-medium text-slate-700">Embedding 模型</label>
            <input
              type="text"
              value={embeddingModel}
              onChange={(e) => setEmbeddingModel(e.target.value)}
              placeholder="text-embedding-3-small"
              className="w-full rounded-xl border border-slate-200 px-3 py-2 text-sm outline-none focus:border-blue-400 focus:ring-2 focus:ring-blue-100"
            />
            <p className="mt-1 text-xs text-slate-400">
              复用已有渠道的 Embedding 模型，确保渠道支持该模型
            </p>
          </div>

          {error && (
            <div className="rounded-lg bg-red-50 p-3 text-sm text-red-600">{error}</div>
          )}
        </div>

        <div className="mt-6 flex justify-end gap-2">
          <button
            onClick={onClose}
            className="rounded-xl px-4 py-2 text-sm text-slate-500 hover:bg-slate-100"
          >
            取消
          </button>
          <button
            onClick={handleCreate}
            disabled={creating}
            className="action-primary disabled:opacity-50"
          >
            {creating ? "创建中..." : "创建"}
          </button>
        </div>
      </div>
    </div>
  );
}

// ─── Helpers ────────────────────────────────────────────────────────────

function DocStatusIcon({ status }: { status: string }) {
  switch (status) {
    case "ready":
      return <CheckCircle2 className="h-5 w-5 shrink-0 text-emerald-500" />;
    case "processing":
      return <Loader2 className="h-5 w-5 shrink-0 animate-spin text-blue-500" />;
    case "failed":
      return <XCircle className="h-5 w-5 shrink-0 text-red-500" />;
    case "pending":
      return <Clock className="h-5 w-5 shrink-0 text-slate-400" />;
    default:
      return <FileText className="h-5 w-5 shrink-0 text-slate-400" />;
  }
}

function formatSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
}

function fileToBase64(file: File): Promise<string> {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onload = () => {
      const result = reader.result as string;
      const base64 = result.split(",")[1] || result;
      resolve(base64);
    };
    reader.onerror = reject;
    reader.readAsDataURL(file);
  });
}
