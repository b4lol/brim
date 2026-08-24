//! Brim core engine.

pub mod backend;
pub mod backends;
pub mod config;
pub mod error;
pub(crate) mod fsutil;
pub mod http;
pub mod manager;
pub mod models;
pub mod sync;
pub mod trending;

pub use backend::Backend;
pub use config::{config_path, Config};
pub use error::{BrimError, Result};
pub use manager::PackageManager;
pub use models::{
    Category, Package, PackageStatus, RepoInfo, RepoKind, SourceStat, SourceType, SystemStats,
    TransactionAction, TransactionResult,
};
pub use sync::SyncEntry;
