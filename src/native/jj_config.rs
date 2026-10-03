//! The user's effective jj configuration, loaded in-process the way the pinned jj CLI loads it:
//! defaults, system and user files (`JJ_CONFIG`), environment layers (`JJ_USER`, `JJ_EMAIL`,
//! operation host and user), repo and workspace files (including jj's secure per-repo storage),
//! conditional `[[--scope]]` tables, and config migrations. jj-fork never lists or prints the
//! configuration; errors name the problem as jj would.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use jj_cli::cli_util::{default_ignored_remote_name, load_fileset_aliases, load_revset_aliases};
use jj_cli::command_error::CommandError;
use jj_cli::config::{
    ConfigEnv, config_from_environment, default_config_layers, default_config_migrations,
};
use jj_cli::ui::Ui;
use jj_lib::config::{ConfigGetResultExt as _, StackedConfig};
use jj_lib::fileset::FilesetAliasesMap;
use jj_lib::ref_name::WorkspaceName;
use jj_lib::repo::Repo as JjRepo;
use jj_lib::repo_path::RepoPathUiConverter;
use jj_lib::revset::{
    RevsetAliasesMap, RevsetDiagnostics, RevsetExtensions, RevsetParseContext,
    RevsetWorkspaceContext, UserRevsetExpression,
};
use jj_lib::settings::UserSettings;
use jj_lib::workspace::{DefaultWorkspaceLoaderFactory, WorkspaceLoaderFactory as _};
use serde::Serialize;
use sha2::{Digest as _, Sha256};

/// Converts a jj CLI error into an error with jj's message and hints.
pub fn cli_error(err: CommandError) -> anyhow::Error {
    let mut message = err.error.to_string();
    let mut source = err.error.source();
    while let Some(cause) = source {
        message.push_str(&format!(": {cause}"));
        source = cause.source();
    }
    for hint in &err.hints {
        message.push_str(&format!(" (hint: {hint:?})"));
    }
    anyhow!(message)
}

/// The jj command a configuration is resolved for: conditional tables
/// (`[[--scope]]` with `--when.commands`) apply only to their commands, as in jj.
///
/// jj-fork's own planning, snapshots, and Git imports resolve as the command `fork` (it runs as
/// `jj fork`), so `--when.commands = ["fork"]` scopes them. Its fetches resolve as `git fetch`
/// and its pushes as `git push`, the jj commands they stand in for, so scopes written for those
/// commands (for example a push-only `git.private-commits` or `git.sign-on-push`) still apply.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CommandContext {
    Fork,
    Fetch,
    Push,
}

impl CommandContext {
    pub fn command(self) -> &'static str {
        match self {
            CommandContext::Fork => "fork",
            CommandContext::Fetch => "git fetch",
            CommandContext::Push => "git push",
        }
    }
}

/// Loads the effective configuration for the workspace at `root` as jj resolves it for
/// `context`, or only the user-level layers when `root` has no jj workspace yet.
pub fn load(root: &Path, context: CommandContext) -> Result<StackedConfig> {
    let ui = Ui::null();
    let mut raw = config_from_environment(default_config_layers());
    let mut env = ConfigEnv::from_environment();
    env.reload_system_config(&mut raw)
        .context("failed to load the system jj config")?;
    env.reload_user_config(&mut raw)
        .context("failed to load the user jj config")?;
    if root.join(".jj").is_dir() {
        let loader = DefaultWorkspaceLoaderFactory
            .create(root)
            .context("failed to find the jj repository")?;
        env.reset_repo_path(loader.repo_path());
        env.reload_repo_config(&ui, &mut raw)
            .map_err(cli_error)
            .context("failed to load the repository jj config")?;
        env.reset_workspace_path(loader.workspace_root());
        env.reload_workspace_config(&ui, &mut raw)
            .map_err(cli_error)
            .context("failed to load the workspace jj config")?;
    }
    env.set_command_name(context.command().to_string());
    let mut config = env
        .resolve_config(&raw)
        .context("failed to resolve conditional jj config")?;
    jj_lib::config::migrate(&mut config, &default_config_migrations())
        .context("failed to migrate the jj config")?;
    Ok(config)
}

