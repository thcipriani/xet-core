use crate::command::CliOverrides;
use crate::config::axe::AxeSettings;
use crate::config::cache::CacheSettings;
use crate::config::cas::CasSettings;
use crate::config::env::XetEnv;
use crate::config::git_path::{ConfigGitPathOption, RepoInfo};
use crate::config::log::LogSettings;
use crate::config::permission::Permission;
use crate::config::user::UserSettings;
use crate::config::util;
use crate::config::util::OptionHelpers;
use crate::config::ConfigError;
use crate::config::ConfigError::{
    Config, InvalidMerkleDBParent, InvalidSummaryDBParent, MerkleDBNotDir, MerkleDBNotFile,
    MerkleDBReadOnly, ProfileNotFound, StagingDirNotCreated, StagingPathNotDir, SummaryDBNotFile,
    SummaryDBReadOnly, UnsupportedConfiguration,
};
use crate::constants::{
    CAS_STAGING_SUBDIR, GIT_LAZY_CHECKOUT_CONFIG, GIT_REPO_SPECIFIC_CONFIG, MERKLEDBV1_PATH_SUBDIR,
    MERKLEDB_V2_CACHE_PATH_SUBDIR, MERKLEDB_V2_SESSION_PATH_SUBDIR, SUMMARIES_PATH_SUBDIR,
};
use crate::errors::GitXetRepoError;
use crate::git_integration::{run_git_captured, GitXetRepo};
use crate::smudge_query_interface::SmudgeQueryPolicy;
use std::fs;
use std::path::{Path, PathBuf};
use url::Url;
use xet_config::{Cfg, Level, XetConfigLoader};

use super::upstream_config::{LocalXetRepoConfig, UpstreamXetRepo};
use toml;

/// Custom env keys
const XET_NO_SMUDGE_ENV: &str = "XET_NO_SMUDGE";

/// Custom env keys
const XET_DISABLE_VERSION_CHECK: &str = "XET_DISABLE_VERSION_CHECK";

/// The configuration for the Xet Client Application. This struct represents the resolved and
/// validated config to be used by the Xet client.
///
/// As opposed to the lower-level, [Cfg], a [XetConfig] provides a cleaner interface
/// for interacting with configurations that the rest of the Xet Client may need. This includes
/// validations that paths/sockets are valid, removing the sea of Option needed by the serialization
/// interface, and helper functions to assist with common config tasks.
///
/// Construction of the XetConfig follows a fairly complex strategy as there are many locations
/// from which the configuration can be gathered. The entrypoint into the config is through:
/// [XetConfig::new].
///
/// Overall, the XetConfig has the following resolution hierarchy (from highest priority to lowest):
/// 1. Setting a config on the CLI.
/// 2. ENV variables defined for the current `profile` (e.g. `XET_DEV_USER_NAME` for profile: "dev").
/// 3. Settings defined in an associated repo's config file for the current profile.
/// 4. Settings defined in the global config file for the current profile.
/// 5. ENV variables defined for the "unnamed" `profile` (e.g. `XET_USER_NAME`).
/// 6. Settings defined in an associated repo's config file for the "unnamed" profile.
/// 7. Settings defined in the global config file for the "unnamed" profile.
/// 8. XetHub defaults.
#[derive(Debug, Clone)]
pub struct XetConfig {
    pub cas: CasSettings,
    pub cache: CacheSettings,
    pub log: LogSettings,
    pub repo_path_if_present: Option<PathBuf>,
    pub merkledb: PathBuf,
    // The directory to cache MDB shards pulled from CAS.
    pub merkledb_v2_cache: PathBuf,
    // The directory to hold MDB shards created in a session (between pushes).
    pub merkledb_v2_session: PathBuf,
    pub smudge_query_policy: SmudgeQueryPolicy,
    pub summarydb: PathBuf,
    pub staging_path: Option<PathBuf>,
    pub user: UserSettings,
    pub axe: AxeSettings,
    pub force_no_smudge: bool,
    pub disable_version_check: bool,
    pub lazy_config: Option<PathBuf>,
    pub origin_cfg: Cfg,
    pub upstream_xet_repo: Option<UpstreamXetRepo>,
    pub permission: Permission,
}

// pub methods
impl XetConfig {
    /// Creates a empty XetConfig with very default values.
    /// You should consider whether you should be using new(None, None, ConfigGitPathOption::NoPath)
    /// instead.
    pub fn empty() -> Self {
        Self {
            cas: Default::default(),
            cache: Default::default(),
            log: Default::default(),
            user: Default::default(),
            axe: Default::default(),
            repo_path_if_present: None,
            merkledb: Default::default(),
            merkledb_v2_cache: Default::default(),
            merkledb_v2_session: Default::default(),
            smudge_query_policy: Default::default(),
            summarydb: Default::default(),
            staging_path: None,
            force_no_smudge: false,
            disable_version_check: true,
            lazy_config: None,
            origin_cfg: Cfg::with_default_values(),
            upstream_xet_repo: Default::default(),
            permission: Permission::current(),
        }
    }

