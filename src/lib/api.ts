import { invoke } from "@tauri-apps/api/core";
import type {
  Channel, CreateChannelInput, UpdateChannelInput, TestChannelResult,
  ApiKey, CreateApiKeyInput, ApiKeyStats,
  RequestLog, LogStats, SecurityFinding,
  DashboardStats,
  Settings,
  ServerStatus,
  BuiltinRule, CustomRule, CreateCustomRuleInput, UpdateBuiltinRuleInput,
} from "../types";

// Channel stats
export interface ChannelStats {
  channel_id: string;
  total_calls: number;
  success_calls: number;
  failed_calls: number;
  total_tokens: number;
  prompt_tokens: number;
  completion_tokens: number;
  avg_latency_ms: number;
  last_call_at: string | null;
}

// Channel commands
export const channelApi = {
  getAll: () => invoke<Channel[]>("get_channels"),
  get: (id: string) => invoke<Channel>("get_channel", { id }),
  getApiKey: (id: string) => invoke<string>("get_channel_api_key", { id }),
  create: (input: CreateChannelInput) => invoke<Channel>("create_channel", { input }),
  update: (input: UpdateChannelInput) => invoke<Channel>("update_channel", { input }),
  toggle: (id: string, status: number) => invoke<void>("toggle_channel", { id, status }),
  delete: (id: string) => invoke<void>("delete_channel", { id }),
  test: (id: string) => invoke<TestChannelResult>("test_channel", { id }),
  getStats: () => invoke<ChannelStats[]>("get_channel_stats"),
  reorder: (orderedIds: string[]) => invoke<void>("reorder_channels", { orderedIds }),
};

// API Key commands
export const apiKeyApi = {
  getAll: () => invoke<ApiKey[]>("get_api_keys"),
  create: (input: CreateApiKeyInput) => invoke<ApiKey>("create_api_key", { input }),
  update: (id: string, status?: number) => invoke<void>("update_api_key", { input: { id, status } }),
  delete: (id: string) => invoke<void>("delete_api_key", { id }),
  getStats: () => invoke<ApiKeyStats[]>("get_api_key_stats"),
};

export interface GetLogsInput {
  limit?: number;
  offset?: number;
  keyword?: string;
  api_key_name?: string;
  channel_name?: string;
  model?: string;
  date_from?: string;
  date_to?: string;
  trace_id?: string;
}

// Log commands
export const logApi = {
  getAll: (input?: GetLogsInput) => invoke<RequestLog[]>("get_logs", { input: input || {} }),
  get: (id: string) => invoke<RequestLog>("get_log", { id }),
  getSecurityFindings: (logId: string) => invoke<SecurityFinding[]>("get_log_security_findings", { logId }),
  getStats: (days?: number) => invoke<LogStats[]>("get_log_stats", { days }),
  delete: (id: string) => invoke<void>("delete_log", { id }),
  deleteBefore: (beforeDate: string) => invoke<number>("delete_logs_before", { beforeDate }),
  deleteAll: () => invoke<number>("delete_all_logs"),
};

// Stats commands
export const statsApi = {
  getDashboard: () => invoke<DashboardStats>("get_dashboard_stats"),
};

// Settings commands
export const settingsApi = {
  get: () => invoke<Settings>("get_settings"),
  save: (settings: Settings) => invoke<void>("save_settings", { settings }),
  applyTheme: (theme: string) => invoke<void>("apply_theme", { theme }),
  setAutoStart: (enabled: boolean) => invoke<void>("set_auto_start", { enabled }),
};

// Server commands
export const serverApi = {
  getStatus: () => invoke<ServerStatus>("get_server_status"),
  restart: () => invoke<void>("restart_server"),
};

// Import / Export
export interface ImportResult {
  imported: number;
  skipped: number;
  errors: string[];
}

export interface ScannedSource {
  source: string;
  name: string;
  base_url: string;
  api_key: string;
  models: string[];
  api_format: string;
  raw: Record<string, unknown>;
}

export interface ScanResult {
  sources: ScannedSource[];
}