/// The `jj-fork` table of the effective jj config, merged across layers, if any layer sets it.
pub fn fork_overrides(config: &StackedConfig) -> Result<Option<toml::Table>> {
    config
        .get::<toml::Table>("jj-fork")
        .optional()
        .context("invalid jj-fork settings in jj config")
}

/// Everything jj-fork needs from the jj configuration of one workspace: settings, aliases, and
/// the revset context jj uses for user expressions.
pub struct JjEnv {
    pub settings: UserSettings,
    pub revset_aliases: RevsetAliasesMap,
    pub fileset_aliases: FilesetAliasesMap,
    pub extensions: Arc<RevsetExtensions>,
    path_converter: RepoPathUiConverter,
}

impl JjEnv {
    pub fn new(config: StackedConfig, workspace_root: &Path) -> Result<JjEnv> {
        let ui = Ui::null();
        let revset_aliases = load_revset_aliases(&ui, &config).map_err(cli_error)?;
        let fileset_aliases = load_fileset_aliases(&ui, &config).map_err(cli_error)?;
        let settings = UserSettings::from_config(config).context("invalid jj settings")?;
        Ok(JjEnv {
            settings,
            revset_aliases,
            fileset_aliases,
            extensions: Arc::new(RevsetExtensions::default()),
            path_converter: RepoPathUiConverter::Fs {
                cwd: workspace_root.to_path_buf(),
                base: workspace_root.to_path_buf(),
            },
        })
    }

    /// The context jj parses user revsets in, for a workspace of `repo`.
    pub fn revset_context<'a>(
        &'a self,
        repo: &dyn JjRepo,
        workspace_name: &'a WorkspaceName,
    ) -> RevsetParseContext<'a> {
        let now = match self.settings.commit_timestamp() {
            Some(timestamp) => chrono::DateTime::from_timestamp_millis(timestamp.timestamp.0)
                .unwrap_or_default()
                .with_timezone(&chrono::Local),
            None => chrono::Local::now(),
        };
        RevsetParseContext {
            aliases_map: &self.revset_aliases,
            local_variables: Default::default(),
            user_email: self.settings.user_email(),
            date_pattern_context: now.into(),
            default_ignored_remote: default_ignored_remote_name(repo.store()),
            fileset_aliases_map: &self.fileset_aliases,
            extensions: &self.extensions,
            workspace: Some(RevsetWorkspaceContext {
                path_converter: &self.path_converter,
                workspace_name,
            }),
        }
    }

    /// The user's `immutable_heads()` (plus the root), as jj protects them during rewrites.
    pub fn immutable_heads(
        &self,
        repo: &dyn JjRepo,
        workspace_name: &WorkspaceName,
    ) -> Result<Arc<UserRevsetExpression>> {
        let context = self.revset_context(repo, workspace_name);
        jj_cli::revset_util::parse_immutable_heads_expression(
            &mut RevsetDiagnostics::new(),
            &context,
        )
        .map_err(|err| anyhow!("invalid revset-aliases.immutable_heads(): {err}"))
    }

    /// Per-remote `remotes.<name>.auto-track-bookmarks` matchers, as `jj git fetch` imports.
    pub fn import_options(&self) -> Result<jj_lib::git::GitImportOptions> {
        let git = jj_lib::git::GitSettings::from_settings(&self.settings)?;
        let remotes = self.settings.remote_settings()?;
        Ok(jj_lib::git::GitImportOptions {
            abandon_unreachable_commits: git.abandon_unreachable_commits,
            record_synthetic_predecessors: git.record_synthetic_predecessors,
            remote_auto_track_bookmarks:
                jj_cli::revset_util::parse_remote_auto_track_bookmarks_map(&Ui::null(), &remotes)
                    .map_err(cli_error)?,
        })
    }

    pub fn subprocess_options(&self) -> Result<jj_lib::git::GitSubprocessOptions> {
        Ok(jj_lib::git::GitSettings::from_settings(&self.settings)?.to_subprocess_options())
    }
}

