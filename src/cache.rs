// SQLite缓存模块 - 连接池版本

#![allow(
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap,
    clippy::uninlined_format_args,
    clippy::cast_lossless,
    clippy::collapsible_if,
    clippy::io_other_error
)]

use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::{Connection, params};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use dunce;

use crate::error::{CacheOperation, HashError, HashResult, IntoCacheError, IoErrorContext};

/// 当前缓存版本
const CURRENT_CACHE_VERSION: u32 = 3;

/// VACUUM 阈值配置
const VACUUM_SIZE_THRESHOLD: f64 = 0.3; // 30% free space

/// 缓存配置
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheConfig {
    pub min_file_size: u64,
    pub retention_days: u32,
    pub buffer_size: usize,
    pub mmap_chunk_size: usize,
    pub auto_compute_enabled: bool,
    pub uppercase_display: bool,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            min_file_size: 1024 * 1024,
            retention_days: 30,
            buffer_size: 256 * 1024,
            mmap_chunk_size: 4 * 1024 * 1024,
            auto_compute_enabled: true,
            uppercase_display: true,
        }
    }
}

/// 缓存条目
#[derive(Debug, Clone)]
pub struct CacheEntry {
    pub path: PathBuf,
    pub file_size: u64,
    pub modified_time: u64,
    pub cached_at: u64,
    pub xxhash3: String,
    pub crc32: String,
    pub md5: String,
    pub sha1: String,
}

/// 路径规范化器（带缓存）
pub struct PathNormalizer {
    cache: Arc<Mutex<HashMap<PathBuf, PathBuf>>>,
}