    /// Creates a new [XetConfig].
    ///
    /// Args:
    /// - maybe_initial_cfg - an optional starting point for building the config
    /// - maybe_overrides - optional overrides defined by the CLI
    /// - gitpath - a path to the repo to associate to the config.
    ///
    /// This follows the following high-level process:
    ///
    /// 1. Start with the `maybe_initial_cfg` if provided, if not, then load an initial config
    ///    from the [system](load_system_cfg()).
    /// 2. Try to find a profile from either a provided profile (via `maybe_overrides`) or
    ///    by finding a profile that matches the associated repo's XetHub remote.
    /// 3. Apply the profile to the config if found.
    /// 4. Apply overrides defined in `maybe_overrides` to the config
    pub fn new(
        maybe_initial_cfg: Option<Cfg>,
        maybe_overrides: Option<CliOverrides>,
        gitpath: ConfigGitPathOption,
    ) -> Result<Self, GitXetRepoError> {
        let cfg = maybe_initial_cfg.ok_or_result(load_system_cfg)?;
        cfg_to_xetconfig(cfg, maybe_overrides, gitpath).map_err(ConfigError::into)
    }

    /// Allows switching the underlying config to a new repo path (and potentially a new profile).
    /// Note, that any overrides originally provided by the CLI will **NOT** be applied to this
    /// new config.
    pub fn switch_repo_path(
        &self,
        gitpath: ConfigGitPathOption,
        overrides: Option<CliOverrides>,
    ) -> Result<XetConfig, GitXetRepoError> {
        cfg_to_xetconfig(self.origin_cfg.clone(), overrides, gitpath).map_err(ConfigError::into)
    }

    /// Allows switching the underlying config to a new repo info (and potentially a new profile).
    /// Note, that any overrides originally provided by the CLI will **NOT** be applied to this
    /// new config.
    pub fn switch_repo_info(
        &self,
        repo_info: RepoInfo,
        overrides: Option<CliOverrides>,
    ) -> Result<XetConfig, GitXetRepoError> {
        cfg_to_xetconfig_with_repoinfo(self.origin_cfg.clone(), overrides, repo_info)
            .map_err(ConfigError::into)
    }

    /// Configure necessary information for xetblob without git repo.
    pub fn switch_xetblob_path(
        self,
        xetblob: &Path,
        overrides: Option<CliOverrides>,
    ) -> Result<XetConfig, GitXetRepoError> {
        self.try_with_xetblob_path(xetblob, overrides)
            .map_err(ConfigError::into)
    }

    /// Get the path to the associated repo. If there is no associated repo, then an error is returned.
    pub fn repo_path(&self) -> Result<&PathBuf, GitXetRepoError> {
        self.repo_path_if_present.as_ref().ok_or_else(|| {
            GitXetRepoError::Other("Associated repository required for this operation.".to_string())
        })
    }

    /// Obtains the path to the associated repository or current directory.
    pub fn get_implied_repo_path(&self) -> Result<PathBuf, GitXetRepoError> {
        match self.repo_path() {
            Ok(path) => Ok(path.clone()),
            Err(_) => std::env::current_dir().map_err(|_| {
                GitXetRepoError::Other("Unable to find current directory".to_string())
            }),
        }
    }

    /// Whether this config has an associated repo.
    pub fn associated_with_repo(&self) -> bool {
        self.repo_path_if_present.is_some()
    }

    /// Get the remote urls for the associated repo if present.
    pub fn remote_repo_paths(&self) -> Vec<String> {
        let maybe_path = self.repo_path_if_present.as_deref();
        GitXetRepo::get_remote_urls(maybe_path).unwrap_or_else(|_| vec!["".to_string()])
    }

    /// Builds an authenticated URL from a URL by injecting in
    /// a username and password as appropriate.
    /// If there already exists a username in the URL, a password will be
    /// inserted if we recognize the username.
    /// Noop if :
    ///  - URL does not parse
    ///  - is not http/https
    ///  - no username/password is configured
    pub fn build_authenticated_remote_url(&self, url: &str) -> String {
        let remote_url = Url::parse(url).ok();
        if remote_url.is_none() {
            // not URL
            return url.to_string();
        }
        let mut remote_url = remote_url.unwrap();
        if remote_url.scheme() != "http" && remote_url.scheme() != "https" {
            // not HTTP
            return url.to_string();
        }
        // do we have username / password configured?

        if let (Some(config_user_name), Some(config_token)) = (&self.user.name, &self.user.token) {
            if !remote_url.username().is_empty() {
                // there is a username in the url
                if remote_url.password().is_some() {
                    // and there is a password in the url
                    // so we just passthough
                    return url.to_string();
                }

                if config_user_name == remote_url.username() {
                    // there is no password, but username matches
                    // So we fill in the password
                    let _ = remote_url.set_password(Some(config_token));
                    return remote_url.as_str().to_string();
                }
                // unknown username. passthrough
                return url.to_string();
            }
            // no username / password
            // we set our own
            let _ = remote_url.set_username(config_user_name);
            let _ = remote_url.set_password(Some(config_token));
            return remote_url.as_str().to_string();
        }
        url.to_string()
    }
}