/// The jj settings that change what jj-fork plans, snapshots, writes, fetches, or pushes, in a
/// canonical form for fingerprinting, resolved for each command context jj-fork runs in.
/// Secrets (signing keys, credentials) are recorded only as presence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EffectiveSettings {
    pub fork: ContextSettings,
    pub fetch: ContextSettings,
    pub push: ContextSettings,
}

impl EffectiveSettings {
    /// Resolves the configuration of the workspace at `root` for every context. `fork` is the
    /// already-resolved planning configuration.
    pub fn load(
        root: &Path,
        fork: &UserSettings,
        ignore_files: &[PathBuf],
    ) -> Result<EffectiveSettings> {
        let other = |context| -> Result<ContextSettings> {
            let settings = UserSettings::from_config(load(root, context)?)?;
            ContextSettings::new(&settings, ignore_files)
        };
        Ok(EffectiveSettings {
            fork: ContextSettings::new(fork, ignore_files)?,
            fetch: other(CommandContext::Fetch)?,
            push: other(CommandContext::Push)?,
        })
    }
}

/// The policy-relevant jj settings of one command context.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ContextSettings {
    pub user_name: String,
    pub user_email: String,
    pub signing_behavior: String,
    pub signing_backend: String,
    pub signing_key_set: bool,
    pub sign_on_push: bool,
    pub private_commits: String,
    pub git_executable: PathBuf,
    pub write_change_id_header: bool,
    pub abandon_unreachable_commits: bool,
    pub merge_hunk_level: String,
    pub merge_same_change: String,
    pub snapshot_auto_track: String,
    pub snapshot_max_new_file_size: String,
    /// Git excludes files jj reads while snapshotting, with the SHA-256 of their contents
    /// (`None` when absent).
    pub ignore_files: BTreeMap<PathBuf, Option<String>>,
    pub revset_aliases: BTreeMap<String, String>,
    pub fileset_aliases: BTreeMap<String, String>,
    pub remotes: BTreeMap<String, String>,
}

impl ContextSettings {
    pub fn new(settings: &UserSettings, ignore_files: &[PathBuf]) -> Result<ContextSettings> {
        let string = |name: &'static str| -> Result<String> {
            Ok(settings
                .get_value(name)
                .optional()?
                .map(|v| v.to_string().trim().to_string())
                .unwrap_or_default())
        };
        let table = |name: &'static str| -> Result<BTreeMap<String, String>> {
            Ok(settings
                .get_table(name)
                .optional()?
                .map(|table| {
                    table
                        .iter()
                        .map(|(k, v)| (k.to_string(), v.to_string().trim().to_string()))
                        .collect()
                })
                .unwrap_or_default())
        };
        let git = jj_lib::git::GitSettings::from_settings(settings)?;
        let mut files = BTreeMap::new();
        for path in ignore_files {
            let digest = std::fs::read(path)
                .ok()
                .map(|bytes| hex(&Sha256::digest(bytes)));
            files.insert(path.clone(), digest);
        }
        Ok(ContextSettings {
            user_name: settings.user_name().to_string(),
            user_email: settings.user_email().to_string(),
            signing_behavior: string("signing.behavior")?,
            signing_backend: string("signing.backend")?,
            signing_key_set: settings.get_value("signing.key").optional()?.is_some(),
            sign_on_push: settings.get_bool("git.sign-on-push")?,
            private_commits: string("git.private-commits")?,
            git_executable: git.executable_path,
            write_change_id_header: git.write_change_id_header,
            abandon_unreachable_commits: git.abandon_unreachable_commits,
            merge_hunk_level: string("merge.hunk-level")?,
            merge_same_change: string("merge.same-change")?,
            snapshot_auto_track: string("snapshot.auto-track")?,
            snapshot_max_new_file_size: string("snapshot.max-new-file-size")?,
            ignore_files: files,
            revset_aliases: table("revset-aliases")?,
            fileset_aliases: table("fileset-aliases")?,
            remotes: table("remotes")?,
        })
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
