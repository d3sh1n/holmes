use holmes_core::types::*;
use rusqlite::{params, Connection};
use std::path::Path;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::schema;
use crate::write_contention::WriteContention;

pub struct MemoryStore {
    conn: Arc<Mutex<Connection>>,
    write_contention: WriteContention,
}

#[derive(Debug, Clone)]
pub struct MemoryEntry {
    pub category: MemoryCategory,
    pub content: String,
    pub tags: Vec<String>,
    pub attack_type: Option<String>,
    pub tech_stack: Vec<String>,
    pub success: bool,
    pub relevance_score: f64,
    pub source_session_id: Option<String>,
}

impl MemoryStore {
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, rusqlite::Error> {
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA busy_timeout=1000; PRAGMA foreign_keys=ON;",
        )?;

        // Ensure schema_version table and migrations are applied. The
        // `memories` table (along with its FTS5 virtual table and triggers)
        // is created by SessionDB schema migrations; run them here as well so
        // a MemoryStore can be opened on a fresh database without needing a
        // SessionDB instance to be created first.
        conn.execute_batch(schema::schema_version_table())?;

        let current_version: u32 = conn
            .query_row(
                "SELECT COALESCE(MAX(version), 0) FROM schema_version",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);

        for (i, migration) in schema::MIGRATIONS.iter().enumerate() {
            let version = (i + 1) as u32;
            if version > current_version {
                conn.execute_batch(migration)?;
                conn.execute(
                    "INSERT INTO schema_version (version) VALUES (?1)",
                    params![version],
                )?;
            }
        }

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            write_contention: WriteContention::new(),
        })
    }

    pub async fn store(&self, entry: MemoryEntry) -> Result<String, rusqlite::Error> {
        let id = uuid::Uuid::new_v4().to_string();
        let now = chrono::Utc::now().to_rfc3339();

        let tags_json = serde_json::to_string(&entry.tags).unwrap_or_else(|_| "[]".into());
        let tech_json = serde_json::to_string(&entry.tech_stack).unwrap_or_else(|_| "[]".into());

        let id_clone = id.clone();
        self.write_contention
            .with_retry(|| {
                let id = id_clone.clone();
                let now = now.clone();
                let tags_json = tags_json.clone();
                let tech_json = tech_json.clone();
                let entry = entry.clone();
                async move {
                    let conn = self.conn.lock().await;
                    conn.execute(
                        "INSERT INTO memories (id, category, content, tags, attack_type, tech_stack,
                         success, relevance_score, source_session_id, created_at)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                        params![
                            id,
                            category_to_str(&entry.category),
                            entry.content,
                            tags_json,
                            entry.attack_type,
                            tech_json,
                            entry.success as i32,
                            entry.relevance_score,
                            entry.source_session_id,
                            now,
                        ],
                    )?;
                    Ok::<_, rusqlite::Error>(())
                }
            })
            .await?;

        Ok(id)
    }

    pub async fn search(&self, query: &str, top_k: u32) -> Result<Vec<Memory>, rusqlite::Error> {
        let sanitized = crate::fts::sanitize_fts5_query(query);
        let conn = self.conn.lock().await;

        let (sql, search_param) = if crate::fts::contains_cjk(query) {
            (
                "SELECT id, category, content, tags, attack_type, tech_stack,
                        success, relevance_score, source_session_id,
                        consolidated_from, created_at
                 FROM memories WHERE content LIKE ?1
                 ORDER BY relevance_score DESC LIMIT ?2",
                format!("%{}%", query),
            )
        } else {
            (
                "SELECT m.id, m.category, m.content, m.tags, m.attack_type, m.tech_stack,
                        m.success, m.relevance_score, m.source_session_id,
                        m.consolidated_from, m.created_at
                 FROM memories m JOIN memories_fts f ON m.rowid = f.rowid
                 WHERE memories_fts MATCH ?1 ORDER BY rank LIMIT ?2",
                sanitized,
            )
        };

        let mut stmt = conn.prepare(sql)?;
        let rows = stmt.query_map(params![search_param, top_k], |row| {
            let cat_str: String = row.get(1)?;
            let created_at_str: String = row.get(10)?;
            Ok(Memory {
                id: row.get(0)?,
                category: str_to_category(&cat_str),
                content: row.get(2)?,
                tags: row
                    .get::<_, String>(3)
                    .ok()
                    .and_then(|t| serde_json::from_str(&t).ok())
                    .unwrap_or_default(),
                attack_type: row.get(4)?,
                tech_stack: row
                    .get::<_, String>(5)
                    .ok()
                    .and_then(|t| serde_json::from_str(&t).ok()),
                success: row.get::<_, i32>(6).unwrap_or(0) != 0,
                relevance_score: row.get(7)?,
                source_session_id: row.get(8)?,
                consolidated_from: row
                    .get::<_, Option<String>>(9)?
                    .and_then(|c| serde_json::from_str(&c).ok()),
                created_at: chrono::DateTime::parse_from_rfc3339(&created_at_str)
                    .map(|dt| dt.with_timezone(&chrono::Utc))
                    .unwrap_or_else(|_| chrono::Utc::now()),
            })
        })?;

        let mut results = Vec::new();
        for row in rows {
            results.push(row?);
        }
        Ok(results)
    }

    pub async fn consolidate(
        &self,
        from_ids: &[String],
        into_content: &str,
        into_tags: &[String],
    ) -> Result<String, rusqlite::Error> {
        let new_id = uuid::Uuid::new_v4().to_string();
        let now = chrono::Utc::now().to_rfc3339();
        let tags_json = serde_json::to_string(into_tags).unwrap_or_else(|_| "[]".into());
        let from_ids_json = serde_json::to_string(from_ids).unwrap_or_else(|_| "[]".into());

        let new_id_clone = new_id.clone();
        self.write_contention
            .with_retry(|| {
                let new_id = new_id_clone.clone();
                let now = now.clone();
                let tags_json = tags_json.clone();
                let from_ids_json = from_ids_json.clone();
                let from_ids = from_ids.to_vec();
                let into_content = into_content.to_string();
                async move {
                    let conn = self.conn.lock().await;

                    // Build a placeholder list "(?, ?, ?)" matching the from_ids
                    // length so we can use plain IN(...) without the rarray
                    // extension (which is not enabled by default on rusqlite).
                    let placeholders = if from_ids.is_empty() {
                        "(NULL)".to_string()
                    } else {
                        let q: Vec<&str> = from_ids.iter().map(|_| "?").collect();
                        format!("({})", q.join(","))
                    };
                    let id_params: Vec<&dyn rusqlite::ToSql> =
                        from_ids.iter().map(|s| s as &dyn rusqlite::ToSql).collect();

                    // Get the highest relevance_score from the merged memories
                    let max_sql = format!(
                        "SELECT COALESCE(MAX(relevance_score), 0.0) FROM memories WHERE id IN {}",
                        placeholders
                    );
                    let max_score: f64 = conn
                        .query_row(&max_sql, id_params.as_slice(), |r| r.get(0))
                        .unwrap_or(0.0);

                    // Get the most common category
                    let cat_sql = format!(
                        "SELECT category FROM memories WHERE id IN {} \
                         GROUP BY category ORDER BY COUNT(*) DESC LIMIT 1",
                        placeholders
                    );
                    let category: String = conn
                        .query_row(&cat_sql, id_params.as_slice(), |r| r.get(0))
                        .unwrap_or_else(|_| "attack_experience".into());

                    // Get the most common attack_type
                    let atk_sql = format!(
                        "SELECT attack_type FROM memories WHERE id IN {} \
                         AND attack_type IS NOT NULL \
                         GROUP BY attack_type ORDER BY COUNT(*) DESC LIMIT 1",
                        placeholders
                    );
                    let attack_type: Option<String> = conn
                        .query_row(&atk_sql, id_params.as_slice(), |r| r.get(0))
                        .ok();

                    conn.execute(
                        "INSERT INTO memories (id, category, content, tags, attack_type, consolidated_from, relevance_score, created_at)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                        params![
                            new_id,
                            category,
                            into_content,
                            tags_json,
                            attack_type,
                            from_ids_json,
                            max_score,
                            now,
                        ],
                    )?;

                    // Soft-delete old memories (set relevance to 0)
                    for id in &from_ids {
                        conn.execute(
                            "UPDATE memories SET relevance_score = 0.0 WHERE id = ?1",
                            params![id],
                        )?;
                    }

                    Ok::<_, rusqlite::Error>(())
                }
            })
            .await?;

        Ok(new_id)
    }
}

