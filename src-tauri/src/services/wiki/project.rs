use std::path::PathBuf;
use tokio::fs;
use uuid::Uuid;
use chrono::Utc;

/// Get the base wiki directory for all projects.
pub fn wiki_base_dir() -> PathBuf {
    let base = dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("./data"))
        .join("waliapi")
        .join("wiki");
    let _ = std::fs::create_dir_all(&base);
    base
}

/// Get a project's wiki directory.
pub fn project_wiki_dir(project_id: &str) -> PathBuf {
    let dir = wiki_base_dir().join("projects").join(project_id);
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::create_dir_all(dir.join("raw").join("sources"));
    let _ = std::fs::create_dir_all(dir.join("raw").join("assets"));
    let _ = std::fs::create_dir_all(dir.join("wiki").join("entities"));
    let _ = std::fs::create_dir_all(dir.join("wiki").join("concepts"));
    let _ = std::fs::create_dir_all(dir.join("wiki").join("summaries"));
    let _ = std::fs::create_dir_all(dir.join("schema"));
    dir
}

/// Initialize a new wiki project directory structure.
pub async fn init_project_dir(project_id: &str, schema_text: &str) -> Result<PathBuf, String> {
    let dir = project_wiki_dir(project_id);

    // Write schema/CLAUDE.md
    let schema_path = dir.join("schema").join("CLAUDE.md");
    fs::write(&schema_path, schema_text).await
        .map_err(|e| format!("Failed to write schema: {}", e))?;

    // Write wiki/index.md
    let index_path = dir.join("wiki").join("index.md");
    if !index_path.exists() {
        fs::write(&index_path, "# Wiki Index\n\n<!-- Add pages below -->\n").await
            .map_err(|e| format!("Failed to write index: {}", e))?;
    }

    // Write wiki/log.md
    let log_path = dir.join("wiki").join("log.md");
    if !log_path.exists() {
        let now = Utc::now().to_rfc3339();
        fs::write(&log_path, &format!("# Wiki Log\n\n## [{}] init | Project created\n", now)).await
            .map_err(|e| format!("Failed to write log: {}", e))?;
    }

    // Write .meta.json
    let meta_path = dir.join(".meta.json");
    let meta = serde_json::json!({
        "project_id": project_id,
        "created_at": Utc::now().to_rfc3339(),
    });
    fs::write(&meta_path, serde_json::to_string_pretty(&meta).unwrap()).await
        .map_err(|e| format!("Failed to write meta: {}", e))?;

    Ok(dir)
}

/// Read a wiki page from disk.
pub async fn read_page(project_id: &str, path: &str) -> Result<String, String> {
    let dir = project_wiki_dir(project_id);
    let full_path = dir.join("wiki").join(path);
    fs::read_to_string(&full_path).await
        .map_err(|e| format!("Failed to read {}: {}", path, e))
}

/// Write a wiki page to disk.
pub async fn write_page(project_id: &str, path: &str, content: &str) -> Result<(), String> {
    let dir = project_wiki_dir(project_id);
    let full_path = dir.join("wiki").join(path);
    if let Some(parent) = full_path.parent() {
        fs::create_dir_all(parent).await
            .map_err(|e| format!("Failed to create dir: {}", e))?;
    }
    fs::write(&full_path, content).await
        .map_err(|e| format!("Failed to write {}: {}", path, e))
}

/// Delete a wiki page from disk.
pub async fn delete_page_file(project_id: &str, path: &str) -> Result<(), String> {
    let dir = project_wiki_dir(project_id);
    let full_path = dir.join("wiki").join(path);
    if full_path.exists() {
        fs::remove_file(&full_path).await
            .map_err(|e| format!("Failed to delete {}: {}", path, e))?;
    }
    Ok(())
}