/// Creates a [XetConfigLoader] to manage the underlying config file(s).
pub fn create_config_loader() -> Result<XetConfigLoader, GitXetRepoError> {
    Ok(XetConfigLoader::new(
        util::get_local_config()?,
        util::get_global_config()?,
    ))
}

// very internal methods
impl XetConfig {
    fn try_from_cfg(active_cfg: Cfg, repo_info: &RepoInfo) -> Result<Self, ConfigError> {
        // Creation of the .xet folder happens below, check permission before it is created.
        let permission = Permission::current();

        let xet_home = dirs::home_dir()
            .ok_or_else(|| ConfigError::HomePathUnknown)?
            .join(".xet");

        let path = if let Some(cache) = &active_cfg.cache {
            cache.path.clone().unwrap_or(xet_home)
        } else {
            xet_home
        };

        permission.check_path(&path);

        Ok(Self {
            cas: active_cfg.cas.as_ref().try_into()?,
            cache: active_cfg.cache.as_ref().try_into()?,
            log: active_cfg.log.as_ref().try_into()?,
            user: (active_cfg.user.as_ref(), &repo_info.remote_urls).try_into()?,
            axe: active_cfg.axe.as_ref().try_into()?,
            repo_path_if_present: repo_info.maybe_git_path.as_ref().cloned(),
            merkledb: Default::default(),
            merkledb_v2_cache: Default::default(),
            merkledb_v2_session: Default::default(),
            smudge_query_policy: Default::default(),
            summarydb: Default::default(),
            staging_path: None,
            force_no_smudge: (!active_cfg.smudge.unwrap_or(true)),
            disable_version_check: false,
            lazy_config: None,
            upstream_xet_repo: Default::default(),
            origin_cfg: active_cfg,
            permission,
        })
    }

    fn with_origin_cfg(mut self, origin_cfg: Cfg) -> Self {
        self.origin_cfg = origin_cfg;
        self
    }

    fn try_with_repo_info(
        self,
        repo_info: &RepoInfo,
        overrides: &Option<CliOverrides>,
    ) -> Result<Self, ConfigError> {
        Ok(match repo_info.maybe_git_path.as_ref() {
            Some(repo_path) => {
                let git_path = repo_path.clone();
                let merkledb = match overrides.as_ref().and_then(|x| x.merkledb.clone()) {
                    Some(merkledb) => merkledb,
                    None => git_path.join(MERKLEDBV1_PATH_SUBDIR),
                };
                let merkledb_v2_cache =
                    match overrides.as_ref().and_then(|x| x.merkledb_v2_cache.clone()) {
                        Some(merkledb_v2_cache) => merkledb_v2_cache,
                        None => git_path.join(MERKLEDB_V2_CACHE_PATH_SUBDIR),
                    };
                let merkledb_v2_session = match overrides
                    .as_ref()
                    .and_then(|x| x.merkledb_v2_session.as_ref())
                {
                    Some(merkledb_v2_session) => merkledb_v2_session.clone(),
                    None => git_path.join(MERKLEDB_V2_SESSION_PATH_SUBDIR),
                };
                let smudge_query_policy = overrides
                    .as_ref()
                    .map(|x| x.smudge_query_policy)
                    .unwrap_or_default();

                let summarydb = git_path.join(SUMMARIES_PATH_SUBDIR);
                let staging_path = git_path.join(CAS_STAGING_SUBDIR);
                let lazy_config = git_path.join(GIT_LAZY_CHECKOUT_CONFIG);

                self.try_with_merkledb(merkledb)?
                    .try_with_merkledb_v2_cache(merkledb_v2_cache)?
                    .try_with_merkledb_v2_session(merkledb_v2_session)?
                    .try_with_summarydb(summarydb)?
                    .try_with_staging_path(staging_path)?
                    .try_with_smudge_query_policy(smudge_query_policy)?
                    .try_with_version_check_policy(overrides)?
                    .try_with_lazy_config(lazy_config)?
                    .try_with_repo_config_file(&git_path)?
            }
            None => self,
        })
    }

    fn try_with_xetblob_path(
        self,
        xetblob: &Path,
        overrides: Option<CliOverrides>,
    ) -> Result<Self, ConfigError> {
        let merkledb_v2_cache = match overrides.as_ref().and_then(|x| x.merkledb_v2_cache.clone()) {
            Some(merkledb_v2_cache) => merkledb_v2_cache,
            None => xetblob.join(MERKLEDB_V2_CACHE_PATH_SUBDIR),
        };
        let merkledb_v2_session = match overrides
            .as_ref()
            .and_then(|x| x.merkledb_v2_session.as_ref())
        {
            Some(merkledb_v2_session) => merkledb_v2_session.clone(),
            None => xetblob.join(MERKLEDB_V2_SESSION_PATH_SUBDIR),
        };
        let smudge_query_policy = overrides.map(|x| x.smudge_query_policy).unwrap_or_default();

        let summarydb = xetblob.join(SUMMARIES_PATH_SUBDIR);

        self.try_with_merkledb_v2_cache(merkledb_v2_cache)?
            .try_with_merkledb_v2_session(merkledb_v2_session)?
            .try_with_smudge_query_policy(smudge_query_policy)?
            .try_with_summarydb(summarydb)
    }