export const importExportApi = {
  exportChannels: () => invoke<string>("export_channels"),
  importWalicodeBackup: (content: string) => invoke<ImportResult>("import_walicode_backup", { content }),
  importWaliapiExport: (content: string) => invoke<ImportResult>("import_waliapi_export", { content }),
  scanLocalAiConfigs: () => invoke<ScanResult>("scan_local_ai_configs"),
  importScannedSources: (sources: ScannedSource[]) => invoke<ImportResult>("import_scanned_sources", { sources }),
  pickImportFile: () => invoke<string | null>("pick_import_file"),
  saveExportFile: (content: string, defaultName: string) => invoke<boolean>("save_export_file", { content, defaultName }),
};

// Security rules
export const securityApi = {
  getBuiltinRules: () => invoke<BuiltinRule[]>("get_builtin_security_rules"),
  updateBuiltinRule: (id: string, input: UpdateBuiltinRuleInput) => invoke<void>("update_builtin_security_rule", { id, input }),
  deleteBuiltinRule: (id: string) => invoke<void>("delete_builtin_security_rule", { id }),
  resetBuiltinRules: () => invoke<BuiltinRule[]>("reset_builtin_security_rules"),
  getCustomRules: () => invoke<CustomRule[]>("get_custom_security_rules"),
  createCustomRule: (input: CreateCustomRuleInput) => invoke<CustomRule>("create_custom_security_rule", { input }),
  toggleCustomRule: (id: string, enabled: boolean) => invoke<void>("toggle_custom_security_rule", { id, enabled }),
  deleteCustomRule: (id: string) => invoke<void>("delete_custom_security_rule", { id }),
};

// Knowledge Base types
export interface KnowledgeBase {
  id: string;
  name: string;
  description: string | null;
  status: number;
  doc_count: number;
  chunk_count: number;
  total_tokens: number;
  embedding_model: string | null;
  embedding_channel_id: string | null;
  mcp_enabled: number;
  chunk_size: number;
  chunk_overlap: number;
  excluded_dirs: string;
  excluded_files: string;
  included_files: string;
  embedding_dim: number;
  index_status: string;
  created_at: string;
  updated_at: string;
}

export interface KbDocument {
  id: string;
  kb_id: string;
  filename: string;
  file_path: string | null;
  file_type: string;
  file_size: number;
  content_hash: string;
  chunk_count: number;
  token_count: number;
  status: string;
  error_message: string | null;
  source_type: string;
  source_url: string | null;
  source_path: string | null;
  doc_meta: string;
  created_at: string;
  updated_at: string;
}

export interface KbConversation {
  id: string;
  kb_id: string;
  role: string;
  content: string;
  sources: string | null;
  model: string | null;
  tokens_used: number;
  created_at: string;
}

export interface KbSource {
  id: string;
  kb_id: string;
  source_type: string;
  source_url: string | null;
  source_path: string | null;
  branch: string | null;
  status: string;
  file_count: number;
  error: string | null;
  created_at: string;
  updated_at: string;
}

export interface KbIndexMeta {
  kb_id: string;
  index_type: string;
  embedding_dim: number;
  chunk_count: number;
  index_path: string | null;
  built_at: string | null;
  status: string;
}

export interface ConversationMessage {
  role: string;
  content: string;
}

export interface KbSearchResult {
  chunk_id: string;
  doc_id: string;
  filename: string;
  content: string;
  score: number;
  metadata: Record<string, unknown>;
}

export interface KbRetrievalDetail {
  chunk_id: string;
  filename: string;
  score: number;
  vector_score: number | null;
  keyword_score: number | null;
  snippet: string;
  symbol_name: string | null;
  symbol_kind: string | null;
}

export interface KbRagAnswer {
  answer: string;
  sources: Array<{
    filename: string;
    score: number;
    snippet: string;
  }>;
  usage: { prompt_tokens: number; completion_tokens: number; total_tokens: number } | null;
  retrieval_details: KbRetrievalDetail[] | null;
}

export interface KbTag {
  word: string;
  count: number;
}

