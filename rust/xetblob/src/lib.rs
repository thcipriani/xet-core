mod atomic_commit_queries;
mod bbq_queries;
mod file_open_flags;
mod retry_policy;
mod rfile_object;
mod wfile_object;
mod xet_repo;
mod xet_repo_manager;

use anyhow::anyhow;
use serde::{Deserialize, Serialize};
use tabled::Tabled;

pub use file_open_flags::*;
pub use rfile_object::XetRFileObject;
pub use wfile_object::XetWFileObject;
pub use xet_repo::XetRepo;
pub use xet_repo::XetRepoWriteTransaction;
pub use xet_repo_manager::XetRepoManager;
/// this is the JSON structure returned by the xetea directory listing function
#[derive(Serialize, Deserialize, Tabled, Debug)]
pub struct DirEntry {
    pub name: String,
    pub size: u64,
    #[serde(rename = "type")]
    pub object_type: String,
    #[serde(rename = "githash")]
    #[tabled(skip)]
    pub git_hash: String,
    #[serde(default)]
    #[serde(rename = "lastmodified")]
    pub last_modified: String,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct AuxRepoInfo {
    pub html_url: String,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct XetRepoInfo {
    pub mdb_version: String,
    pub repo_salt: Option<String>,
}

/// this is the JSON structure returned by the xetea repo info function,
/// explicitly ignoring part of the "repo" section because unneeded.
#[derive(Serialize, Deserialize, Debug)]
pub struct RepoInfo {
    pub repo: AuxRepoInfo,
    pub xet: XetRepoInfo,
}