    fn try_with_merkledb(mut self, merkledb: PathBuf) -> Result<Self, ConfigError> {
        if !merkledb.exists() {
            let parent_dir = merkledb
                .parent()
                .ok_or_else(|| InvalidMerkleDBParent(merkledb.clone()))?;
            fs::create_dir_all(parent_dir).map_err(|_| InvalidMerkleDBParent(merkledb.clone()))?;
        } else if !merkledb.is_file() {
            return Err(MerkleDBNotFile(merkledb));
        } else if !util::can_write(&merkledb) {
            return Err(MerkleDBReadOnly(merkledb));
        }
        self.merkledb = merkledb;
        Ok(self)
    }

    fn try_with_merkledb_v2_cache(
        mut self,
        merkledb_v2_cache: PathBuf,
    ) -> Result<Self, ConfigError> {
        if !merkledb_v2_cache.exists() {
            fs::create_dir_all(&merkledb_v2_cache)?;
        } else if !merkledb_v2_cache.is_dir() {
            return Err(MerkleDBNotDir(merkledb_v2_cache));
        } else if !util::can_write(&merkledb_v2_cache) {
            return Err(MerkleDBReadOnly(merkledb_v2_cache));
        }
        self.merkledb_v2_cache = merkledb_v2_cache;

        Ok(self)
    }

    fn try_with_merkledb_v2_session(
        mut self,
        merkledb_v2_session: PathBuf,
    ) -> Result<Self, ConfigError> {
        if !merkledb_v2_session.exists() {
            fs::create_dir_all(&merkledb_v2_session)?;
        } else if !merkledb_v2_session.is_dir() {
            return Err(MerkleDBNotDir(merkledb_v2_session));
        } else if !util::can_write(&merkledb_v2_session) {
            return Err(MerkleDBReadOnly(merkledb_v2_session));
        }
        self.merkledb_v2_session = merkledb_v2_session;
        Ok(self)
    }

    fn try_with_summarydb(mut self, summarydb: PathBuf) -> Result<Self, ConfigError> {
        if !summarydb.exists() {
            let parent_dir = summarydb
                .parent()
                .ok_or_else(|| InvalidSummaryDBParent(summarydb.clone()))?;
            fs::create_dir_all(parent_dir)
                .map_err(|_| InvalidSummaryDBParent(summarydb.clone()))?;
        } else if !summarydb.is_file() {
            return Err(SummaryDBNotFile(summarydb));
        } else if !util::can_write(&summarydb) {
            return Err(SummaryDBReadOnly(summarydb));
        }
        self.summarydb = summarydb;
        Ok(self)
    }

    fn try_with_smudge_query_policy(
        mut self,
        smudge_query_policy: Option<SmudgeQueryPolicy>,
    ) -> Result<Self, ConfigError> {
        self.smudge_query_policy = smudge_query_policy.unwrap_or_default();
        Ok(self)
    }

    fn try_with_staging_path(mut self, staging_path: PathBuf) -> Result<Self, ConfigError> {
        if !staging_path.exists() {
            fs::create_dir_all(&staging_path)
                .map_err(|e| StagingDirNotCreated(staging_path.clone(), e))?;
        } else if !staging_path.is_dir() {
            return Err(StagingPathNotDir(staging_path));
        }
        self.staging_path = Some(staging_path);
        Ok(self)
    }

    fn try_with_version_check_policy(
        mut self,
        overrides: &Option<CliOverrides>,
    ) -> Result<Self, ConfigError> {
        if let Some(ovr) = overrides {
            if ovr.disable_version_check {
                self.disable_version_check = true;
            }
        }

        if self.disable_version_check || no_version_check_from_env() {
            self.disable_version_check = true;
        }

        Ok(self)
    }

    fn try_with_lazy_config(mut self, lazy_config: PathBuf) -> Result<Self, ConfigError> {
        self.lazy_config = if lazy_config.exists() {
            Some(lazy_config)
        } else {
            None
        };
        Ok(self)
    }

    fn try_with_repo_config_file(mut self, repo_dir: &PathBuf) -> Result<Self, ConfigError> {
        let query_spec = format!("HEAD:{GIT_REPO_SPECIFIC_CONFIG}");

        let Ok((status, stdout, _stderr)) =
            run_git_captured(Some(repo_dir), "show", &[&query_spec], false, None)
        else {
            return Ok(self);
        };

        if status != Some(0) {
            return Ok(self);
        }

        if let Ok(local_config) = toml::from_str::<LocalXetRepoConfig>(&stdout).map_err(
            |e|
        {
            let msg = format!("Warning: Error parsing local config ref {query_spec}: {e:?}. Please correct the errors and commit the corrected version into the repo."); 
            eprintln!("{msg}");
        }) {
            self.upstream_xet_repo = local_config.upstream;
        }

        Ok(self)
    }
}

