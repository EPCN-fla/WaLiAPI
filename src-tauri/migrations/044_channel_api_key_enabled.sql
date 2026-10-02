-- 044: 主 Key 启用开关
--
-- 渠道主 Key（channels.api_key，负载均衡池中的 #1）支持启用/停用：
--   1 = 参与调度（默认，历史行回填为启用，向后兼容）
--   0 = 停用，不参与负载均衡；若渠道全部 Key 均停用则该渠道整体跳过。
-- 额外 Keys（channel_api_keys.status，迁移 023）已有独立开关。
--
-- 注意：不放入 config JSON——015 的 trg_channels_legacy_invalidate_identity
-- 触发器会在 UPDATE OF config 时清空身份列，窄列更新可完全绕开身份重建。

ALTER TABLE channels ADD COLUMN api_key_enabled INTEGER NOT NULL DEFAULT 1;