// Knowledge Base commands
export const kbApi = {
  getAll: () => invoke<KnowledgeBase[]>("get_knowledge_bases"),
  create: (input: { name: string; description?: string; embedding_model?: string }) =>
    invoke<KnowledgeBase>("create_knowledge_base", { input }),
  update: (id: string, input: Partial<{ name: string; description: string; embedding_model: string; embedding_channel_id: string; status: number; mcp_enabled: number; chunk_size: number; chunk_overlap: number; excluded_dirs: string; excluded_files: string; included_files: string }>) =>
    invoke<KnowledgeBase>("update_knowledge_base", { id, input }),
  delete: (id: string) => invoke<void>("delete_knowledge_base", { id }),
  getDocuments: (kbId: string) => invoke<KbDocument[]>("get_kb_documents", { kbId }),
  uploadDocument: (input: { kb_id: string; filename: string; content: string }) =>
    invoke<KbDocument>("upload_kb_document", { input }),
  deleteDocument: (docId: string, kbId: string) =>
    invoke<void>("delete_kb_document", { docId, kbId }),
  reindexDocument: (docId: string) =>
    invoke<void>("reindex_kb_document", { docId }),
  search: (input: { query: string; kb_id?: string; top_k?: number; vector_weight?: number; keyword_weight?: number; search_mode?: string }) =>
    invoke<KbSearchResult[]>("search_knowledge_base", { input }),
  ask: (input: { question: string; kb_id?: string; top_k?: number; model?: string; history?: ConversationMessage[]; deep_research?: boolean; max_rounds?: number; vector_weight?: number; keyword_weight?: number; search_mode?: string }) =>
    invoke<KbRagAnswer>("ask_knowledge_base", { input }),
  getStats: (kbId: string) => invoke<Record<string, unknown>>("get_kb_stats", { kbId }),
  // Conversation history
  getConversations: (kbId: string) => invoke<KbConversation[]>("get_kb_conversations", { kbId }),
  clearConversations: (kbId: string) => invoke<void>("clear_kb_conversations", { kbId }),
  // Sources (multi-source import)
  getSources: (kbId: string) => invoke<KbSource[]>("get_kb_sources", { kbId }),
  deleteSource: (sourceId: string, kbId: string) => invoke<void>("delete_kb_source", { sourceId, kbId }),
  importSource: (kbId: string, input: { source_type: string; repo_url?: string; branch?: string; token?: string; url?: string; dir_path?: string; excluded_dirs?: string[]; included_files?: string[]; max_file_size?: number }) =>
    invoke<KbSource>("import_kb_source", { kbId, input }),
  // Index management
  getIndexStatus: (kbId: string) => invoke<KbIndexMeta | null>("get_kb_index_status", { kbId }),
  buildIndex: (kbId: string) => invoke<void>("build_kb_index", { kbId }),
  dropIndex: (kbId: string) => invoke<void>("drop_kb_index", { kbId }),
  getTags: (kbId: string, limit?: number) => invoke<KbTag[]>("get_kb_tags", { kbId, limit }),
};

// Service status
export interface ServiceStatus {
  id: string;
  name: string;
  description: string;
  enabled: boolean;
  running: boolean;
  stats: Record<string, unknown>;
}

export const serviceApi = {
  getStatuses: () => invoke<ServiceStatus[]>("get_service_statuses"),
};

// ── App Config (应用配置) ──
export interface AppInfo {
  name: string;
  label: string;
  icon: string;
  description: string;
  config_path: string;
  config_format: string;
  available: boolean;
  applied: boolean;
}

export interface ApplyResult {
  success: boolean;
  message: string;
}

export interface ConfigContent {
  exists: boolean;
  content: string;
  error: string | null;
}

export const appConfigApi = {
  getApps: () => invoke<AppInfo[]>("get_app_configs"),
  apply: (appName: string, apiKey: string, model: string) => invoke<ApplyResult>("apply_app_config", { appName, apiKey, model }),
  clear: (appName: string) => invoke<ApplyResult>("clear_app_config", { appName }),
  getContent: (appName: string) => invoke<ConfigContent>("get_app_config_content", { appName }),
  openFolder: (appName: string) => invoke<void>("open_config_folder", { appName }),
};