fn no_version_check_from_env() -> bool {
    match std::env::var_os(XET_DISABLE_VERSION_CHECK) {
        Some(v) => v != "0",
        None => false,
    }
}

/// Returns true if XET_NO_SMUDGE=1 is set in the environment
fn no_smudge_from_env() -> bool {
    match std::env::var_os(XET_NO_SMUDGE_ENV) {
        Some(v) => v != "0",

        None => false,
    }
}

/// Try to remove XET_NO_SMUDGE from the environment. This is to avoid
/// polluting the config parsing from ENV
fn remove_no_smudge_from_env() {
    std::env::remove_var(XET_NO_SMUDGE_ENV);
}

/// Loads the current known cfg reading system and environment variables.
fn load_system_cfg() -> Result<Cfg, GitXetRepoError> {
    let no_smudge = no_smudge_from_env();
    if no_smudge {
        remove_no_smudge_from_env()
    }

    let loader = create_config_loader()?;
    let mut resolved_cfg = loader.resolve_config(Level::ENV).map_err(Config)?;

    resolved_cfg.smudge = Some(!no_smudge);

    Ok(resolved_cfg)
}

/// Converts a Cfg to a XetConfig, applying any profile and overrides to the config.
fn cfg_to_xetconfig_with_repoinfo(
    cfg: Cfg,
    overrides: Option<CliOverrides>,
    repo_info: RepoInfo,
) -> Result<XetConfig, ConfigError> {
    let original_cfg = cfg.clone();

    // Apply profile to the cfg
    let profile_name = overrides.as_ref().and_then(|o| o.profile.as_ref());
    let profile_cfg = load_profile(&cfg, profile_name, &repo_info)?;
    let working_cfg = profile_cfg
        .cloned()
        .map(|pcfg| cfg.apply_override(pcfg))
        .unwrap_or(Ok(cfg))
        .map_err(Config)?;

    // Apply cli-overrides to the cfg
    let working_cfg = overrides
        .as_ref()
        .map(util::get_override_cfg)
        .map(|override_cfg| working_cfg.apply_override(override_cfg))
        .unwrap_or(Ok(working_cfg))
        .map_err(Config)?;

    // Build the XetConfig from the updated Cfg, saving the original Cfg, and updating the paths
    // via the repo info.
    XetConfig::try_from_cfg(working_cfg, &repo_info)?
        .with_origin_cfg(original_cfg)
        .try_with_repo_info(&repo_info, &overrides)
}

/// Converts a Cfg to a XetConfig, applying any profile and overrides to the config.
fn cfg_to_xetconfig(
    cfg: Cfg,
    overrides: Option<CliOverrides>,
    gitpath: ConfigGitPathOption,
) -> Result<XetConfig, ConfigError> {
    let repo_info = gitpath.into_repo_info()?;
    cfg_to_xetconfig_with_repoinfo(cfg, overrides, repo_info)
}

/// Loads a profile that should be used from the [Cfg]. If a profile name has been indicated,
/// then we try to find that profile, returning an error if it cannot be found.
///
/// If a profile name hasn't been indicated, then we will look in the config for a profile
/// whose endpoint matches that of the Xetea environment for this repo (e.g. xethub.com).
/// If no such profile can be found, then Ok(None) is returned. However, if there are multiple
/// profiles that both apply to the endpoint, then an error will be returned.
fn load_profile<'a>(
    cfg: &'a Cfg,
    maybe_profile_name: Option<&String>,
    repo_info: &RepoInfo,
) -> Result<Option<&'a Cfg>, ConfigError> {
    if let Some(profile_name) = maybe_profile_name {
        // User provided a specific profile to use. Try to find that or error out.
        return cfg
            .profiles
            .get(profile_name)
            .map(Some)
            .ok_or_else(|| ProfileNotFound(profile_name.clone()));
    }
    // Search in the cfg profiles for one that matches the Xetea environment for the repo
    let mut candidates: Vec<Option<&'a Cfg>> = vec![];
    for prof in cfg.profiles.values() {
        if let Some(endpoint) = &prof.endpoint {
            if repo_info.env == XetEnv::Custom {
                for remote_url in &repo_info.remote_urls {
                    if remote_url.contains(endpoint) {
                        candidates.push(Some(prof));
                    }
                }
            } else if XetEnv::from_xetea_url(endpoint) == repo_info.env {
                candidates.push(Some(prof));
            }
        }
    }
    // it is annoyingly difficult to dedup by ref.
    // candidates.dedup does not work. It seems to dedupe (Some(&a), Some(&b))
    // into Some(&a)
    candidates.dedup_by_key(|x| x.map_or(0_usize, |x| x as *const Cfg as usize));
    if candidates.len() > 1 {
        return Err(UnsupportedConfiguration(format!(
            "Multiple profiles match the requested endpoint {:?}",
            repo_info.remote_urls
        )));
    }
    if candidates.is_empty() {
        Ok(None)
    } else {
        Ok(candidates[0])
    }
}