impl PathNormalizer {
    pub fn new() -> Self {
        Self {
            cache: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn normalize(&self, path: &Path) -> HashResult<PathBuf> {
        let cache_guard = self.cache.lock().map_err(|e| HashError::Cache {
            operation: CacheOperation::PathNormalization,
            kind: crate::error::CacheErrorKind::PoolExhausted,
            context: format!("Mutex 中毒（读取缓存时）: {}", e),
        })?;

        if let Some(cached) = cache_guard.get(path) {
            return Ok(cached.clone());
        }
        drop(cache_guard);

        let normalized = dunce::canonicalize(path).with_path(path)?;

        #[cfg(windows)]
        let normalized = {
            let s = normalized.to_string_lossy().to_lowercase();
            PathBuf::from(s)
        };

        let mut cache_guard = self.cache.lock().map_err(|e| HashError::Cache {
            operation: CacheOperation::PathNormalization,
            kind: crate::error::CacheErrorKind::PoolExhausted,
            context: format!("Mutex 中毒（写入缓存时）: {}", e),
        })?;

        cache_guard.insert(path.to_path_buf(), normalized.clone());
        Ok(normalized)
    }
}

/// SQLite 连接池管理器
pub struct HashCachePool {
    read_pool: Pool<SqliteConnectionManager>,
    write_pool: Pool<SqliteConnectionManager>,
    config: CacheConfig,
    pub path_normalizer: Arc<PathNormalizer>,
}

impl HashCachePool {
    pub fn new(db_path: &Path, config: CacheConfig) -> HashResult<Self> {
        Self::initialize_database(db_path)?;

        let read_manager = SqliteConnectionManager::file(db_path).with_init(|conn| {
            let _ = conn.query_row("PRAGMA journal_mode=WAL", [], |row| row.get::<_, String>(0));
            let _ = conn.execute("PRAGMA synchronous=NORMAL", []);
            let _ = conn.execute("PRAGMA cache_size=-64000", []); // 64MB
            let _ = conn.execute("PRAGMA mmap_size=268435456", []); // 256MB
            let _ = conn.execute("PRAGMA temp_store=MEMORY", []);
            Ok(())
        });

        let write_manager = SqliteConnectionManager::file(db_path).with_init(|conn| {
            let _ = conn.query_row("PRAGMA journal_mode=WAL", [], |row| row.get::<_, String>(0));
            let _ = conn.execute("PRAGMA synchronous=NORMAL", []);
            let _ = conn.execute("PRAGMA cache_size=-64000", []); // 64MB
            let _ = conn.execute("PRAGMA mmap_size=268435456", []); // 256MB
            let _ = conn.execute("PRAGMA temp_store=MEMORY", []);
            Ok(())
        });

        // 读连接池（10个连接）
        let read_pool = Pool::builder()
            .max_size(10)
            .min_idle(Some(2))
            .connection_timeout(Duration::from_secs(5))
            .build(read_manager)
            .map_err(|e: r2d2::Error| HashError::Cache {
                operation: CacheOperation::Connection,
                kind: crate::error::CacheErrorKind::ConnectionFailed(e.to_string()),
                context: "failed to create read pool".to_string(),
            })?;

        // 写连接池（2个连接）
        let write_pool = Pool::builder()
            .max_size(2)
            .min_idle(Some(1))
            .connection_timeout(Duration::from_secs(10))
            .build(write_manager)
            .map_err(|e: r2d2::Error| HashError::Cache {
                operation: CacheOperation::Connection,
                kind: crate::error::CacheErrorKind::ConnectionFailed(e.to_string()),
                context: "failed to create write pool".to_string(),
            })?;

        Ok(Self {
            read_pool,
            write_pool,
            config,
            path_normalizer: Arc::new(PathNormalizer::new()),
        })
    }

    /// 初始化数据库：创建表、索引、迁移
    fn initialize_database(db_path: &Path) -> HashResult<()> {
        let mut conn = Connection::open(db_path)
            .with_cache_error(CacheOperation::Connection, "failed to open database")?;

        // 读取当前版本
        let version: u32 = conn
            .query_row(
                "SELECT COALESCE((SELECT value FROM metadata WHERE key = 'version'), '0')",
                [],
                |row| row.get(0),
            )
            .unwrap_or(0);

        // 执行迁移
        if version < CURRENT_CACHE_VERSION {
            Self::run_migrations(&mut conn, version)?;
        }

        // 创建主表（v3 schema）
        Self::create_schema_v3(&mut conn)?;

        // 创建设置表
        conn.execute(
            "CREATE TABLE IF NOT EXISTS settings (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            )",
            [],
        )
        .with_cache_error(CacheOperation::Migrate, "failed to create settings table")?;

        Ok(())
    }

    /// 创建 v3 schema（带 CHECK 约束）
    fn create_schema_v3(conn: &mut Connection) -> HashResult<()> {
        conn.execute(
            "CREATE TABLE IF NOT EXISTS hash_cache (
                path TEXT NOT NULL PRIMARY KEY,
                file_size INTEGER NOT NULL CHECK(file_size > 0),
                modified_time INTEGER NOT NULL CHECK(modified_time >= 0),
                cached_at INTEGER NOT NULL CHECK(cached_at > 0),
                xxhash3 TEXT NOT NULL CHECK(length(xxhash3) = 32),
                crc32 TEXT NOT NULL CHECK(length(crc32) = 8),
                md5 TEXT NOT NULL CHECK(length(md5) = 32),
                sha1 TEXT NOT NULL CHECK(length(sha1) = 40),
                CHECK(xxhash3 GLOB '[0-9a-fA-F][0-9a-fA-F]*'),
                CHECK(crc32 GLOB '[0-9a-fA-F][0-9a-fA-F]*'),
                CHECK(md5 GLOB '[0-9a-fA-F][0-9a-fA-F]*'),
                CHECK(sha1 GLOB '[0-9a-fA-F][0-9a-fA-F]*')
            ) WITHOUT ROWID",
            [],
        )
        .with_cache_error(CacheOperation::Migrate, "failed to create hash_cache table")?;

        // 性能优化索引
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_cache_validation
             ON hash_cache(file_size, modified_time, xxhash3)",
            [],
        )
        .with_cache_error(CacheOperation::Migrate, "failed to create validation index")?;

        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_cache_cleanup
             ON hash_cache(cached_at)",
            [],
        )
        .with_cache_error(CacheOperation::Migrate, "failed to create cleanup index")?;

