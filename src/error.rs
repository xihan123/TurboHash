// 错误类型定义模块

use std::fmt;
use std::io;
use std::path::PathBuf;
use std::time::SystemTimeError;

/// 缓存操作类型（用于错误分类）
#[derive(Debug, Clone, Copy)]
pub enum CacheOperation {
    BatchRead,
    BatchWrite,
    Cleanup,
    Migrate,
    Connection,
    PathNormalization,
}

impl fmt::Display for CacheOperation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CacheOperation::BatchRead => write!(f, "批量读取"),
            CacheOperation::BatchWrite => write!(f, "批量写入"),
            CacheOperation::Cleanup => write!(f, "清理过期"),
            CacheOperation::Migrate => write!(f, "数据库迁移"),
            CacheOperation::Connection => write!(f, "连接池"),
            CacheOperation::PathNormalization => write!(f, "路径规范化"),
        }
    }
}

/// 缓存错误类型
#[derive(Debug)]
pub enum CacheErrorKind {
    ConnectionFailed(String),
    DatabaseLocked,
    ConstraintViolation(String),
    QueryFailed(String),
    InvalidPath(String),
    PoolExhausted,
}

impl fmt::Display for CacheErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CacheErrorKind::ConnectionFailed(msg) => write!(f, "连接失败: {}", msg),
            CacheErrorKind::DatabaseLocked => write!(f, "数据库锁定"),
            CacheErrorKind::ConstraintViolation(msg) => write!(f, "约束违反: {}", msg),
            CacheErrorKind::QueryFailed(msg) => write!(f, "查询失败: {}", msg),
            CacheErrorKind::InvalidPath(msg) => write!(f, "无效路径: {}", msg),
            CacheErrorKind::PoolExhausted => write!(f, "连接池耗尽"),
        }
    }
}

#[derive(Debug)]
pub enum HashError {
    Io(io::Error, PathBuf),
    FontLoadFailed(String),
    Cache {
        operation: CacheOperation,
        kind: CacheErrorKind,
        context: String,
    },
    SystemResource(String),
    #[cfg(target_pointer_width = "32")]
    FileTooLarge(PathBuf),
}

impl fmt::Display for HashError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HashError::Io(err, path) => {
                write!(f, "IO错误: {} (路径: {})", err, path.display())
            }
            HashError::FontLoadFailed(msg) => {
                write!(f, "字体加载失败: {}", msg)
            }
            HashError::Cache {
                operation,
                kind,
                context,
            } => {
                write!(f, "缓存错误 [{}] {} - {}", operation, kind, context)
            }
            HashError::SystemResource(msg) => {
                write!(f, "系统资源错误: {}", msg)
            }
            #[cfg(target_pointer_width = "32")]
            HashError::FileTooLarge(path) => {
                write!(f, "文件过大（超过32位系统限制）: {}", path.display())
            }
        }
    }
}

impl std::error::Error for HashError {}

impl From<io::Error> for HashError {
    fn from(err: io::Error) -> Self {
        HashError::Io(err, PathBuf::from("unknown"))
    }
}

pub type HashResult<T> = Result<T, HashError>;

pub trait IoErrorContext<T> {
    fn with_path<P: Into<PathBuf>>(self, path: P) -> HashResult<T>;
}

impl<T> IoErrorContext<T> for Result<T, io::Error> {
    fn with_path<P: Into<PathBuf>>(self, path: P) -> HashResult<T> {
        self.map_err(|e| HashError::Io(e, path.into()))
    }
}

/// Helper trait for converting rusqlite errors to structured cache errors
pub trait IntoCacheError<T> {
    fn with_cache_error(self, operation: CacheOperation, context: &str) -> HashResult<T>;
}

impl<T> IntoCacheError<T> for Result<T, rusqlite::Error> {
    fn with_cache_error(self, operation: CacheOperation, context: &str) -> HashResult<T> {
        self.map_err(|e| {
            let kind = match &e {
                rusqlite::Error::SqliteFailure(err, msg) => match &err.code {
                    rusqlite::ErrorCode::ConstraintViolation => {
                        CacheErrorKind::ConstraintViolation(
                            msg.as_deref().unwrap_or("unknown").to_string(),
                        )
                    }
                    rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked => {
                        CacheErrorKind::DatabaseLocked
                    }
                    _ => CacheErrorKind::QueryFailed(format!(
                        "{:?}: {}",
                        e,
                        msg.as_deref().unwrap_or("unknown")
                    )),
                },
                rusqlite::Error::QueryReturnedNoRows => {
                    CacheErrorKind::QueryFailed("no rows returned".to_string())
                }
                _ => CacheErrorKind::QueryFailed(e.to_string()),
            };
            HashError::Cache {
                operation,
                kind,
                context: context.to_string(),
            }
        })
    }
}

// Legacy rusqlite 错误转换（保持向后兼容）
impl From<rusqlite::Error> for HashError {
    fn from(err: rusqlite::Error) -> Self {
        HashError::Cache {
            operation: CacheOperation::Connection,
            kind: CacheErrorKind::QueryFailed(err.to_string()),
            context: "legacy conversion".to_string(),
        }
    }
}

// walkdir 错误转换
impl From<walkdir::Error> for HashError {
    fn from(err: walkdir::Error) -> Self {
        let path_buf = err
            .path()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("unknown"));
        match err.io_error() {
            Some(io_err) => {
                // 创建新的 io::Error，因为 io::Error 不实现 Clone
                HashError::Io(io::Error::new(io_err.kind(), io_err.to_string()), path_buf)
            }
            None => HashError::Io(
                io::Error::new(io::ErrorKind::Other, err.to_string()),
                path_buf,
            ),
        }
    }
}

// SystemTimeError 转换
impl From<SystemTimeError> for HashError {
    fn from(err: SystemTimeError) -> Self {
        HashError::SystemResource(format!("System time error: {}", err))
    }
}