#[cfg(test)]
mod config_create_tests {
    use super::*;
    use crate::config::env::XetEnv;
    use crate::config::git_path::{ConfigGitPathOption, RepoInfo};
    use crate::config::xet::{cfg_to_xetconfig, load_profile, XetConfig};
    use crate::git_integration::git_repo_test_tools::TestRepoPath;
    use crate::git_integration::run_git_captured;
    use std::str::FromStr;
    use tokio_test::assert_err;
    use xet_config::{Cache, User, PROD_CAS_ENDPOINT};

    fn get_test_dev_profile() -> Cfg {
        Cfg {
            endpoint: Some("xethubdev.com".to_string()),
            user: Some(User {
                name: Some("dev-user".to_string()),
                token: Some("tokenABCXet".to_string()),
                ..Default::default()
            }),
            cache: Some(Cache {
                blocksize: Some(1024),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn get_test_prod_profile() -> Cfg {
        Cfg {
            endpoint: Some("xethub.com".to_string()),
            user: Some(User {
                name: Some("prod-user".to_string()),
                token: Some("tokenXetABC".to_string()),
                ..Default::default()
            }),
            cache: Some(Cache {
                blocksize: Some(1_000_000_000),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn get_test_custom_profile() -> Cfg {
        Cfg {
            endpoint: Some("gitlab.com".to_string()),
            user: Some(User {
                name: Some("custom-user".to_string()),
                token: Some("".to_string()),
                ..Default::default()
            }),
            cache: Some(Cache {
                size: Some(0),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn test_load_requested_profile() {
        let mut cfg = Cfg::with_default_values();
        let expected_profile = get_test_dev_profile();
        let profiles = &mut cfg.profiles;
        let key = "dev".to_string();
        profiles.insert(key.clone(), expected_profile.clone());
        let repo_info = RepoInfo::default();
        let profile_cfg = load_profile(&cfg, Some(&key), &repo_info).unwrap().unwrap();
        assert_eq!(expected_profile, *profile_cfg);
    }

    #[test]
    fn test_load_requested_profile_multiple() {
        let mut cfg = Cfg::with_default_values();
        let prod_profile = get_test_prod_profile();
        let dev_profile = get_test_dev_profile();
        let profiles = &mut cfg.profiles;
        let dev_key = "dev".to_string();
        let prod_key = "prod".to_string();
        profiles.insert(dev_key, dev_profile);
        profiles.insert(prod_key.clone(), prod_profile.clone());
        let repo_info = RepoInfo::default();
        let profile_cfg = load_profile(&cfg, Some(&prod_key), &repo_info)
            .unwrap()
            .unwrap();
        assert_eq!(prod_profile, *profile_cfg);
    }

    #[test]
    fn test_load_requested_profile_missing() {
        let cfg = Cfg::with_default_values();
        let key = "dev".to_string();
        let repo_info = RepoInfo::default();
        assert_err!(load_profile(&cfg, Some(&key), &repo_info));
    }

    #[test]
    fn test_load_profile_endpoint() {
        let mut cfg = Cfg::with_default_values();
        let expected_profile = get_test_dev_profile();
        let profiles = &mut cfg.profiles;
        let key = "dev".to_string();
        profiles.insert(key, expected_profile.clone());
        let repo_info = RepoInfo {
            env: XetEnv::Custom,
            remote_urls: vec!["https://xethubdev.com/org/repo".to_string()],
            maybe_git_path: None,
        };
        let profile_cfg = load_profile(&cfg, None, &repo_info).unwrap().unwrap();
        assert_eq!(expected_profile, *profile_cfg);
    }

    #[test]
    fn test_load_profile_endpoint_custom_key() {
        let mut cfg = Cfg::with_default_values();
        let expected_profile = get_test_dev_profile();
        let profiles = &mut cfg.profiles;
        let key = "something_else".to_string();
        profiles.insert(key, expected_profile.clone());
        let repo_info = RepoInfo {
            env: XetEnv::Custom,
            remote_urls: vec!["https://xethubdev.com/org/repo".to_string()],
            maybe_git_path: None,
        };
        let profile_cfg = load_profile(&cfg, None, &repo_info).unwrap().unwrap();
        assert_eq!(expected_profile, *profile_cfg);
    }
    #[test]
    fn test_load_profile_endpoint_custom_env() {
        let mut cfg = Cfg::with_default_values();
        let expected_profile = get_test_custom_profile();
        let dev_profile = get_test_dev_profile();
        let profiles = &mut cfg.profiles;
        profiles.insert("custom".to_string(), expected_profile.clone());
        profiles.insert("dev".to_string(), dev_profile);
        let repo_info = RepoInfo {
            env: XetEnv::Custom,
            remote_urls: vec!["https://gitlab.com/org/repo".to_string()],
            maybe_git_path: None,
        };
        let profile_cfg = load_profile(&cfg, None, &repo_info).unwrap().unwrap();
        assert_eq!(expected_profile, *profile_cfg);
    }

    #[test]
    fn test_load_profile_endpoint_not_found() {
        let mut cfg = Cfg::with_default_values();
        let prod_profile = get_test_prod_profile();
        let profiles = &mut cfg.profiles;
        let key = "my_prod".to_string();
        profiles.insert(key, prod_profile);
        let repo_info = RepoInfo {
            env: XetEnv::Custom,
            remote_urls: vec!["https://xethub1.com/org/repo".to_string()],
            maybe_git_path: None,
        };
        let profile_cfg = load_profile(&cfg, None, &repo_info).unwrap();
        assert!(profile_cfg.is_none());
    }

    #[test]
    fn test_load_profile_fail_multiple_valid_profiles() {
        let mut cfg = Cfg::with_default_values();
        let dev_profile = get_test_dev_profile();
        let dev1_profile = get_test_dev_profile();
        let profiles = &mut cfg.profiles;
        profiles.insert("prod".to_string(), dev_profile);
        profiles.insert("prod1".to_string(), dev1_profile);
        let repo_info = RepoInfo {
            env: XetEnv::Custom,
            remote_urls: vec!["https://xethubdev.com/org/repo".to_string()],
            maybe_git_path: None,
        };
        assert_err!(load_profile(&cfg, None, &repo_info));
    }

    #[test]
    fn test_load_profile_succeed_multiple_identical_profiles() {
        let mut cfg = Cfg::with_default_values();
        let dev_profile = get_test_dev_profile();
        let profiles = &mut cfg.profiles;
        profiles.insert("prod".to_string(), dev_profile);
        {
            let repo_info = RepoInfo {
                env: XetEnv::Custom,
                remote_urls: vec![
                    "https://xethubdev.com/org/repo".to_string(),
                    "https://xethubdev.com/org/repo".to_string(),
                ],
                maybe_git_path: None,
            };
            let profile_cfg = load_profile(&cfg, None, &repo_info).unwrap();
            assert!(profile_cfg.is_some());
        }
        {
            let repo_info = RepoInfo {
                env: XetEnv::Custom,
                remote_urls: vec![
                    "https://xethubdev.com/org/repo".to_string(),
                    "https://xethubdev.com/user/repo".to_string(),
                ],
                maybe_git_path: None,
            };
            let profile_cfg = load_profile(&cfg, None, &repo_info).unwrap();
            assert!(profile_cfg.is_some());
        }
    }

    #[test]
    fn test_try_from_default_cfg() {
        let cfg = Cfg::with_default_values();
        let xet_config = XetConfig::try_from_cfg(cfg.clone(), &RepoInfo::default()).unwrap();
        assert_eq!(cfg, xet_config.origin_cfg);
    }

    #[test]
    fn test_try_from_cfg_profile() {
        let mut cfg = Cfg::with_default_values();
        cfg.user = Some(User {
            name: Some("default-user".to_string()),
            token: Some("tokenDefault".to_string()),
            ..Default::default()
        });
        let dev_profile = get_test_dev_profile();
        let profiles = &mut cfg.profiles;
        profiles.insert("dev".to_string(), dev_profile);

        let tmp_repo = TestRepoPath::new("config_with_profiles").unwrap();
        let path = tmp_repo.path;
        run_git_captured(Some(&path), "init", &[], true, None).unwrap();
        run_git_captured(
            Some(&path),
            "remote",
            &["add", "origin", "http://xethubdev.com/org/repo.git"],
            true,
            None,
        )
        .unwrap();

        let cloned_cfg = cfg.clone();
        let config = cfg_to_xetconfig(cfg, None, ConfigGitPathOption::PathDiscover(path)).unwrap();
        assert_eq!(PROD_CAS_ENDPOINT.to_string(), config.cas.endpoint);
        assert_eq!(
            cloned_cfg.cache.as_ref().unwrap().size.unwrap(),
            config.cache.size
        );
        assert_eq!("dev-user", config.user.name.as_ref().unwrap());
    }

    #[test]
    fn test_try_from_cfg_no_profile() {
        let mut cfg = Cfg::with_default_values();
        cfg.user = Some(User {
            name: Some("default-user".to_string()),
            token: Some("tokenDefault".to_string()),
            ..Default::default()
        });
        let prod_profile = get_test_prod_profile();
        let profiles = &mut cfg.profiles;
        profiles.insert("prod".to_string(), prod_profile);

        let tmp_repo = TestRepoPath::new("config_with_profiles").unwrap();
        let path = tmp_repo.path;
        run_git_captured(Some(&path), "init", &[], true, None).unwrap();
        run_git_captured(
            Some(&path),
            "remote",
            &["add", "origin", "http://xethub1.com/org/repo.git"],
            true,
            None,
        )
        .unwrap();

        let cloned_cfg = cfg.clone();
        let config = cfg_to_xetconfig(cfg, None, ConfigGitPathOption::PathDiscover(path)).unwrap();
        assert_eq!(PROD_CAS_ENDPOINT.to_string(), config.cas.endpoint);
        assert_eq!(
            cloned_cfg.cache.as_ref().unwrap().size.unwrap(),
            config.cache.size
        );
        assert_eq!("default-user", config.user.name.as_ref().unwrap());
    }

    #[test]
    fn test_try_from_cfg_cli_overrides() {
        let mut cfg = Cfg::with_default_values();
        cfg.user = Some(User {
            name: Some("default-user".to_string()),
            token: Some("tokenDefault".to_string()),
            ..Default::default()
        });
        let prod_profile = get_test_prod_profile();
        let profiles = &mut cfg.profiles;
        profiles.insert("prod".to_string(), prod_profile);

        let tmp_repo = TestRepoPath::new("config_with_profiles").unwrap();
        let path = tmp_repo.path;
        run_git_captured(Some(&path), "init", &[], true, None).unwrap();
        run_git_captured(
            Some(&path),
            "remote",
            &["add", "origin", "http://xethub.com/org/repo.git"],
            true,
            None,
        )
        .unwrap();

        let cloned_cfg = cfg.clone();
        let expected_mdb_path = PathBuf::from_str("merkledb.other").unwrap();
        let expected_mdbv2_cache_path = PathBuf::from_str("shard-cache").unwrap();
        let expected_mdbv2_session_path = PathBuf::from_str("shard-session").unwrap();
        let overrides = CliOverrides {
            verbose: 2,
            log: None,
            cas: None,
            smudge_query_policy: Default::default(),
            merkledb: Some(expected_mdb_path.clone()),
            merkledb_v2_cache: Some(expected_mdbv2_cache_path.clone()),
            merkledb_v2_session: Some(expected_mdbv2_session_path.clone()),
            profile: None,
            user_name: None,
            user_token: None,
            user_email: None,
            disable_version_check: true,
            user_login_id: None,
        };
        let config = cfg_to_xetconfig(
            cfg,
            Some(overrides),
            ConfigGitPathOption::PathDiscover(path),
        )
        .unwrap();
        assert_eq!(PROD_CAS_ENDPOINT.to_string(), config.cas.endpoint);
        assert_eq!(
            cloned_cfg.cache.as_ref().unwrap().size.unwrap(),
            config.cache.size
        );
        assert_eq!(tracing::Level::DEBUG, config.log.level);
        assert_eq!(expected_mdb_path, config.merkledb);
        assert_eq!(expected_mdbv2_cache_path, config.merkledb_v2_cache);
        assert_eq!(expected_mdbv2_session_path, config.merkledb_v2_session);
    }

    #[test]
    fn test_try_from_cfg_no_path_with_profile() {
        let mut cfg = Cfg::with_default_values();
        cfg.user = Some(User {
            name: Some("default-user".to_string()),
            token: Some("tokenDefault".to_string()),
            ..Default::default()
        });
        let prod_profile = get_test_prod_profile();
        let profiles = &mut cfg.profiles;
        profiles.insert("prod".to_string(), prod_profile);

        let cloned_cfg = cfg.clone();
        let config = cfg_to_xetconfig(cfg, None, ConfigGitPathOption::NoPath).unwrap();
        assert_eq!(PROD_CAS_ENDPOINT.to_string(), config.cas.endpoint); // default should be prod
        assert_eq!(
            cloned_cfg.cache.as_ref().unwrap().size.unwrap(),
            config.cache.size
        );
        assert_eq!("prod-user", config.user.name.as_ref().unwrap());
    }

    #[test]
    fn test_try_from_cfg_no_path_no_profile() {
        let mut cfg = Cfg::with_default_values();
        cfg.user = Some(User {
            name: Some("default-user".to_string()),
            token: Some("tokenDefault".to_string()),
            ..Default::default()
        });

        let cloned_cfg = cfg.clone();
        let config = cfg_to_xetconfig(cfg, None, ConfigGitPathOption::NoPath).unwrap();
        assert_eq!(PROD_CAS_ENDPOINT.to_string(), config.cas.endpoint); // default should be prod
        assert_eq!(
            cloned_cfg.cache.as_ref().unwrap().size.unwrap(),
            config.cache.size
        );
        assert_eq!("default-user", config.user.name.as_ref().unwrap());
    }
}

#[cfg(test)]
impl Default for XetConfig {
    /// Default only needed for tests. Use [XetConfig::new(None, None, ConfigGitPathOption::NoPath)](XetConfig::new) instead.
    fn default() -> Self {
        Self::new(
            Some(Cfg::with_default_values()),
            None,
            ConfigGitPathOption::CurdirDiscover,
        )
        .unwrap()
    }
}