        Ok(())
    }

    /// 运行数据库迁移
    fn run_migrations(conn: &mut Connection, current_version: u32) -> HashResult<()> {
        let tx = conn.unchecked_transaction().with_cache_error(
            CacheOperation::Migrate,
            "failed to begin migration transaction",
        )?;

        // 更新版本号到元数据表
        tx.execute(
            "CREATE TABLE IF NOT EXISTS metadata (key TEXT PRIMARY KEY, value TEXT)",
            [],
        )
        .with_cache_error(CacheOperation::Migrate, "failed to create metadata table")?;

        // 更新版本号
        tx.execute(
            "INSERT OR REPLACE INTO metadata (key, value) VALUES ('version', ?1)",
            params![CURRENT_CACHE_VERSION],
        )
        .with_cache_error(CacheOperation::Migrate, "failed to update version")?;

        tx.commit()
            .with_cache_error(CacheOperation::Migrate, "failed to commit migration")?;

        eprintln!("[Cache] 已迁移到 v{}", CURRENT_CACHE_VERSION);
        Ok(())
    }

    /// 批量查询缓存（使用读连接池）
    pub fn get_by_paths_batch(
        &self,
        paths: &[&Path],
    ) -> HashResult<HashMap<PathBuf, Option<CacheEntry>>> {
        if paths.is_empty() {
            return Ok(HashMap::new());
        }

        const SQLITE_MAX_VARIABLE_NUMBER: usize = 999;
        let mut result = HashMap::new();

        let conn = self.read_pool.get().map_err(|e| HashError::Cache {
            operation: CacheOperation::Connection,
            kind: crate::error::CacheErrorKind::PoolExhausted,
            context: format!("read pool timeout: {}", e),
        })?;

        // 规范化所有输入路径（关键修复：确保查询时也使用规范化路径）
        let normalized_paths: Vec<PathBuf> = paths
            .iter()
            .map(|p| self.path_normalizer.normalize(p))
            .collect::<HashResult<Vec<_>>>()?;

        // 为所有路径初始化为 None，然后填充找到的条目
        for (i, original_path) in paths.iter().enumerate() {
            result.insert(original_path.to_path_buf(), None);
            result.insert(normalized_paths[i].clone(), None);
        }

        for chunk in normalized_paths.chunks(SQLITE_MAX_VARIABLE_NUMBER) {
            let placeholders = (0..chunk.len()).map(|_| "?").collect::<Vec<_>>().join(", ");

            let sql = format!(
                "SELECT path, file_size, modified_time, cached_at, xxhash3, crc32, md5, sha1
                 FROM hash_cache WHERE path IN ({})",
                placeholders
            );

            // 使用 prepare_cached 提升性能
            let mut stmt = conn
                .prepare_cached(&sql)
                .with_cache_error(CacheOperation::BatchRead, "failed to prepare statement")?;

            let path_strs: Vec<String> = chunk
                .iter()
                .map(|p| {
                    p.to_str()
                        .ok_or_else(|| HashError::Cache {
                            operation: CacheOperation::PathNormalization,
                            kind: crate::error::CacheErrorKind::InvalidPath(
                                "path contains invalid UTF-8".to_string(),
                            ),
                            context: format!("path: {}", p.display()),
                        })
                        .map(|s| s.to_string())
                })
                .collect::<HashResult<Vec<_>>>()?;

            let params: Vec<&dyn rusqlite::ToSql> = path_strs
                .iter()
                .map(|s| s as &dyn rusqlite::ToSql)
                .collect();

            let mut rows = stmt
                .query(params.as_slice())
                .with_cache_error(CacheOperation::BatchRead, "query failed")?;

            while let Some(row) = rows
                .next()
                .with_cache_error(CacheOperation::BatchRead, "row iteration failed")?
            {
                let db_path = PathBuf::from(row.get::<_, String>(0)?);
                let entry = CacheEntry {
                    path: db_path.clone(),
                    file_size: row.get::<_, i64>(1)? as u64,
                    modified_time: row.get::<_, i64>(2)? as u64,
                    cached_at: row.get::<_, i64>(3)? as u64,
                    xxhash3: row.get(4)?,
                    crc32: row.get(5)?,
                    md5: row.get(6)?,
                    sha1: row.get(7)?,
                };
                // 同时用规范化路径和原始路径作为键
                result.insert(db_path.clone(), Some(entry.clone()));
                // 查找对应的原始路径并也插入
                if let Some(idx) = normalized_paths.iter().position(|p| p == &db_path) {
                    result.insert(paths[idx].to_path_buf(), Some(entry));
                }
            }
        }

        Ok(result)
    }

    /// 批量保存缓存（使用写连接池 + 路径规范化）
    pub fn save_entries_batch(&self, entries: &[CacheEntry]) -> HashResult<usize> {
        if entries.is_empty() {
            return Ok(0);
        }

        let conn = self
            .write_pool
            .get()
            .map_err(|e: r2d2::Error| HashError::Cache {
                operation: CacheOperation::Connection,
                kind: crate::error::CacheErrorKind::PoolExhausted,
                context: format!("write pool timeout: {}", e),
            })?;

        let tx = conn
            .unchecked_transaction()
            .with_cache_error(CacheOperation::BatchWrite, "failed to begin transaction")?;

        let mut saved = 0;
        {
            // 使用 prepare_cached
            let mut stmt = tx
                .prepare_cached(
                    "INSERT OR REPLACE INTO hash_cache
                 (path, file_size, modified_time, cached_at, xxhash3, crc32, md5, sha1)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                )
                .with_cache_error(CacheOperation::BatchWrite, "failed to prepare statement")?;

            for entry in entries {
                // 规范化路径
                let normalized_path = self.path_normalizer.normalize(&entry.path)?;
                let path_str = normalized_path.to_str().ok_or_else(|| HashError::Cache {
                    operation: CacheOperation::PathNormalization,
                    kind: crate::error::CacheErrorKind::InvalidPath(
                        "normalized path contains invalid UTF-8".to_string(),
                    ),
                    context: format!("path: {}", normalized_path.display()),
                })?;

                match stmt.execute(params![
                    path_str,
                    entry.file_size as i64,
                    entry.modified_time as i64,
                    entry.cached_at as i64,
                    &entry.xxhash3,
                    &entry.crc32,
                    &entry.md5,
                    &entry.sha1,
                ]) {
                    Ok(_) => saved += 1,
                    Err(e) => {
                        eprintln!("[Cache] 批量保存失败: {} (path: {})", e, path_str);
                    }
                }
            }
            // stmt 在这里 drop
        }

        tx.commit()
            .with_cache_error(CacheOperation::BatchWrite, "failed to commit transaction")?;

        Ok(saved)
    }

    /// 清理过期缓存
    pub fn cleanup_expired(&self) -> HashResult<usize> {
        if self.config.retention_days == 0 {
            return Ok(0);
        }

        let now = SystemTime::now();
        let elapsed = now
            .duration_since(UNIX_EPOCH)
            .map_err(|e| HashError::SystemResource(format!("SystemTime error: {}", e)))?;
        let cutoff_time = elapsed
            .as_secs()
            .saturating_sub(self.config.retention_days as u64 * 86400);

        let conn = self.write_pool.get().map_err(|e| HashError::Cache {
            operation: CacheOperation::Connection,
            kind: crate::error::CacheErrorKind::PoolExhausted,
            context: format!("write pool timeout: {}", e),
        })?;

        let deleted = conn
            .execute(
                "DELETE FROM hash_cache WHERE cached_at < ?1",
                params![cutoff_time as i64],
            )
            .with_cache_error(CacheOperation::Cleanup, "failed to delete expired entries")?;

        if deleted > 0 {
            eprintln!("[Cache] 清理了 {} 条过期条目", deleted);

            // 检查是否需要 VACUUM
            self.schedule_vacuum_if_needed()?;
        }

        Ok(deleted)
    }

    /// 清空所有缓存
    pub fn clear_all(&self) -> HashResult<usize> {
        let conn = self.write_pool.get().map_err(|e| HashError::Cache {
            operation: CacheOperation::Connection,
            kind: crate::error::CacheErrorKind::PoolExhausted,
            context: format!("write pool timeout: {}", e),
        })?;

        let deleted = conn
            .execute("DELETE FROM hash_cache", [])
            .with_cache_error(CacheOperation::Cleanup, "failed to clear all entries")?;

        Ok(deleted)
    }

    /// 使单个缓存条目失效
    pub fn invalidate_entry(&self, path: &Path) -> HashResult<()> {
        let normalized_path = self.path_normalizer.normalize(path)?;
        let path_str = normalized_path.to_str().ok_or_else(|| HashError::Cache {
            operation: CacheOperation::PathNormalization,
            kind: crate::error::CacheErrorKind::InvalidPath(
                "normalized path contains invalid UTF-8".to_string(),
            ),
            context: format!("path: {}", normalized_path.display()),
        })?;

        let conn = self.write_pool.get().map_err(|e| HashError::Cache {
            operation: CacheOperation::Connection,
            kind: crate::error::CacheErrorKind::PoolExhausted,
            context: format!("write pool timeout: {}", e),
        })?;

        conn.execute("DELETE FROM hash_cache WHERE path = ?1", params![path_str])
            .with_cache_error(CacheOperation::Cleanup, "failed to invalidate entry")?;

        Ok(())
    }

    /// 检查是否需要 VACUUM
    fn should_vacuum(&self) -> HashResult<bool> {
        // 检查空闲空间比例
        let conn = self.read_pool.get().map_err(|e| HashError::Cache {
            operation: CacheOperation::Connection,
            kind: crate::error::CacheErrorKind::PoolExhausted,
            context: format!("read pool timeout: {}", e),
        })?;

        let page_count: i64 = conn
            .query_row("PRAGMA page_count", [], |r| r.get(0))
            .unwrap_or(0);
        let free_pages: i64 = conn
            .query_row("PRAGMA freelist_count", [], |r| r.get(0))
            .unwrap_or(0);

        if page_count > 0 && (free_pages as f64 / page_count as f64) > VACUUM_SIZE_THRESHOLD {
            return Ok(true);
        }

        Ok(false)
    }

    /// 调度 VACUUM（异步执行）
    fn schedule_vacuum_if_needed(&self) -> HashResult<()> {
        if !self.should_vacuum()? {
            return Ok(());
        }

        let write_pool = self.write_pool.clone();

        thread::spawn(move || {
            // try_get 返回 Option，需要处理
            if let Some(conn) = write_pool.try_get() {
                eprintln!("[Cache] 开始 VACUUM...");

                match conn.execute("VACUUM", []) {
                    Ok(_) => {
                        eprintln!("[Cache] VACUUM 完成");
                        conn.execute("ANALYZE", []).ok();
                    }
                    Err(e) => {
                        eprintln!("[Cache] VACUUM 失败: {}", e);
                    }
                }
            }
        });

        Ok(())
    }

    /// 验证缓存条目与元数据匹配
    pub fn is_valid_with_metadata(entry: &CacheEntry, file_size: u64, modified_time: u64) -> bool {
        entry.file_size == file_size && entry.modified_time == modified_time
    }

    /// 验证缓存条目完整性
    pub fn validate_cache_integrity(
        entry: &CacheEntry,
        computed_xxhash3: &str,
        file_size: u64,
        modified_time: u64,
    ) -> bool {
        if entry.file_size != file_size {
            eprintln!(
                "[Cache] 验证失败: 文件大小不匹配 (缓存: {}, 当前: {})",
                entry.file_size, file_size
            );
            return false;
        }

        if entry.modified_time != modified_time {
            let (cache_secs, cache_nanos) = parse_modified_time(entry.modified_time);
            let (current_secs, current_nanos) = parse_modified_time(modified_time);
            eprintln!(
                "[Cache] 验证失败: 修改时间不匹配 (缓存: {}.{:09}, 当前: {}.{:09})",
                cache_secs, cache_nanos, current_secs, current_nanos
            );
            return false;
        }

        if entry.xxhash3 != computed_xxhash3 {
            eprintln!(
                "[Cache] 验证失败: xxhash3 不匹配 (缓存: {}, 计算: {})",
                entry.xxhash3, computed_xxhash3
            );
            return false;
        }

        true
    }

    /// 验证哈希格式
    pub fn verify_cached_hashes(&self, entry: &CacheEntry) -> HashResult<bool> {
        if entry.xxhash3.len() != 32 {
            return Ok(false);
        }

        if entry.crc32.len() != 8 {
            return Ok(false);
        }

        if entry.md5.len() != 32 {
            return Ok(false);
        }

        if entry.sha1.len() != 40 {
            return Ok(false);
        }

        // 验证十六进制格式
        if hex::decode(&entry.xxhash3).is_err()
            || hex::decode(&entry.crc32).is_err()
            || hex::decode(&entry.md5).is_err()
            || hex::decode(&entry.sha1).is_err()
        {
            return Ok(false);
        }

        Ok(true)
    }

    /// Getter 方法
    pub fn get_buffer_size(&self) -> usize {
        self.config.buffer_size
    }

    pub fn get_mmap_chunk_size(&self) -> usize {
        self.config.mmap_chunk_size
    }

    /// 设置管理
    pub fn save_setting(&self, key: &str, value: &str) -> HashResult<()> {
        let conn = self.write_pool.get().map_err(|e| HashError::Cache {
            operation: CacheOperation::Connection,
            kind: crate::error::CacheErrorKind::PoolExhausted,
            context: format!("write pool timeout: {}", e),
        })?;

        conn.execute(
            "INSERT OR REPLACE INTO settings (key, value) VALUES (?1, ?2)",
            params![key, value],
        )
        .with_cache_error(CacheOperation::Connection, "failed to save setting")?;

        Ok(())
    }

    pub fn get_setting(&self, key: &str) -> HashResult<Option<String>> {
        let conn = self.read_pool.get().map_err(|e| HashError::Cache {
            operation: CacheOperation::Connection,
            kind: crate::error::CacheErrorKind::PoolExhausted,
            context: format!("read pool timeout: {}", e),
        })?;

        let mut stmt = conn
            .prepare_cached("SELECT value FROM settings WHERE key = ?1")
            .with_cache_error(CacheOperation::Connection, "failed to prepare statement")?;

        let result = stmt.query_row(params![key], |row| row.get::<_, String>(0));

        match result {
            Ok(v) => Ok(Some(v)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(HashError::Cache {
                operation: CacheOperation::Connection,
                kind: crate::error::CacheErrorKind::QueryFailed(e.to_string()),
                context: format!("failed to get setting: {}", key),
            }),
        }
    }

    fn get_setting_or_default<T: FromStr + Copy>(&self, key: &str, default: T) -> T {
        self.get_setting(key)
            .ok()
            .flatten()
            .and_then(|s| s.parse().ok())
            .unwrap_or(default)
    }

    pub fn save_cache_config(&self, config: &CacheConfig) -> HashResult<()> {
        self.save_setting("min_file_size", &config.min_file_size.to_string())?;
        self.save_setting("retention_days", &config.retention_days.to_string())?;
        self.save_setting("buffer_size", &config.buffer_size.to_string())?;
        self.save_setting("mmap_chunk_size", &config.mmap_chunk_size.to_string())?;
        self.save_setting(
            "auto_compute_enabled",
            &config.auto_compute_enabled.to_string(),
        )?;
        self.save_setting("uppercase_display", &config.uppercase_display.to_string())?;
        Ok(())
    }

    pub fn load_cache_config(&self) -> HashResult<CacheConfig> {
        let default = CacheConfig::default();

        Ok(CacheConfig {
            min_file_size: self.get_setting_or_default("min_file_size", default.min_file_size),
            retention_days: self.get_setting_or_default("retention_days", default.retention_days),
            buffer_size: self.get_setting_or_default("buffer_size", default.buffer_size),
            mmap_chunk_size: self
                .get_setting_or_default("mmap_chunk_size", default.mmap_chunk_size),
            auto_compute_enabled: self
                .get_setting_or_default("auto_compute_enabled", default.auto_compute_enabled),
            uppercase_display: self
                .get_setting_or_default("uppercase_display", default.uppercase_display),
        })
    }
}