/// Append to log.md
pub async fn append_log(project_id: &str, entry: &str) -> Result<(), String> {
    let dir = project_wiki_dir(project_id);
    let log_path = dir.join("wiki").join("log.md");
    let now = Utc::now().format("%Y-%m-%d %H:%M").to_string();
    let line = format!("\n## [{}] {}\n", now, entry);
    let mut content = fs::read_to_string(&log_path).await.unwrap_or_default();
    content.push_str(&line);
    fs::write(&log_path, &content).await
        .map_err(|e| format!("Failed to append log: {}", e))
}

/// Update index.md with a new entry.
pub async fn update_index(project_id: &str, entries: &[IndexEntry]) -> Result<(), String> {
    let dir = project_wiki_dir(project_id);
    let index_path = dir.join("wiki").join("index.md");
    let mut content = String::from("# Wiki Index\n\n");
    for entry in entries {
        content.push_str(&format!("- [[{}]] — {}\n", entry.path, entry.summary));
    }
    fs::write(&index_path, &content).await
        .map_err(|e| format!("Failed to write index: {}", e))
}

pub struct IndexEntry {
    pub path: String,
    pub summary: String,
}

/// List all wiki page files on disk.
pub async fn list_page_files(project_id: &str) -> Result<Vec<PageFileInfo>, String> {
    let dir = project_wiki_dir(project_id);
    let wiki_dir = dir.join("wiki");
    let mut results = Vec::new();
    let mut stack = vec![wiki_dir.clone()];
    while let Some(current) = stack.pop() {
        let mut entries = match fs::read_dir(&current).await {
            Ok(e) => e,
            Err(_) => continue,
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if let Some(ext) = path.extension() {
                if ext == "md" {
                    let rel_path = path.strip_prefix(&wiki_dir).unwrap_or(&path).to_string_lossy().to_string();
                    let name = path.file_stem().unwrap_or_default().to_string_lossy().to_string();
                    results.push(PageFileInfo { path: rel_path, title: name });
                }
            }
        }
    }
    Ok(results)
}

pub struct PageFileInfo {
    pub path: String,
    pub title: String,
}

/// Write a source file to raw/sources/.
pub async fn write_source_file(project_id: &str, filename: &str, content: &[u8]) -> Result<PathBuf, String> {
    let dir = project_wiki_dir(project_id);
    let sources_dir = dir.join("raw").join("sources");
    fs::create_dir_all(&sources_dir).await
        .map_err(|e| format!("Failed to create sources dir: {}", e))?;
    let file_path = sources_dir.join(filename);
    fs::write(&file_path, content).await
        .map_err(|e| format!("Failed to write source file: {}", e))?;
    Ok(file_path)
}

/// Read a source file from raw/sources/ or raw/.
pub async fn read_source_file(project_id: &str, path: &str) -> Result<Vec<u8>, String> {
    let dir = project_wiki_dir(project_id);
    // Allow reading from raw/sources/ and wiki/ only
    let allowed_prefixes = ["raw/sources/", "wiki/"];
    let safe_path = allowed_prefixes.iter()
        .find_map(|prefix| {
            if path.starts_with(prefix) {
                Some(dir.join(path))
            } else {
                None
            }
        })
        .or_else(|| {
            // If path doesn't have a prefix, assume raw/sources/
            Some(dir.join("raw/sources").join(path))
        })
        .unwrap();

    if !safe_path.starts_with(&dir) {
        return Err("Path traversal detected".to_string());
    }

    fs::read(&safe_path).await
        .map_err(|e| format!("Failed to read file: {}", e))
}

/// Remove a project directory entirely.
pub async fn remove_project_dir(project_id: &str) -> Result<(), String> {
    let dir = project_wiki_dir(project_id);
    if dir.exists() {
        fs::remove_dir_all(&dir).await
            .map_err(|e| format!("Failed to remove project dir: {}", e))?;
    }
    Ok(())
}

pub fn new_uuid() -> String {
    Uuid::new_v4().to_string()
}