fn category_to_str(cat: &MemoryCategory) -> &'static str {
    match cat {
        MemoryCategory::AttackExperience => "attack_experience",
        MemoryCategory::DiscoveredPattern => "discovered_pattern",
        MemoryCategory::ToolUsage => "tool_usage",
        MemoryCategory::TargetKnowledge => "target_knowledge",
    }
}

fn str_to_category(s: &str) -> MemoryCategory {
    match s {
        "discovered_pattern" => MemoryCategory::DiscoveredPattern,
        "tool_usage" => MemoryCategory::ToolUsage,
        "target_knowledge" => MemoryCategory::TargetKnowledge,
        _ => MemoryCategory::AttackExperience,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;
    use std::process;

    fn temp_db_path() -> String {
        let mut dir = env::temp_dir();
        dir.push(format!(
            "holmes_test_memory_{}_{}.db",
            process::id(),
            uuid::Uuid::new_v4()
        ));
        dir.to_string_lossy().to_string()
    }

    #[tokio::test]
    async fn test_store_and_search() {
        let path = temp_db_path();
        let store = MemoryStore::open(&path).await.unwrap();

        let id = store
            .store(MemoryEntry {
                category: MemoryCategory::AttackExperience,
                content: "SQL injection via UNION SELECT on login.php parameter username".into(),
                tags: vec!["sqli".into(), "union".into(), "login".into()],
                attack_type: Some("sqli".into()),
                tech_stack: vec!["PHP".into(), "MySQL".into()],
                success: true,
                relevance_score: 0.9,
                source_session_id: Some("test-session-1".into()),
            })
            .await
            .unwrap();

        assert!(!id.is_empty());

        let results = store.search("SQL injection", 10).await.unwrap();
        assert!(!results.is_empty());
        assert_eq!(results[0].tags, vec!["sqli", "union", "login"]);

        // Clean up
        std::fs::remove_file(&path).ok();
    }

    #[tokio::test]
    async fn test_consolidate() {
        let path = temp_db_path();
        let store = MemoryStore::open(&path).await.unwrap();

        let id1 = store
            .store(MemoryEntry {
                category: MemoryCategory::AttackExperience,
                content: "SQL injection in login.php via POST username".into(),
                tags: vec!["sqli".into(), "post".into()],
                attack_type: Some("sqli".into()),
                tech_stack: vec!["PHP".into()],
                success: true,
                relevance_score: 0.8,
                source_session_id: None,
            })
            .await
            .unwrap();

        let id2 = store
            .store(MemoryEntry {
                category: MemoryCategory::AttackExperience,
                content: "SQL injection in search.php via GET q parameter".into(),
                tags: vec!["sqli".into(), "get".into()],
                attack_type: Some("sqli".into()),
                tech_stack: vec!["PHP".into()],
                success: true,
                relevance_score: 0.7,
                source_session_id: None,
            })
            .await
            .unwrap();

        let consolidated_id = store
            .consolidate(
                &[id1.clone(), id2.clone()],
                "Multiple SQL injection points found in PHP app - both POST and GET vectors",
                &["sqli".into(), "php".into(), "consolidated".into()],
            )
            .await
            .unwrap();

        assert!(!consolidated_id.is_empty());

        // Old memories should have relevance 0
        let results = store.search("sqli php", 5).await.unwrap();
        // The consolidated entry should appear with the new tags
        let consolidated = results.iter().find(|m| m.id == consolidated_id);
        assert!(consolidated.is_some());
        assert!(consolidated
            .unwrap()
            .tags
            .contains(&"consolidated".to_string()));

        std::fs::remove_file(&path).ok();
    }
}