pub fn get_file_modified_time(path: &Path) -> HashResult<u64> {
    let metadata = fs::metadata(path).with_path(path)?;
    let time = metadata.modified().with_path(path)?;
    let duration = time.duration_since(UNIX_EPOCH).map_err(|_| {
        HashError::Io(
            std::io::Error::new(std::io::ErrorKind::Other, "SystemTime before UNIX_EPOCH"),
            path.to_path_buf(),
        )
    })?;

    // 高精度时间戳：秒 + 纳秒的组合
    let secs = duration.as_secs();
    let nanos = duration.subsec_nanos();
    Ok((secs << 32) | (nanos as u64))
}

pub fn parse_modified_time(combined: u64) -> (u64, u32) {
    let secs = combined >> 32;
    let nanos = (combined & 0xFFFF_FFFF) as u32;
    (secs, nanos)
}

// 为了保持向后兼容，保留旧的别名
pub use HashCachePool as HashCache;

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn create_test_pool() -> HashResult<(HashCachePool, TempDir)> {
        let temp_dir = TempDir::new().map_err(|e| {
            HashError::Io(
                std::io::Error::new(std::io::ErrorKind::Other, e.to_string()),
                PathBuf::from("temp"),
            )
        })?;
        let db_path = temp_dir.path().join("test.db");
        let config = CacheConfig::default();
        let pool = HashCachePool::new(&db_path, config)?;
        Ok((pool, temp_dir))
    }

    #[test]
    fn test_connection_pool_creation() {
        let (pool, _temp) = create_test_pool().unwrap();
        assert_eq!(pool.config.min_file_size, 1024 * 1024);
    }

    #[test]
    fn test_path_normalization() {
        let normalizer = PathNormalizer::new();

        // 测试基本路径
        let test_path = Path::new(".");
        let normalized = normalizer.normalize(test_path);
        assert!(normalized.is_ok());

        // 测试缓存
        let _ = normalizer.normalize(test_path).unwrap();
    }

    #[test]
    fn test_batch_save_and_query() {
        let (pool, temp) = create_test_pool().unwrap();

        let entries: Vec<CacheEntry> = (0..10)
            .map(|i| {
                let mut path = temp.path().to_path_buf();
                path.push(format!("file{}.txt", i));

                // Create the file so path normalization works
                let _ = std::fs::write(&path, format!("test content {}", i));

                // Normalize the path first, since the cache stores normalized paths
                let normalized = pool.path_normalizer.normalize(&path).unwrap();

                CacheEntry {
                    path: normalized,
                    file_size: 1024 * (i + 1),
                    modified_time: 12345 + i as u64,
                    cached_at: 67890,
                    xxhash3: format!("{:032}", i), // 32字符十六进制
                    crc32: format!("{:08x}", i),   // 8字符十六进制
                    md5: format!("{:032}", i),     // 32字符十六进制
                    sha1: format!("{:040}", i),    // 40字符十六进制
                }
            })
            .collect();

        let saved = pool.save_entries_batch(&entries).unwrap();
        assert_eq!(saved, 10);

        let paths: Vec<&Path> = entries.iter().map(|e| e.path.as_path()).collect();
        let result = pool.get_by_paths_batch(&paths).unwrap();

        assert_eq!(result.len(), 10);
        for entry in &entries {
            let loaded = result.get(&entry.path).unwrap();
            assert!(loaded.is_some());
            assert_eq!(loaded.as_ref().unwrap().file_size, entry.file_size);
        }
    }

    #[test]
    fn test_constraint_validation() {
        let (pool, temp) = create_test_pool().unwrap();

        let mut path = temp.path().to_path_buf();
        path.push("invalid.txt");

        // Create the file so path normalization works
        let _ = std::fs::write(&path, "test content");

        // 测试无效哈希长度（应该被 CHECK 约束拒绝）
        let invalid_entry = CacheEntry {
            path: path.clone(),
            file_size: 1024,
            modified_time: 12345,
            cached_at: 67890,
            xxhash3: "too_short".to_string(), // 无效长度
            crc32: "01234567".to_string(),
            md5: "0123456789abcdef0123456789abcdef".to_string(),
            sha1: "0123456789abcdef0123456789abcdef01234567".to_string(),
        };

        let saved = pool.save_entries_batch(&[invalid_entry]).unwrap();
        // CHECK 约束应该阻止插入
        assert_eq!(saved, 0);
    }

    #[test]
    fn test_cleanup_expired() {
        let (pool, temp) = create_test_pool().unwrap();

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let mut path = temp.path().to_path_buf();
        path.push("old.txt");

        // Create the file so path normalization works
        let _ = std::fs::write(&path, "test content");

        // Normalize the path
        let normalized = pool.path_normalizer.normalize(&path).unwrap();

        let old_entry = CacheEntry {
            path: normalized,
            file_size: 1024,
            modified_time: 12345,
            cached_at: now - 40 * 86400, // 40天前 (默认保留期30天)
            xxhash3: format!("{:032}", 1),
            crc32: format!("{:08x}", 1),
            md5: format!("{:032}", 1),
            sha1: format!("{:040}", 1),
        };

        pool.save_entries_batch(&[old_entry]).unwrap();

        let deleted = pool.cleanup_expired().unwrap();
        assert!(deleted > 0);
    }

    #[test]
    fn test_cache_integrity_validation() {
        let entry = CacheEntry {
            path: PathBuf::from("/test/file"),
            file_size: 1024,
            modified_time: 12345,
            xxhash3: "0123456789abcdef0123456789abcdef".to_string(),
            crc32: "01234567".to_string(),
            md5: "0123456789abcdef0123456789abcdef".to_string(),
            sha1: "0123456789abcdef0123456789abcdef01234567".to_string(),
            cached_at: 1_234_567_890,
        };

        // 测试文件大小不匹配
        assert!(!HashCachePool::validate_cache_integrity(
            &entry,
            "0123456789abcdef0123456789abcdef",
            2048,
            12345
        ));

        // 测试完全匹配
        assert!(HashCachePool::validate_cache_integrity(
            &entry,
            "0123456789abcdef0123456789abcdef",
            1024,
            12345
        ));
    }

    #[test]
    fn test_settings() {
        let (pool, _temp) = create_test_pool().unwrap();

        pool.save_setting("test_key", "test_value").unwrap();
        let value = pool.get_setting("test_key").unwrap();
        assert_eq!(value, Some("test_value".to_string()));
    }

    #[test]
    fn test_config_persistence() {
        let (pool, _temp) = create_test_pool().unwrap();

        let mut config = CacheConfig::default();
        config.min_file_size = 2048 * 1024;
        config.retention_days = 60;

        pool.save_cache_config(&config).unwrap();
        let loaded = pool.load_cache_config().unwrap();

        assert_eq!(loaded.min_file_size, 2048 * 1024);
        assert_eq!(loaded.retention_days, 60);
    }
}
