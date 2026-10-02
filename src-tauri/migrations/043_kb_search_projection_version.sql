-- 投影升级不改正文/哈希/向量；旧投影由 Rust 按当前 KB 分批 CAS 重建，可中断续跑。
ALTER TABLE kb_chunks ADD COLUMN search_text_version INTEGER NOT NULL DEFAULT 0;
CREATE INDEX idx_kb_chunks_search_version_pending
    ON kb_chunks(kb_id, id) WHERE search_text IS NULL OR search_text_version < 2;

DROP TRIGGER kb_chunks_search_dirty;
CREATE TRIGGER kb_chunks_search_dirty AFTER UPDATE OF content ON kb_chunks
WHEN NEW.content IS NOT OLD.content AND NEW.search_text IS OLD.search_text
BEGIN
    UPDATE kb_chunks SET search_text = NULL, search_text_version = 0 WHERE id = NEW.id;
END;
