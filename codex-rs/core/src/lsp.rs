use crate::file_watcher::FileWatcher;
use crate::file_watcher::WatchRegistration;
use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use serde::Deserialize;
use serde_json::Value;
use serde_json::json;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::collections::HashSet;
use std::env;
use std::fs;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::process::Child;
use std::process::ChildStderr;
use std::process::ChildStdin;
use std::process::ChildStdout;
use std::process::Command;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::Weak;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::sync::mpsc::Receiver;
use std::sync::mpsc::RecvTimeoutError;
use std::thread;
use std::time::Duration;
use std::time::Instant;
use tracing::warn;
use url::Url;

const DEFAULT_TIMEOUT_MS: u64 = 15_000;
const DEFAULT_LIMIT: usize = 50;
const DIAGNOSTICS_QUIET_PERIOD_MS: u64 = 300;
const DEFAULT_SESSION_IDLE_TIMEOUT_MS: u64 = 60 * 1000;
const DEFAULT_MAX_PERSISTENT_SESSIONS: usize = 2;
const DEFAULT_MAX_OPEN_DOCUMENTS: usize = 8;
const SESSION_REAPER_INTERVAL_MS: u64 = 15 * 1000;
const WORK_DONE_PROGRESS_QUIET_PERIOD_MS: u64 = 250;
const REQUEST_PROGRESS_GRACE_PERIOD_MS: u64 = 2_000;
const MAX_PROGRESS_TIMEOUT_EXTENSION_MS: u64 = 45_000;
const EMPTY_RESULT_RETRY_MAX_WAIT_MS: u64 = 8_000;
const MIN_EMPTY_RESULT_RETRY_TIMEOUT_MS: u64 = 250;
const URI_KEYS: [&str; 4] = ["uri", "targetUri", "oldUri", "newUri"];
const DEFAULT_CODE_ACTION_KINDS: [&str; 7] = [
    "quickfix",
    "refactor",
    "refactor.extract",
    "refactor.inline",
    "refactor.rewrite",
    "source",
    "source.organizeImports",
];
const WORKSPACE_PROVIDER_DIR_RELATIVE: [&str; 2] = [".codex", "lsp-providers"];
const USER_PROVIDER_DIR_NAME: &str = "lsp-providers";
const PROVIDER_DIRS_ENV_VAR: &str = "CODEX_LSP_PROVIDER_DIRS";
const LSP_DISABLE_PERSISTENT_ENV_VAR: &str = "CODEX_LSP_DISABLE_PERSISTENT";
const LSP_IDLE_TIMEOUT_ENV_VAR: &str = "CODEX_LSP_IDLE_TIMEOUT_MS";
const LSP_MAX_SESSIONS_ENV_VAR: &str = "CODEX_LSP_MAX_SESSIONS";
const LSP_MAX_OPEN_DOCUMENTS_ENV_VAR: &str = "CODEX_LSP_MAX_OPEN_DOCUMENTS";
const LSP_STDERR_TAIL_LIMIT: usize = 8 * 1024;

#[derive(Debug, Clone, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LspAction {
    Auto,
    Providers,
    Diagnostics,
    Definition,
    References,
    Hover,
    DocumentSymbols,
    WorkspaceSymbols,
    Rename,
    Completion,
    SignatureHelp,
    CodeActions,
}

impl LspAction {
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Providers => "providers",
            Self::Diagnostics => "diagnostics",
            Self::Definition => "definition",
            Self::References => "references",
            Self::Hover => "hover",
            Self::DocumentSymbols => "document_symbols",
            Self::WorkspaceSymbols => "workspace_symbols",
            Self::Rename => "rename",
            Self::Completion => "completion",
            Self::SignatureHelp => "signature_help",
            Self::CodeActions => "code_actions",
        }
    }

    fn requires_file(&self) -> bool {
        !matches!(self, Self::Auto | Self::Providers | Self::WorkspaceSymbols)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct LspToolRequest {
    pub(crate) action: LspAction,
    pub(crate) path: Option<String>,
    pub(crate) language: Option<String>,
    pub(crate) goal: Option<String>,
    pub(crate) line: Option<u32>,
    pub(crate) column: Option<u32>,
    pub(crate) end_line: Option<u32>,
    pub(crate) end_column: Option<u32>,
    pub(crate) query: Option<String>,
    pub(crate) new_name: Option<String>,
    pub(crate) include_declaration: Option<bool>,
    pub(crate) limit: Option<usize>,
    pub(crate) trigger_character: Option<String>,
    #[serde(default)]
    pub(crate) only: Vec<String>,
    #[serde(default)]
    pub(crate) apply: bool,
    pub(crate) timeout_ms: Option<u64>,
}

#[derive(Debug, Clone)]
struct LspProvider {
    id: String,
    source: String,
    aliases: Vec<String>,
    file_extensions: Vec<String>,
    workspace_markers: Vec<String>,
    default_command: String,
    default_args: Vec<String>,
    command_env_var: Option<String>,
    args_env_var: Option<String>,
    default_language_id: String,
    extension_language_ids: HashMap<String, String>,
}

#[derive(Debug, Clone)]
pub struct LspProviderStatus {
    pub id: String,
    pub source: String,
    pub file_extensions: Vec<String>,
    pub command: String,
    pub command_available: bool,
    pub command_path: Option<PathBuf>,
    pub status: String,
    pub error: Option<String>,
}

impl LspProvider {
    #[allow(clippy::too_many_arguments)]
    fn builtin(
        id: &str,
        aliases: &[&str],
        file_extensions: &[&str],
        workspace_markers: &[&str],
        default_command: &str,
        default_args: &[&str],
        command_env_var: &str,
        args_env_var: &str,
        default_language_id: &str,
        extension_language_ids: &[(&str, &str)],
    ) -> Self {
        Self {
            id: id.to_string(),
            source: "builtin".to_string(),
            aliases: aliases
                .iter()
                .map(std::string::ToString::to_string)
                .collect(),
            file_extensions: file_extensions
                .iter()
                .map(|value| normalize_extension(value))
                .collect(),
            workspace_markers: workspace_markers
                .iter()
                .map(std::string::ToString::to_string)
                .collect(),
            default_command: default_command.to_string(),
            default_args: default_args
                .iter()
                .map(std::string::ToString::to_string)
                .collect(),
            command_env_var: Some(command_env_var.to_string()),
            args_env_var: Some(args_env_var.to_string()),
            default_language_id: default_language_id.to_string(),
            extension_language_ids: extension_language_ids
                .iter()
                .map(|(key, value)| (normalize_extension(key), value.to_string()))
                .collect(),
        }
    }

    fn from_plugin(plugin: LspProviderPlugin, source: String) -> Result<Self> {
        if plugin.id.trim().is_empty() {
            bail!("provider id must not be empty");
        }
        if plugin.command.trim().is_empty() {
            bail!("provider command must not be empty for {}", plugin.id);
        }

        let default_language_id = plugin.language_id.unwrap_or_else(|| plugin.id.clone());

        Ok(Self {
            id: plugin.id,
            source,
            aliases: plugin.aliases,
            file_extensions: plugin
                .file_extensions
                .into_iter()
                .map(|value| normalize_extension(&value))
                .collect(),
            workspace_markers: plugin.workspace_markers,
            default_command: plugin.command,
            default_args: plugin.args,
            command_env_var: plugin.command_env_var,
            args_env_var: plugin.args_env_var,
            default_language_id,
            extension_language_ids: plugin
                .extension_language_ids
                .into_iter()
                .map(|(key, value)| (normalize_extension(&key), value))
                .collect(),
        })
    }

    fn matches_language_hint(&self, language: &str) -> bool {
        let normalized = language.trim().to_ascii_lowercase();
        self.id.eq_ignore_ascii_case(&normalized)
            || self
                .aliases
                .iter()
                .any(|alias| alias.eq_ignore_ascii_case(&normalized))
    }

    fn supports_path(&self, path: &Path) -> bool {
        path.extension()
            .map(|value| normalize_extension(&value.to_string_lossy()))
            .is_some_and(|extension| {
                self.file_extensions
                    .iter()
                    .any(|candidate| candidate == &extension)
            })
    }

    fn language_id_for_path(&self, path: &Path) -> String {
        path.extension()
            .map(|value| normalize_extension(&value.to_string_lossy()))
            .and_then(|extension| self.extension_language_ids.get(&extension).cloned())
            .unwrap_or_else(|| self.default_language_id.clone())
    }

    fn resolved_command(&self) -> String {
        self.command_env_var
            .as_deref()
            .and_then(|key| env::var(key).ok())
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| self.default_command.clone())
    }

    fn resolved_args(&self) -> Vec<String> {
        self.args_env_var
            .as_deref()
            .and_then(|key| env::var(key).ok())
            .and_then(|value| shlex::split(&value))
            .unwrap_or_else(|| self.default_args.clone())
    }

    fn resolve_server_config(&self) -> Result<LspServerConfig> {
        let command = self.resolved_command();
        if command.trim().is_empty() {
            bail!("language server command is empty for provider {}", self.id);
        }
        Ok(LspServerConfig {
            command,
            args: self.resolved_args(),
        })
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
struct LspProviderPlugin {
    id: String,
    #[serde(default)]
    aliases: Vec<String>,
    #[serde(default, alias = "extensions", alias = "fileExtensions")]
    file_extensions: Vec<String>,
    #[serde(default, alias = "workspaceMarkers")]
    workspace_markers: Vec<String>,
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default, alias = "languageId")]
    language_id: Option<String>,
    #[serde(default, alias = "extensionLanguageIds")]
    extension_language_ids: HashMap<String, String>,
    #[serde(default, alias = "commandEnvVar")]
    command_env_var: Option<String>,
    #[serde(default, alias = "argsEnvVar")]
    args_env_var: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
#[allow(clippy::large_enum_variant)]
enum LspProviderPluginFile {
    Single(LspProviderPlugin),
    Many { providers: Vec<LspProviderPlugin> },
}

#[derive(Debug, Clone, Eq, PartialEq)]
struct LspServerConfig {
    command: String,
    args: Vec<String>,
}

#[derive(Debug, Clone, Copy)]
struct TextDocumentSyncSupport {
    open_close: bool,
    supports_did_change: bool,
}

impl Default for TextDocumentSyncSupport {
    fn default() -> Self {
        Self {
            open_close: true,
            supports_did_change: true,
        }
    }
}

#[derive(Debug, Clone)]
struct OpenDocumentState {
    language_id: String,
    text: String,
    version: i32,
    last_accessed_at: Instant,
}

#[derive(Debug, Clone)]
struct WorkspaceTextDocumentBatch {
    uri: String,
    path: PathBuf,
    edits: Vec<Value>,
}

#[derive(Debug, Clone)]
enum WorkspaceEditOperation {
    TextDocument(WorkspaceTextDocumentBatch),
    CreateResource {
        uri: String,
        overwrite: bool,
        ignore_if_exists: bool,
    },
    RenameResource {
        old_uri: String,
        new_uri: String,
        overwrite: bool,
        ignore_if_exists: bool,
    },
    DeleteResource {
        uri: String,
        recursive: bool,
        ignore_if_not_exists: bool,
    },
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum LspWatchedFileChangeKind {
    Created,
    Changed,
    Deleted,
}

impl LspWatchedFileChangeKind {
    fn as_lsp_code(self) -> i64 {
        match self {
            Self::Created => 1,
            Self::Changed => 2,
            Self::Deleted => 3,
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct LspWatchedFileChange {
    pub path: PathBuf,
    pub kind: LspWatchedFileChangeKind,
}

#[derive(Debug, Clone, Eq, Hash, PartialEq)]
struct LspSessionKey {
    provider_id: String,
    workspace_root: PathBuf,
}

struct LspManagedSession {
    session: Arc<Mutex<LspServerSession>>,
    last_used_at: Instant,
    active_requests: usize,
}

#[derive(Default)]
struct LspSessionManagerInner {
    sessions: HashMap<LspSessionKey, LspManagedSession>,
}

#[derive(Debug, Clone)]
struct LspSessionManagerConfig {
    persistent_enabled: bool,
    idle_timeout: Duration,
    max_sessions: usize,
    max_open_documents: usize,
}

impl Default for LspSessionManagerConfig {
    fn default() -> Self {
        Self {
            persistent_enabled: !env_var_truthy(LSP_DISABLE_PERSISTENT_ENV_VAR),
            idle_timeout: Duration::from_millis(
                env::var(LSP_IDLE_TIMEOUT_ENV_VAR)
                    .ok()
                    .and_then(|value| value.parse::<u64>().ok())
                    .filter(|value| *value > 0)
                    .unwrap_or(DEFAULT_SESSION_IDLE_TIMEOUT_MS),
            ),
            max_sessions: env::var(LSP_MAX_SESSIONS_ENV_VAR)
                .ok()
                .and_then(|value| value.parse::<usize>().ok())
                .filter(|value| *value > 0)
                .unwrap_or(DEFAULT_MAX_PERSISTENT_SESSIONS),
            max_open_documents: env::var(LSP_MAX_OPEN_DOCUMENTS_ENV_VAR)
                .ok()
                .and_then(|value| value.parse::<usize>().ok())
                .filter(|value| *value > 0)
                .unwrap_or(DEFAULT_MAX_OPEN_DOCUMENTS),
        }
    }
}

pub(crate) struct LspSessionManager {
    config: LspSessionManagerConfig,
    inner: Arc<Mutex<LspSessionManagerInner>>,
    workspace_watch_registrations: Mutex<HashMap<PathBuf, WatchRegistration>>,
    reaper_started: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
}

impl Default for LspSessionManager {
    fn default() -> Self {
        Self::new()
    }
}

impl LspSessionManager {
    pub(crate) fn new() -> Self {
        Self {
            config: LspSessionManagerConfig::default(),
            inner: Arc::new(Mutex::new(LspSessionManagerInner::default())),
            workspace_watch_registrations: Mutex::new(HashMap::new()),
            reaper_started: Arc::new(AtomicBool::new(false)),
            shutdown: Arc::new(AtomicBool::new(false)),
        }
    }

    fn persistent_enabled(&self) -> bool {
        self.config.persistent_enabled
    }

    fn ensure_idle_reaper(&self) {
        if !self.config.persistent_enabled {
            return;
        }
        if self
            .reaper_started
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        self.spawn_idle_reaper();
    }

    fn spawn_idle_reaper(&self) {
        let inner = Arc::downgrade(&self.inner);
        let shutdown = Arc::clone(&self.shutdown);
        let idle_timeout = self.config.idle_timeout;
        thread::spawn(move || {
            loop {
                if shutdown.load(Ordering::Relaxed) {
                    break;
                }
                thread::sleep(Duration::from_millis(SESSION_REAPER_INTERVAL_MS));
                if shutdown.load(Ordering::Relaxed) {
                    break;
                }

                let Some(inner) = Weak::upgrade(&inner) else {
                    break;
                };
                let sessions_to_shutdown = if let Ok(mut inner) = inner.lock() {
                    LspSessionManager::reap_idle_sessions(&mut inner, Instant::now(), idle_timeout)
                } else {
                    Vec::new()
                };
                LspSessionManager::shutdown_session_list(sessions_to_shutdown);
            }
        });
    }
}

impl Drop for LspSessionManager {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }
}

struct LspServerSession {
    provider_id: String,
    workspace_root: PathBuf,
    server_config: LspServerConfig,
    workspace_settings: Value,
    transport: LspTransport,
}

#[derive(Debug)]
enum ReaderEvent {
    Message(Value),
    Error(String),
    Closed,
}

struct LspTransport {
    child: Child,
    stdin: ChildStdin,
    events: Receiver<ReaderEvent>,
    stderr_tail: Arc<Mutex<Vec<u8>>>,
    workspace_root: PathBuf,
    workspace_settings: Value,
    next_id: i64,
    text_document_sync: TextDocumentSyncSupport,
    supports_prepare_rename: bool,
    watched_files_registered: bool,
    diagnostics_by_uri: HashMap<String, Value>,
    open_documents: HashMap<String, OpenDocumentState>,
    work_done_progress: WorkDoneProgressState,
}

#[derive(Debug, Default)]
struct WorkDoneProgressState {
    active_tokens: HashSet<String>,
    last_activity_at: Option<Instant>,
    token_last_activity_at: HashMap<String, Instant>,
}

#[derive(Debug, Clone, Copy, Default, Eq, PartialEq)]
struct ProgressWaitOutcome {
    observed_progress: bool,
    waited: Duration,
}

impl Drop for LspTransport {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl LspTransport {
    fn spawn(
        config: &LspServerConfig,
        workspace_root: &Path,
        workspace_settings: Value,
    ) -> Result<Self> {
        let command = resolve_command_path(&config.command)
            .with_context(|| format!("language server command not found: {}", config.command))?;

        let mut command_builder = Command::new(&command);
        command_builder
            .args(&config.args)
            .current_dir(workspace_root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if should_clear_rustup_toolchain(&command) {
            command_builder.env_remove("RUSTUP_TOOLCHAIN");
        }

        let mut child = command_builder
            .spawn()
            .with_context(|| format!("failed to start language server: {}", command.display()))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("failed to capture language server stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("failed to capture language server stdout"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow!("failed to capture language server stderr"))?;
        let events = spawn_reader(stdout);
        let stderr_tail = drain_stderr(stderr);

        Ok(Self {
            child,
            stdin,
            events,
            stderr_tail,
            workspace_root: workspace_root.to_path_buf(),
            workspace_settings,
            next_id: 1,
            text_document_sync: TextDocumentSyncSupport::default(),
            supports_prepare_rename: false,
            watched_files_registered: false,
            diagnostics_by_uri: HashMap::new(),
            open_documents: HashMap::new(),
            work_done_progress: WorkDoneProgressState::default(),
        })
    }

    fn initialize(&mut self, workspace_root: &Path, timeout: Duration) -> Result<()> {
        let root_uri = path_to_file_url(workspace_root)?;
        let workspace_folders = workspace_folders_value(workspace_root)?;
        let params = json!({
            "processId": std::process::id(),
            "rootUri": root_uri,
            "workspaceFolders": workspace_folders,
            "capabilities": {
                "workspace": {
                    "applyEdit": true,
                    "didChangeWatchedFiles": {
                        "dynamicRegistration": true
                    },
                    "workspaceFolders": true,
                    "configuration": true
                },
                "window": {
                    "workDoneProgress": true
                },
                "textDocument": {
                    "publishDiagnostics": {
                        "relatedInformation": true,
                        "versionSupport": true
                    },
                    "codeAction": {
                        "dynamicRegistration": false,
                        "codeActionLiteralSupport": {
                            "codeActionKind": {
                                "valueSet": DEFAULT_CODE_ACTION_KINDS
                            }
                        }
                    },
                    "completion": {
                        "dynamicRegistration": false,
                        "completionItem": {
                            "snippetSupport": false,
                            "documentationFormat": ["markdown", "plaintext"]
                        }
                    },
                    "hover": {
                        "contentFormat": ["markdown", "plaintext"]
                    },
                    "signatureHelp": {
                        "signatureInformation": {
                            "documentationFormat": ["markdown", "plaintext"],
                            "parameterInformation": {
                                "labelOffsetSupport": true
                            }
                        }
                    },
                    "definition": {
                        "linkSupport": true
                    },
                    "references": {},
                    "rename": {
                        "prepareSupport": true
                    },
                    "documentSymbol": {
                        "hierarchicalDocumentSymbolSupport": true
                    }
                }
            },
            "clientInfo": {
                "name": "codex-cli"
            }
        });

        let initialize_result = self.send_request("initialize", params, timeout)?;
        self.text_document_sync = text_document_sync_support(&initialize_result);
        self.supports_prepare_rename = server_supports_prepare_rename(&initialize_result);
        self.send_notification("initialized", json!({}))?;
        Ok(())
    }

    fn sync_document(&mut self, path: &Path, language_id: &str) -> Result<String> {
        let uri = path_to_file_url(path)?;
        let text = String::from_utf8_lossy(&fs::read(path)?).into_owned();
        let now = Instant::now();
        let existing = self.open_documents.get(&uri).cloned();

        match existing {
            Some(document) if document.language_id == language_id && document.text == text => {
                if let Some(current) = self.open_documents.get_mut(&uri) {
                    current.last_accessed_at = now;
                }
            }
            Some(document) => {
                let should_reopen = document.language_id != language_id
                    || !self.text_document_sync.supports_did_change;
                if should_reopen {
                    if self.text_document_sync.open_close {
                        let _ = self.send_notification(
                            "textDocument/didClose",
                            json!({
                                "textDocument": {
                                    "uri": uri
                                }
                            }),
                        );
                    }
                    self.send_notification(
                        "textDocument/didOpen",
                        json!({
                            "textDocument": {
                                "uri": uri,
                                "languageId": language_id,
                                "version": 1,
                                "text": text
                            }
                        }),
                    )?;
                    self.open_documents.insert(
                        uri.clone(),
                        OpenDocumentState {
                            language_id: language_id.to_string(),
                            text,
                            version: 1,
                            last_accessed_at: now,
                        },
                    );
                } else {
                    let next_version = document.version.saturating_add(1);
                    self.send_notification(
                        "textDocument/didChange",
                        json!({
                            "textDocument": {
                                "uri": uri,
                                "version": next_version
                            },
                            "contentChanges": [{
                                "text": text
                            }]
                        }),
                    )?;
                    self.open_documents.insert(
                        uri.clone(),
                        OpenDocumentState {
                            language_id: language_id.to_string(),
                            text,
                            version: next_version,
                            last_accessed_at: now,
                        },
                    );
                    self.diagnostics_by_uri.remove(&uri);
                }
            }
            None => {
                self.send_notification(
                    "textDocument/didOpen",
                    json!({
                        "textDocument": {
                            "uri": uri,
                            "languageId": language_id,
                            "version": 1,
                            "text": text
                        }
                    }),
                )?;
                self.open_documents.insert(
                    uri.clone(),
                    OpenDocumentState {
                        language_id: language_id.to_string(),
                        text,
                        version: 1,
                        last_accessed_at: now,
                    },
                );
                self.diagnostics_by_uri.remove(&uri);
            }
        }
        Ok(uri)
    }

    fn set_workspace_settings(&mut self, workspace_settings: Value) -> Result<()> {
        if self.workspace_settings == workspace_settings {
            return Ok(());
        }

        self.workspace_settings = workspace_settings.clone();
        self.send_notification(
            "workspace/didChangeConfiguration",
            json!({
                "settings": workspace_settings
            }),
        )
    }

    fn close_document(&mut self, uri: &str) -> Result<()> {
        if self.open_documents.remove(uri).is_none() {
            return Ok(());
        }

        self.diagnostics_by_uri.remove(uri);
        if self.text_document_sync.open_close {
            self.send_notification(
                "textDocument/didClose",
                json!({
                    "textDocument": {
                        "uri": uri
                    }
                }),
            )?;
        }
        Ok(())
    }

    fn evict_open_documents(&mut self, max_open_documents: usize) -> Result<()> {
        if self.open_documents.len() <= max_open_documents {
            return Ok(());
        }

        let mut uris_by_age: Vec<(String, Instant)> = self
            .open_documents
            .iter()
            .map(|(uri, document)| (uri.clone(), document.last_accessed_at))
            .collect();
        uris_by_age.sort_by_key(|(_, last_accessed_at)| *last_accessed_at);

        let evict_count = self.open_documents.len().saturating_sub(max_open_documents);
        for (uri, _) in uris_by_age.into_iter().take(evict_count) {
            self.close_document(&uri)?;
        }
        Ok(())
    }

    fn collect_diagnostics(
        &mut self,
        uri: &str,
        timeout: Duration,
        quiet_period: Duration,
    ) -> Result<Value> {
        let started_at = Instant::now();
        let mut last_seen_at: Option<Instant> = None;

        loop {
            if let Some(last_seen_at_value) = last_seen_at
                && last_seen_at_value.elapsed() >= quiet_period
            {
                break;
            }

            let elapsed = started_at.elapsed();
            if elapsed >= timeout {
                break;
            }

            let remaining_timeout = timeout.saturating_sub(elapsed);
            match self.recv_event(remaining_timeout)? {
                Some(ReaderEvent::Message(message)) => {
                    if self.handle_message(message)? && self.diagnostics_by_uri.contains_key(uri) {
                        last_seen_at = Some(Instant::now());
                    }
                }
                Some(ReaderEvent::Closed) => break,
                Some(ReaderEvent::Error(message)) => bail!(message),
                None => break,
            }
        }

        Ok(self
            .diagnostics_by_uri
            .get(uri)
            .cloned()
            .unwrap_or_else(|| Value::Array(Vec::new())))
    }

    fn wait_for_progress_quiet(
        &mut self,
        max_wait: Duration,
        quiet_period: Duration,
    ) -> Result<ProgressWaitOutcome> {
        let started_at = Instant::now();
        let mut observed_progress = !self.work_done_progress.active_tokens.is_empty();

        loop {
            if observed_progress
                && self
                    .work_done_progress
                    .last_activity_at
                    .is_none_or(|last_activity_at| last_activity_at.elapsed() >= quiet_period)
            {
                return Ok(ProgressWaitOutcome {
                    observed_progress,
                    waited: started_at.elapsed(),
                });
            }

            let elapsed = started_at.elapsed();
            if elapsed >= max_wait {
                return Ok(ProgressWaitOutcome {
                    observed_progress,
                    waited: elapsed,
                });
            }

            let remaining = max_wait.saturating_sub(elapsed);
            let wait_timeout = remaining.min(quiet_period);
            match self.recv_event(wait_timeout)? {
                Some(ReaderEvent::Message(message)) => {
                    if self.handle_message(message)? {
                        observed_progress = true;
                    }
                }
                Some(ReaderEvent::Closed) => return Err(self.closed_before_responding_error()),
                Some(ReaderEvent::Error(message)) => bail!(message),
                None => {
                    if observed_progress {
                        return Ok(ProgressWaitOutcome {
                            observed_progress,
                            waited: started_at.elapsed(),
                        });
                    }
                }
            }
        }
    }

    fn send_notification(&mut self, method: &str, params: Value) -> Result<()> {
        self.write_message(&json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        }))
    }

    fn send_request(&mut self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        let request_id = self.next_id;
        self.next_id += 1;
        let progress_token = request_supports_work_done_progress(method)
            .then(|| build_request_progress_token(method, request_id));
        let params = if let Some(token) = progress_token.as_deref() {
            attach_work_done_progress_token(params, token)
        } else {
            params
        };

        self.write_message(&json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "method": method,
            "params": params,
        }))?;

        let started_at = Instant::now();
        let mut deadline = started_at + timeout;
        let max_deadline = deadline + progress_extension_budget(timeout);

        let result = (|| -> Result<Value> {
            loop {
                let now = Instant::now();
                if now >= deadline {
                    if let Some(token) = progress_token.as_deref()
                        && let Some(next_deadline) = next_request_deadline_for_progress(
                            &self.work_done_progress,
                            token,
                            now,
                            deadline,
                            max_deadline,
                        )
                    {
                        deadline = next_deadline;
                        continue;
                    }
                    let _ = self.cancel_request(request_id);
                    break Err(anyhow!("LSP request timed out: {method}"));
                }

                let remaining_timeout = deadline.saturating_duration_since(now);
                match self.recv_event(remaining_timeout)? {
                    Some(ReaderEvent::Message(message)) => {
                        if let Some(response_id) = message.get("id")
                            && response_id.as_i64() == Some(request_id)
                        {
                            if let Some(error) = message.get("error") {
                                break Err(anyhow!("LSP request failed for {method}: {error}"));
                            }
                            break Ok(message.get("result").cloned().unwrap_or(Value::Null));
                        }
                        let _ = self.handle_message(message)?;
                    }
                    Some(ReaderEvent::Closed) => break Err(self.closed_before_responding_error()),
                    Some(ReaderEvent::Error(message)) => break Err(anyhow!(message)),
                    None => continue,
                }
            }
        })();

        self.clear_progress_token(progress_token.as_deref());
        result
    }

    fn shutdown(&mut self) {
        let uris: Vec<String> = self.open_documents.keys().cloned().collect();
        for uri in uris {
            let _ = self.close_document(&uri);
        }
        let _ = self.send_request("shutdown", Value::Null, Duration::from_secs(2));
        let _ = self.send_notification("exit", json!({}));
    }

    fn process_has_exited(&mut self) -> bool {
        self.child.try_wait().ok().flatten().is_some()
    }

    fn recv_event(&self, timeout: Duration) -> Result<Option<ReaderEvent>> {
        match self.events.recv_timeout(timeout) {
            Ok(event) => Ok(Some(event)),
            Err(RecvTimeoutError::Timeout) => Ok(None),
            Err(RecvTimeoutError::Disconnected) => Ok(Some(ReaderEvent::Closed)),
        }
    }

    fn cancel_request(&mut self, request_id: i64) -> Result<()> {
        self.send_notification(
            "$/cancelRequest",
            json!({
                "id": request_id
            }),
        )
    }

    fn clear_progress_token(&mut self, token: Option<&str>) {
        let Some(token) = token else {
            return;
        };
        self.work_done_progress.active_tokens.remove(token);
        self.work_done_progress.token_last_activity_at.remove(token);
    }

    fn note_external_file_changes(&mut self, changes: &[LspWatchedFileChange]) -> Result<()> {
        let mut lsp_changes = Vec::new();
        for change in changes {
            if !change.path.starts_with(&self.workspace_root) {
                continue;
            }

            match change.kind {
                LspWatchedFileChangeKind::Changed => {
                    self.refresh_open_document_from_disk(&change.path)?;
                }
                LspWatchedFileChangeKind::Deleted => {
                    self.drop_open_document_for_path(&change.path)?;
                }
                LspWatchedFileChangeKind::Created => {}
            }

            if self.watched_files_registered {
                lsp_changes.push(json!({
                    "uri": path_to_file_url(&change.path)?,
                    "type": change.kind.as_lsp_code(),
                }));
            }
        }

        if !lsp_changes.is_empty() {
            self.send_notification(
                "workspace/didChangeWatchedFiles",
                json!({
                    "changes": lsp_changes
                }),
            )?;
        }

        Ok(())
    }

    fn refresh_open_document_from_disk(&mut self, path: &Path) -> Result<()> {
        let uri = path_to_file_url(path)?;
        let Some(language_id) = self
            .open_documents
            .get(&uri)
            .map(|document| document.language_id.clone())
        else {
            return Ok(());
        };
        self.sync_document(path, &language_id)?;
        Ok(())
    }

    fn drop_open_document_for_path(&mut self, path: &Path) -> Result<()> {
        let uri = path_to_file_url(path)?;
        self.close_document(&uri)
    }

    fn closed_before_responding_error(&self) -> anyhow::Error {
        let stderr_tail = self.stderr_tail();
        if stderr_tail.is_empty() {
            anyhow!("language server closed before responding")
        } else {
            anyhow!("language server closed before responding; stderr tail:\n{stderr_tail}")
        }
    }

    fn stderr_tail(&self) -> String {
        self.stderr_tail
            .lock()
            .ok()
            .map(|bytes| String::from_utf8_lossy(&bytes).trim().to_string())
            .unwrap_or_default()
    }

    fn handle_message(&mut self, message: Value) -> Result<bool> {
        if message.get("method").is_some() && message.get("id").is_some() {
            self.respond_to_server_request(&message)?;
            return Ok(false);
        }

        if let Some(method) = message.get("method").and_then(Value::as_str)
            && method == "$/progress"
        {
            record_work_done_progress(&mut self.work_done_progress, message.get("params"));
            return Ok(true);
        }

        if let Some(method) = message.get("method").and_then(Value::as_str)
            && method == "textDocument/publishDiagnostics"
        {
            let Some(params) = message.get("params") else {
                return Ok(false);
            };
            let Some(uri) = params.get("uri").and_then(Value::as_str) else {
                return Ok(false);
            };
            let diagnostics = params
                .get("diagnostics")
                .cloned()
                .unwrap_or_else(|| Value::Array(Vec::new()));
            self.diagnostics_by_uri.insert(uri.to_string(), diagnostics);
            return Ok(true);
        }

        Ok(false)
    }

    fn respond_to_server_request(&mut self, request: &Value) -> Result<()> {
        let method = request
            .get("method")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("server request missing method"))?;
        let request_id = request
            .get("id")
            .cloned()
            .ok_or_else(|| anyhow!("server request missing id"))?;
        let params = request.get("params").cloned().unwrap_or(Value::Null);

        let result = match method {
            "workspace/configuration" => {
                workspace_configuration_response(&self.workspace_settings, &params)
            }
            "client/registerCapability" => {
                self.update_capability_registration(&params, true);
                Value::Null
            }
            "client/unregisterCapability" => {
                self.update_capability_registration(&params, false);
                Value::Null
            }
            "workspace/workspaceFolders" => workspace_folders_value(&self.workspace_root)?,
            "workspace/applyEdit" => workspace_apply_edit_response(
                &params,
                &mut self.open_documents,
                &mut self.diagnostics_by_uri,
            ),
            "window/workDoneProgress/create" => Value::Null,
            "window/showDocument" => json!({ "success": false }),
            _ => Value::Null,
        };

        self.write_message(&json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "result": result,
        }))
    }

    fn update_capability_registration(&mut self, params: &Value, enabled: bool) {
        let key = if enabled {
            "registrations"
        } else {
            "unregisterations"
        };
        let Some(items) = params.get(key).and_then(Value::as_array) else {
            return;
        };

        for item in items {
            if item.get("method").and_then(Value::as_str) == Some("workspace/didChangeWatchedFiles")
            {
                self.watched_files_registered = enabled;
            }
        }
    }

    fn write_message(&mut self, value: &Value) -> Result<()> {
        let payload = serde_json::to_vec(value)?;
        write!(self.stdin, "Content-Length: {}\r\n\r\n", payload.len())?;
        self.stdin.write_all(&payload)?;
        self.stdin.flush()?;
        Ok(())
    }
}

impl LspServerSession {
    fn start(
        provider: &LspProvider,
        server_config: &LspServerConfig,
        workspace_root: &Path,
        workspace_settings: Value,
        timeout: Duration,
    ) -> Result<Self> {
        let mut transport =
            LspTransport::spawn(server_config, workspace_root, workspace_settings.clone())?;
        transport.initialize(workspace_root, timeout)?;
        transport.send_notification(
            "workspace/didChangeConfiguration",
            json!({
                "settings": workspace_settings
            }),
        )?;

        Ok(Self {
            provider_id: provider.id.clone(),
            workspace_root: workspace_root.to_path_buf(),
            server_config: server_config.clone(),
            workspace_settings,
            transport,
        })
    }

    fn refresh_workspace_settings(&mut self, workspace_settings: Value) -> Result<()> {
        if self.workspace_settings == workspace_settings {
            return Ok(());
        }

        self.transport
            .set_workspace_settings(workspace_settings.clone())?;
        self.workspace_settings = workspace_settings;
        Ok(())
    }

    fn shutdown(&mut self) {
        self.transport.shutdown();
    }
}

impl LspSessionManager {
    fn checkout_session(
        &self,
        key: &LspSessionKey,
        provider: &LspProvider,
        server_config: &LspServerConfig,
        workspace_root: &Path,
        workspace_settings: &Value,
        timeout: Duration,
    ) -> Result<Option<Arc<Mutex<LspServerSession>>>> {
        let now = Instant::now();
        let mut sessions_to_shutdown: Vec<Arc<Mutex<LspServerSession>>> = Vec::new();
        let mut should_start_reaper = false;

        let checkout_result = (|| {
            let mut inner = self
                .inner
                .lock()
                .map_err(|_| anyhow!("LSP session manager lock poisoned"))?;

            sessions_to_shutdown.extend(Self::reap_idle_sessions(
                &mut inner,
                now,
                self.config.idle_timeout,
            ));

            let should_reuse = inner
                .sessions
                .get(key)
                .and_then(|entry| entry.session.lock().ok())
                .map(|mut session| {
                    !session.transport.process_has_exited()
                        && session.server_config == *server_config
                })
                .unwrap_or(false);

            if should_reuse {
                if let Some(entry) = inner.sessions.get_mut(key) {
                    entry.active_requests += 1;
                    entry.last_used_at = now;
                    return Ok(Some(Arc::clone(&entry.session)));
                }
            } else if let Some(evicted) = inner.sessions.remove(key) {
                sessions_to_shutdown.push(evicted.session);
            }

            sessions_to_shutdown.extend(Self::evict_for_capacity(
                &mut inner,
                self.config.max_sessions,
            ));

            if inner.sessions.len() >= self.config.max_sessions {
                return Ok(None);
            }

            let session = Arc::new(Mutex::new(LspServerSession::start(
                provider,
                server_config,
                workspace_root,
                workspace_settings.clone(),
                timeout,
            )?));
            inner.sessions.insert(
                key.clone(),
                LspManagedSession {
                    session: Arc::clone(&session),
                    last_used_at: now,
                    active_requests: 1,
                },
            );
            should_start_reaper = true;
            Ok(Some(session))
        })();

        self.shutdown_sessions(sessions_to_shutdown);
        if should_start_reaper {
            self.ensure_idle_reaper();
        }
        checkout_result
    }

    #[allow(clippy::needless_collect)]
    fn reap_idle_sessions(
        inner: &mut LspSessionManagerInner,
        now: Instant,
        idle_timeout: Duration,
    ) -> Vec<Arc<Mutex<LspServerSession>>> {
        let stale_keys: Vec<LspSessionKey> = inner
            .sessions
            .iter()
            .filter(|(_, entry)| {
                entry.active_requests == 0 && now.duration_since(entry.last_used_at) >= idle_timeout
            })
            .map(|(key, _)| key.clone())
            .collect();

        stale_keys
            .into_iter()
            .filter_map(|key| inner.sessions.remove(&key).map(|entry| entry.session))
            .collect()
    }

    fn evict_for_capacity(
        inner: &mut LspSessionManagerInner,
        max_sessions: usize,
    ) -> Vec<Arc<Mutex<LspServerSession>>> {
        if inner.sessions.len() < max_sessions {
            return Vec::new();
        }

        let evictable_key = inner
            .sessions
            .iter()
            .filter(|(_, entry)| entry.active_requests == 0)
            .min_by_key(|(_, entry)| entry.last_used_at)
            .map(|(key, _)| key.clone());

        evictable_key
            .into_iter()
            .filter_map(|key| inner.sessions.remove(&key).map(|entry| entry.session))
            .collect()
    }

    fn finish_session_use(&self, key: &LspSessionKey) {
        if let Ok(mut inner) = self.inner.lock()
            && let Some(entry) = inner.sessions.get_mut(key)
        {
            entry.active_requests = entry.active_requests.saturating_sub(1);
            entry.last_used_at = Instant::now();
        }
    }

    pub(crate) fn note_external_file_changes(&self, changes: &[LspWatchedFileChange]) {
        if changes.is_empty() {
            return;
        }

        let sessions = if let Ok(mut inner) = self.inner.lock() {
            let now = Instant::now();
            inner
                .sessions
                .values_mut()
                .map(|entry| {
                    entry.last_used_at = now;
                    Arc::clone(&entry.session)
                })
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };

        for session in sessions {
            if let Ok(mut session) = session.lock() {
                let _ = session.transport.note_external_file_changes(changes);
            }
        }
    }

    pub(crate) fn ensure_workspace_watch(
        &self,
        file_watcher: &Arc<FileWatcher>,
        workspace_root: &Path,
    ) {
        let Ok(mut registrations) = self.workspace_watch_registrations.lock() else {
            return;
        };
        if registrations.contains_key(workspace_root) {
            return;
        }
        registrations.insert(
            workspace_root.to_path_buf(),
            file_watcher.register_workspace_root(workspace_root.to_path_buf()),
        );
    }

    fn discard_session(&self, key: &LspSessionKey) -> Option<Arc<Mutex<LspServerSession>>> {
        self.inner
            .lock()
            .ok()
            .and_then(|mut inner| inner.sessions.remove(key).map(|entry| entry.session))
    }

    fn shutdown_session_list(sessions: Vec<Arc<Mutex<LspServerSession>>>) {
        for session in sessions {
            if let Ok(mut session) = session.lock() {
                session.shutdown();
            }
        }
    }

    fn shutdown_sessions(&self, sessions: Vec<Arc<Mutex<LspServerSession>>>) {
        Self::shutdown_session_list(sessions);
    }

    pub(crate) fn shutdown_all(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
        let sessions = if let Ok(mut inner) = self.inner.lock() {
            inner
                .sessions
                .drain()
                .map(|(_, entry)| entry.session)
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };

        self.shutdown_sessions(sessions);
    }

    #[allow(clippy::too_many_arguments)]
    fn invoke_persistent(
        &self,
        request: &LspToolRequest,
        effective_action: &LspAction,
        provider: &LspProvider,
        server_config: &LspServerConfig,
        workspace_root: &Path,
        resolved_path: Option<&Path>,
        timeout: Duration,
        limit: usize,
    ) -> Result<String> {
        let key = LspSessionKey {
            provider_id: provider.id.clone(),
            workspace_root: workspace_root.to_path_buf(),
        };
        let workspace_settings = load_workspace_settings(workspace_root);
        let session = self.checkout_session(
            &key,
            provider,
            server_config,
            workspace_root,
            &workspace_settings,
            timeout,
        )?;
        let Some(session) = session else {
            return invoke_transient(
                request,
                effective_action,
                provider,
                server_config,
                workspace_root,
                resolved_path,
                timeout,
                limit,
                self.config.max_open_documents,
            );
        };

        let mut discard_cached_session = false;
        let result = {
            let mut session = session
                .lock()
                .map_err(|_| anyhow!("LSP session lock poisoned"))?;
            if session.workspace_root != workspace_root || session.provider_id != provider.id {
                discard_cached_session = true;
                Err(anyhow!(
                    "cached LSP session does not match requested workspace"
                ))
            } else if let Err(err) = session.refresh_workspace_settings(workspace_settings) {
                discard_cached_session = true;
                Err(err)
            } else {
                let session_server_config = session.server_config.clone();
                let request_result = run_transport_request(
                    &mut session.transport,
                    request,
                    effective_action,
                    provider,
                    &session_server_config,
                    workspace_root,
                    resolved_path,
                    limit,
                    self.config.max_open_documents,
                    timeout,
                );
                if let Err(err) = &request_result {
                    discard_cached_session = session.transport.process_has_exited()
                        || should_discard_cached_session_after_error(err);
                }
                request_result
            }
        };

        if discard_cached_session {
            if let Some(session) = self.discard_session(&key) {
                self.shutdown_sessions(vec![session]);
            }
        } else {
            self.finish_session_use(&key);
        }

        result
    }
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn invoke(request: LspToolRequest, cwd: PathBuf, codex_home: PathBuf) -> Result<String> {
    invoke_with_session_manager(request, cwd, codex_home, None, None)
}

pub(crate) fn invoke_with_session_manager(
    request: LspToolRequest,
    cwd: PathBuf,
    codex_home: PathBuf,
    session_manager: Option<Arc<LspSessionManager>>,
    file_watcher: Option<Arc<FileWatcher>>,
) -> Result<String> {
    let timeout = Duration::from_millis(request.timeout_ms.unwrap_or_else(default_timeout_ms));
    let limit = request.limit.unwrap_or(DEFAULT_LIMIT).max(1);
    let resolved_path = resolve_request_path(&cwd, request.path.as_deref())?;
    let registry_base = provider_registry_base(&cwd, resolved_path.as_deref())?;
    let providers = load_provider_registry(&registry_base, &codex_home);

    if request.action == LspAction::Providers {
        let mut result: Vec<Value> = probe_provider_status(
            &cwd,
            resolved_path.as_deref(),
            &codex_home,
        )
        .into_iter()
        .map(|provider| {
            json!({
                "id": provider.id,
                "source": provider.source,
                "file_extensions": provider.file_extensions,
                "command_available": provider.command_available,
                "command": provider.command,
                "command_path": provider.command_path.map(|path| path.display().to_string()),
                "status": provider.status,
                "error": provider.error,
            })
        })
        .collect();
        result.truncate(limit);
        return Ok(serde_json::to_string_pretty(&json!({
            "action": request.action.as_str(),
            "providers": result,
        }))?);
    }

    let effective_action = if request.action == LspAction::Auto {
        infer_auto_action(&request, resolved_path.as_deref())
    } else {
        request.action.clone()
    };

    if effective_action.requires_file() && resolved_path.is_none() {
        bail!("path is required for {}", effective_action.as_str());
    }

    let provider = resolve_provider(
        request.language.as_deref(),
        resolved_path.as_deref(),
        &providers,
    )?;
    let workspace_root = resolve_workspace_root(&cwd, resolved_path.as_deref(), &provider)?;
    let server_config = provider.resolve_server_config()?;

    if let Some(manager) = session_manager.filter(|manager| manager.persistent_enabled()) {
        if let Some(file_watcher) = file_watcher.as_ref() {
            manager.ensure_workspace_watch(file_watcher, &workspace_root);
        }
        return manager.invoke_persistent(
            &request,
            &effective_action,
            &provider,
            &server_config,
            &workspace_root,
            resolved_path.as_deref(),
            timeout,
            limit,
        );
    }

    invoke_stateless(
        &request,
        &effective_action,
        &provider,
        &server_config,
        &workspace_root,
        resolved_path.as_deref(),
        timeout,
        limit,
    )
}

#[allow(clippy::too_many_arguments)]
fn invoke_stateless(
    request: &LspToolRequest,
    effective_action: &LspAction,
    provider: &LspProvider,
    server_config: &LspServerConfig,
    workspace_root: &Path,
    resolved_path: Option<&Path>,
    timeout: Duration,
    limit: usize,
) -> Result<String> {
    invoke_transient(
        request,
        effective_action,
        provider,
        server_config,
        workspace_root,
        resolved_path,
        timeout,
        limit,
        DEFAULT_MAX_OPEN_DOCUMENTS,
    )
}

#[allow(clippy::too_many_arguments)]
fn invoke_transient(
    request: &LspToolRequest,
    effective_action: &LspAction,
    provider: &LspProvider,
    server_config: &LspServerConfig,
    workspace_root: &Path,
    resolved_path: Option<&Path>,
    timeout: Duration,
    limit: usize,
    max_open_documents: usize,
) -> Result<String> {
    let workspace_settings = load_workspace_settings(workspace_root);
    let mut transport =
        LspTransport::spawn(server_config, workspace_root, workspace_settings.clone())?;
    transport.initialize(workspace_root, timeout)?;
    transport.send_notification(
        "workspace/didChangeConfiguration",
        json!({
            "settings": workspace_settings
        }),
    )?;
    let result = run_transport_request(
        &mut transport,
        request,
        effective_action,
        provider,
        server_config,
        workspace_root,
        resolved_path,
        limit,
        max_open_documents,
        timeout,
    );
    transport.shutdown();
    result
}

#[allow(clippy::too_many_arguments)]
fn dispatch_transport_request(
    transport: &mut LspTransport,
    request: &LspToolRequest,
    effective_action: &LspAction,
    opened_document_uri: Option<&str>,
    timeout: Duration,
) -> Result<Value> {
    match effective_action {
        LspAction::Auto => unreachable!("auto action resolves before request dispatch"),
        LspAction::Providers => unreachable!("providers action returns before request dispatch"),
        LspAction::Diagnostics => transport.collect_diagnostics(
            opened_document_uri.ok_or_else(|| anyhow!("diagnostics requires an opened document"))?,
            timeout,
            Duration::from_millis(DIAGNOSTICS_QUIET_PERIOD_MS),
        ),
        LspAction::Definition => transport.send_request(
            "textDocument/definition",
            position_params(opened_document_uri, request.line, request.column)?,
            timeout,
        ),
        LspAction::References => transport.send_request(
            "textDocument/references",
            json!({
                "textDocument": {
                    "uri": opened_document_uri.ok_or_else(|| anyhow!("references requires an opened document"))?
                },
                "position": position_value(request.line, request.column)?,
                "context": {
                    "includeDeclaration": request.include_declaration.unwrap_or(true)
                }
            }),
            timeout,
        ),
        LspAction::Hover => transport.send_request(
            "textDocument/hover",
            position_params(opened_document_uri, request.line, request.column)?,
            timeout,
        ),
        LspAction::DocumentSymbols => transport.send_request(
            "textDocument/documentSymbol",
            json!({
                "textDocument": {
                    "uri": opened_document_uri.ok_or_else(|| anyhow!("document_symbols requires an opened document"))?
                }
            }),
            timeout,
        ),
        LspAction::WorkspaceSymbols => transport.send_request(
            "workspace/symbol",
            json!({
                "query": request.query.clone().unwrap_or_default()
            }),
            timeout,
        ),
        LspAction::Rename => {
            let document_uri =
                opened_document_uri.ok_or_else(|| anyhow!("rename requires an opened document"))?;
            let position = position_value(request.line, request.column)?;
            if transport.supports_prepare_rename {
                let prepare_result = transport.send_request(
                    "textDocument/prepareRename",
                    json!({
                        "textDocument": {
                            "uri": document_uri
                        },
                        "position": position,
                    }),
                    timeout,
                )?;
                if prepare_result.is_null() {
                    bail!("rename is not valid at the requested position");
                }
            }
            transport.send_request(
                "textDocument/rename",
                json!({
                    "textDocument": {
                        "uri": document_uri
                    },
                    "position": position,
                    "newName": request.new_name.clone().ok_or_else(|| anyhow!("rename requires new_name"))?
                }),
                timeout,
            )
        }
        LspAction::Completion => {
            let context = if let Some(trigger_character) = request.trigger_character.clone() {
                json!({
                    "triggerKind": 2,
                    "triggerCharacter": trigger_character,
                })
            } else {
                json!({
                    "triggerKind": 1
                })
            };
            transport.send_request(
                "textDocument/completion",
                json!({
                    "textDocument": {
                        "uri": opened_document_uri.ok_or_else(|| anyhow!("completion requires an opened document"))?
                    },
                    "position": position_value(request.line, request.column)?,
                    "context": context,
                }),
                timeout,
            )
        }
        LspAction::SignatureHelp => {
            let context = if let Some(trigger_character) = request.trigger_character.clone() {
                json!({
                    "triggerKind": 2,
                    "triggerCharacter": trigger_character,
                })
            } else {
                json!({
                    "triggerKind": 1
                })
            };
            transport.send_request(
                "textDocument/signatureHelp",
                json!({
                    "textDocument": {
                        "uri": opened_document_uri.ok_or_else(|| anyhow!("signature_help requires an opened document"))?
                    },
                    "position": position_value(request.line, request.column)?,
                    "context": context,
                }),
                timeout,
            )
        }
        LspAction::CodeActions => {
            let document_uri =
                opened_document_uri.ok_or_else(|| anyhow!("code_actions requires an opened document"))?;
            let diagnostics = transport.collect_diagnostics(
                document_uri,
                Duration::from_millis(timeout.as_millis().min(2_000) as u64),
                Duration::from_millis(DIAGNOSTICS_QUIET_PERIOD_MS),
            )?;
            let mut context = json!({
                "diagnostics": diagnostics
            });
            if !request.only.is_empty() {
                context["only"] = Value::Array(
                    request
                        .only
                        .iter()
                        .cloned()
                        .map(Value::String)
                        .collect(),
                );
            }
            transport.send_request(
                "textDocument/codeAction",
                json!({
                    "textDocument": {
                        "uri": document_uri
                    },
                    "range": range_value(
                        request.line,
                        request.column,
                        request.end_line,
                        request.end_column,
                    )?,
                    "context": context,
                }),
                timeout,
            )
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn run_transport_request(
    transport: &mut LspTransport,
    request: &LspToolRequest,
    effective_action: &LspAction,
    provider: &LspProvider,
    server_config: &LspServerConfig,
    workspace_root: &Path,
    resolved_path: Option<&Path>,
    limit: usize,
    max_open_documents: usize,
    timeout: Duration,
) -> Result<String> {
    let mut opened_document_uri: Option<String> = None;
    let mut opened_document_path: Option<PathBuf> = None;
    let mut rename_applied = false;
    if *effective_action != LspAction::WorkspaceSymbols {
        let path = resolved_path
            .ok_or_else(|| anyhow!("path is required for {}", effective_action.as_str()))?;
        if !path.is_file() {
            bail!(
                "path must point to a file for {}",
                effective_action.as_str()
            );
        }
        let language_id = provider.language_id_for_path(path);
        let uri = transport.sync_document(path, &language_id)?;
        transport.evict_open_documents(max_open_documents.max(1))?;
        opened_document_uri = Some(uri);
        opened_document_path = Some(path.to_path_buf());
    }

    let mut raw_result = dispatch_transport_request(
        transport,
        request,
        effective_action,
        opened_document_uri.as_deref(),
        timeout,
    )?;
    if should_retry_empty_result(provider, effective_action, &raw_result) {
        let retry_budget = empty_result_retry_budget(timeout);
        if retry_budget > Duration::ZERO {
            let progress_wait = transport.wait_for_progress_quiet(
                retry_budget,
                Duration::from_millis(WORK_DONE_PROGRESS_QUIET_PERIOD_MS),
            )?;
            if progress_wait.observed_progress {
                let retry_timeout = retry_budget
                    .saturating_sub(progress_wait.waited)
                    .max(Duration::from_millis(MIN_EMPTY_RESULT_RETRY_TIMEOUT_MS));
                raw_result = dispatch_transport_request(
                    transport,
                    request,
                    effective_action,
                    opened_document_uri.as_deref(),
                    retry_timeout,
                )?;
            }
        }
    }
    if *effective_action == LspAction::WorkspaceSymbols
        && is_empty_symbol_result(&raw_result)
        && let Some(path) = resolved_path
    {
        let language_id = provider.language_id_for_path(path);
        let uri = transport.sync_document(path, &language_id)?;
        transport.evict_open_documents(max_open_documents.max(1))?;
        let document_symbols = dispatch_transport_request(
            transport,
            request,
            &LspAction::DocumentSymbols,
            Some(uri.as_str()),
            timeout,
        )?;
        let fallback_result = fallback_workspace_symbols_from_document_symbols(
            &document_symbols,
            path,
            request.query.as_deref(),
            limit,
        )?;
        if !is_empty_symbol_result(&fallback_result) {
            raw_result = fallback_result;
            opened_document_path = Some(path.to_path_buf());
        }
    }
    if *effective_action == LspAction::Hover
        && raw_result.is_null()
        && let Some(document_symbols) = opened_document_uri
            .as_deref()
            .map(|uri| {
                dispatch_transport_request(
                    transport,
                    request,
                    &LspAction::DocumentSymbols,
                    Some(uri),
                    timeout,
                )
            })
            .transpose()?
        && let Some(fallback_result) =
            hover_fallback_from_document_symbols(&document_symbols, request.line, request.column)
    {
        raw_result = fallback_result;
    }
    if *effective_action == LspAction::Rename {
        let rename_preview = summarize_workspace_edit(&raw_result, limit)?;
        if request.apply && rename_preview_operation_count(&rename_preview) > 0 {
            apply_workspace_edit(
                &raw_result,
                &mut transport.open_documents,
                &mut transport.diagnostics_by_uri,
            )?;
            rename_applied = true;
        }
        raw_result = rename_preview;
    }

    render_lsp_result(
        request,
        effective_action,
        provider,
        server_config,
        workspace_root,
        opened_document_path,
        raw_result,
        limit,
        rename_applied,
    )
}

#[allow(clippy::too_many_arguments)]
fn render_lsp_result(
    request: &LspToolRequest,
    effective_action: &LspAction,
    provider: &LspProvider,
    server_config: &LspServerConfig,
    workspace_root: &Path,
    opened_document_path: Option<PathBuf>,
    mut raw_result: Value,
    limit: usize,
    rename_applied: bool,
) -> Result<String> {
    normalize_value_for_output(&mut raw_result);
    truncate_result(effective_action, &mut raw_result, limit);

    let mut payload = json!({
        "requested_action": request.action.as_str(),
        "resolved_action": effective_action.as_str(),
        "provider": provider.id,
        "workspace_root": workspace_root.display().to_string(),
        "path": opened_document_path.map(|path| path.display().to_string()),
        "server_command": server_config.command,
        "server_args": server_config.args,
        "result": raw_result,
    });
    if *effective_action == LspAction::Rename {
        payload["apply_requested"] = Value::Bool(request.apply);
        payload["applied"] = Value::Bool(rename_applied);
    }
    Ok(serde_json::to_string_pretty(&payload)?)
}

fn env_var_truthy(key: &str) -> bool {
    env::var(key)
        .ok()
        .map(|value| value.trim().to_ascii_lowercase())
        .is_some_and(|value| matches!(value.as_str(), "1" | "true" | "yes" | "on"))
}

fn should_discard_cached_session_after_error(err: &anyhow::Error) -> bool {
    let message = err.to_string().to_ascii_lowercase();
    [
        "closed before responding",
        "failed to write",
        "unexpected eof",
        "missing content-length header",
        "invalid content-length header",
        "failed to start language server",
        "timed out",
    ]
    .iter()
    .any(|pattern| message.contains(pattern))
}

fn workspace_folders_value(workspace_root: &Path) -> Result<Value> {
    let uri = path_to_file_url(workspace_root)?;
    let name = workspace_root
        .file_name()
        .and_then(|value| value.to_str())
        .map(std::string::ToString::to_string)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| workspace_root.display().to_string());
    Ok(Value::Array(vec![json!({
        "uri": uri,
        "name": name,
    })]))
}

fn workspace_apply_edit_response(
    params: &Value,
    open_documents: &mut HashMap<String, OpenDocumentState>,
    diagnostics_by_uri: &mut HashMap<String, Value>,
) -> Value {
    let Some(edit) = params.get("edit") else {
        return json!({
            "applied": false,
            "failureReason": "workspace/applyEdit request missing edit payload",
        });
    };

    match apply_workspace_edit(edit, open_documents, diagnostics_by_uri) {
        Ok(()) => json!({ "applied": true }),
        Err(err) => json!({
            "applied": false,
            "failureReason": err.to_string(),
        }),
    }
}

fn apply_workspace_edit(
    edit: &Value,
    open_documents: &mut HashMap<String, OpenDocumentState>,
    diagnostics_by_uri: &mut HashMap<String, Value>,
) -> Result<()> {
    let operations = parse_workspace_edit_operations(edit)?;
    if operations.is_empty() {
        return Ok(());
    }

    for operation in operations {
        apply_workspace_edit_operation(operation, open_documents, diagnostics_by_uri)?;
    }

    Ok(())
}

fn parse_workspace_edit_operations(edit: &Value) -> Result<Vec<WorkspaceEditOperation>> {
    let mut operations = Vec::new();

    if let Some(changes) = edit.get("changes").and_then(Value::as_object) {
        for (uri, edits) in changes {
            operations.push(WorkspaceEditOperation::TextDocument(
                WorkspaceTextDocumentBatch {
                    uri: uri.clone(),
                    path: lsp_uri_to_path(uri)?,
                    edits: edits.as_array().cloned().ok_or_else(|| {
                        anyhow!("workspace edit changes for {uri} must be an array")
                    })?,
                },
            ));
        }
    }

    if let Some(document_changes) = edit.get("documentChanges").and_then(Value::as_array) {
        for change in document_changes {
            match change.get("kind").and_then(Value::as_str) {
                Some("create") => {
                    let uri = change
                        .get("uri")
                        .and_then(Value::as_str)
                        .ok_or_else(|| anyhow!("workspace create operation missing uri"))?;
                    let options = change.get("options");
                    operations.push(WorkspaceEditOperation::CreateResource {
                        uri: uri.to_string(),
                        overwrite: workspace_operation_option(options, "overwrite"),
                        ignore_if_exists: workspace_operation_option(options, "ignoreIfExists"),
                    });
                }
                Some("rename") => {
                    let old_uri = change
                        .get("oldUri")
                        .and_then(Value::as_str)
                        .ok_or_else(|| anyhow!("workspace rename operation missing oldUri"))?;
                    let new_uri = change
                        .get("newUri")
                        .and_then(Value::as_str)
                        .ok_or_else(|| anyhow!("workspace rename operation missing newUri"))?;
                    let options = change.get("options");
                    operations.push(WorkspaceEditOperation::RenameResource {
                        old_uri: old_uri.to_string(),
                        new_uri: new_uri.to_string(),
                        overwrite: workspace_operation_option(options, "overwrite"),
                        ignore_if_exists: workspace_operation_option(options, "ignoreIfExists"),
                    });
                }
                Some("delete") => {
                    let uri = change
                        .get("uri")
                        .and_then(Value::as_str)
                        .ok_or_else(|| anyhow!("workspace delete operation missing uri"))?;
                    let options = change.get("options");
                    operations.push(WorkspaceEditOperation::DeleteResource {
                        uri: uri.to_string(),
                        recursive: workspace_operation_option(options, "recursive"),
                        ignore_if_not_exists: workspace_operation_option(
                            options,
                            "ignoreIfNotExists",
                        ),
                    });
                }
                Some(kind) => {
                    bail!("workspace/applyEdit resource operations are not supported yet: {kind}")
                }
                None => {
                    let text_document = change.get("textDocument").ok_or_else(|| {
                        anyhow!("workspace edit documentChanges entry missing textDocument")
                    })?;
                    let uri = text_document
                        .get("uri")
                        .and_then(Value::as_str)
                        .ok_or_else(|| anyhow!("workspace edit textDocument missing uri"))?;
                    let edits = change
                        .get("edits")
                        .and_then(Value::as_array)
                        .cloned()
                        .ok_or_else(|| {
                            anyhow!("workspace edit textDocument edits must be an array")
                        })?;
                    operations.push(WorkspaceEditOperation::TextDocument(
                        WorkspaceTextDocumentBatch {
                            uri: uri.to_string(),
                            path: lsp_uri_to_path(uri)?,
                            edits,
                        },
                    ));
                }
            }
        }
    }

    Ok(operations)
}

fn summarize_workspace_edit(edit: &Value, limit: usize) -> Result<Value> {
    let operations = parse_workspace_edit_operations(edit)?;
    let mut touched_paths = BTreeSet::new();
    let mut operation_summaries = Vec::with_capacity(operations.len());
    let mut text_edit_count = 0usize;
    let mut resource_operation_count = 0usize;

    for operation in operations {
        match operation {
            WorkspaceEditOperation::TextDocument(batch) => {
                let path = batch.path.display().to_string();
                touched_paths.insert(path.clone());
                text_edit_count += batch.edits.len();
                operation_summaries.push(json!({
                    "kind": "text_document",
                    "path": path,
                    "edit_count": batch.edits.len(),
                }));
            }
            WorkspaceEditOperation::CreateResource {
                uri,
                overwrite,
                ignore_if_exists,
            } => {
                let path = lsp_uri_to_path(&uri)?.display().to_string();
                touched_paths.insert(path.clone());
                resource_operation_count += 1;
                let mut summary = json!({
                    "kind": "create",
                    "path": path,
                });
                if overwrite {
                    summary["overwrite"] = Value::Bool(true);
                }
                if ignore_if_exists {
                    summary["ignore_if_exists"] = Value::Bool(true);
                }
                operation_summaries.push(summary);
            }
            WorkspaceEditOperation::RenameResource {
                old_uri,
                new_uri,
                overwrite,
                ignore_if_exists,
            } => {
                let old_path = lsp_uri_to_path(&old_uri)?.display().to_string();
                let new_path = lsp_uri_to_path(&new_uri)?.display().to_string();
                touched_paths.insert(old_path.clone());
                touched_paths.insert(new_path.clone());
                resource_operation_count += 1;
                let mut summary = json!({
                    "kind": "rename",
                    "old_path": old_path,
                    "new_path": new_path,
                });
                if overwrite {
                    summary["overwrite"] = Value::Bool(true);
                }
                if ignore_if_exists {
                    summary["ignore_if_exists"] = Value::Bool(true);
                }
                operation_summaries.push(summary);
            }
            WorkspaceEditOperation::DeleteResource {
                uri,
                recursive,
                ignore_if_not_exists,
            } => {
                let path = lsp_uri_to_path(&uri)?.display().to_string();
                touched_paths.insert(path.clone());
                resource_operation_count += 1;
                let mut summary = json!({
                    "kind": "delete",
                    "path": path,
                });
                if recursive {
                    summary["recursive"] = Value::Bool(true);
                }
                if ignore_if_not_exists {
                    summary["ignore_if_not_exists"] = Value::Bool(true);
                }
                operation_summaries.push(summary);
            }
        }
    }

    let operation_count = operation_summaries.len();
    let truncated = operation_count > limit;
    if limit > 0 {
        operation_summaries.truncate(limit);
    }
    Ok(json!({
        "kind": "workspace_edit_preview",
        "operation_count": operation_count,
        "touched_path_count": touched_paths.len(),
        "text_edit_count": text_edit_count,
        "resource_operation_count": resource_operation_count,
        "truncated": truncated,
        "operations": operation_summaries,
    }))
}

fn rename_preview_operation_count(preview: &Value) -> usize {
    preview
        .get("operation_count")
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or(0)
}

fn workspace_operation_option(options: Option<&Value>, key: &str) -> bool {
    options
        .and_then(|options| options.get(key))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn apply_workspace_edit_operation(
    operation: WorkspaceEditOperation,
    open_documents: &mut HashMap<String, OpenDocumentState>,
    diagnostics_by_uri: &mut HashMap<String, Value>,
) -> Result<()> {
    match operation {
        WorkspaceEditOperation::TextDocument(batch) => {
            let current_text = String::from_utf8_lossy(&fs::read(&batch.path)?).into_owned();
            let updated_text = apply_lsp_text_edits(&current_text, &batch.edits)?;
            if let Some(parent) = batch.path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&batch.path, &updated_text)?;

            diagnostics_by_uri.remove(&batch.uri);
            if let Some(document) = open_documents.get_mut(&batch.uri) {
                document.text = updated_text;
                document.version = document.version.saturating_add(1);
                document.last_accessed_at = Instant::now();
            }
            Ok(())
        }
        WorkspaceEditOperation::CreateResource {
            uri,
            overwrite,
            ignore_if_exists,
        } => {
            let path = lsp_uri_to_path(&uri)?;
            match fs::metadata(&path) {
                Ok(metadata) => {
                    if ignore_if_exists {
                        return Ok(());
                    }
                    if !overwrite {
                        bail!("workspace create target already exists: {}", path.display());
                    }
                    if metadata.is_dir() {
                        bail!(
                            "workspace create target is an existing directory: {}",
                            path.display()
                        );
                    }
                }
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => return Err(err.into()),
            }

            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&path, "")?;

            diagnostics_by_uri.remove(&uri);
            if let Some(document) = open_documents.get_mut(&uri) {
                document.text.clear();
                document.version = document.version.saturating_add(1);
                document.last_accessed_at = Instant::now();
            }
            Ok(())
        }
        WorkspaceEditOperation::RenameResource {
            old_uri,
            new_uri,
            overwrite,
            ignore_if_exists,
        } => {
            let old_path = lsp_uri_to_path(&old_uri)?;
            let new_path = lsp_uri_to_path(&new_uri)?;
            if old_path == new_path {
                return Ok(());
            }

            let old_metadata = fs::metadata(&old_path).with_context(|| {
                format!(
                    "workspace rename source does not exist: {}",
                    old_path.display()
                )
            })?;
            let old_is_dir = old_metadata.is_dir();

            match fs::metadata(&new_path) {
                Ok(new_metadata) => {
                    if ignore_if_exists {
                        return Ok(());
                    }
                    if !overwrite {
                        bail!(
                            "workspace rename target already exists: {}",
                            new_path.display()
                        );
                    }
                    remove_workspace_path(&new_path, new_metadata.is_dir(), true)?;
                }
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => return Err(err.into()),
            }

            if let Some(parent) = new_path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::rename(&old_path, &new_path)?;
            remap_cached_workspace_paths(
                open_documents,
                diagnostics_by_uri,
                &old_path,
                &new_path,
                old_is_dir,
            )?;
            Ok(())
        }
        WorkspaceEditOperation::DeleteResource {
            uri,
            recursive,
            ignore_if_not_exists,
        } => {
            let path = lsp_uri_to_path(&uri)?;
            let metadata = match fs::metadata(&path) {
                Ok(metadata) => metadata,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    if ignore_if_not_exists {
                        return Ok(());
                    }
                    return Err(anyhow!(
                        "workspace delete target does not exist: {}",
                        path.display()
                    ));
                }
                Err(err) => return Err(err.into()),
            };
            let is_dir = metadata.is_dir();
            remove_workspace_path(&path, is_dir, recursive)?;
            drop_cached_workspace_paths(open_documents, diagnostics_by_uri, &path, is_dir)?;
            Ok(())
        }
    }
}

fn remove_workspace_path(path: &Path, is_dir: bool, recursive: bool) -> Result<()> {
    if is_dir {
        if recursive {
            fs::remove_dir_all(path)?;
        } else {
            fs::remove_dir(path)?;
        }
    } else {
        fs::remove_file(path)?;
    }
    Ok(())
}

fn remap_cached_workspace_paths(
    open_documents: &mut HashMap<String, OpenDocumentState>,
    diagnostics_by_uri: &mut HashMap<String, Value>,
    old_path: &Path,
    new_path: &Path,
    old_is_dir: bool,
) -> Result<()> {
    let affected_uris = collect_cached_uris_for_path(open_documents.keys(), old_path, old_is_dir);
    let mut remapped_documents = Vec::new();
    for old_uri in &affected_uris {
        if let Some(document) = open_documents.remove(old_uri) {
            let cached_path = lsp_uri_to_path(old_uri)?;
            let remapped_path = remap_workspace_path(&cached_path, old_path, new_path, old_is_dir)?;
            let new_uri = path_to_file_url(&remapped_path)?;
            remapped_documents.push((new_uri, document));
        }
        diagnostics_by_uri.remove(old_uri);
    }

    for (new_uri, document) in remapped_documents {
        diagnostics_by_uri.remove(&new_uri);
        open_documents.insert(new_uri, document);
    }
    Ok(())
}

fn drop_cached_workspace_paths(
    open_documents: &mut HashMap<String, OpenDocumentState>,
    diagnostics_by_uri: &mut HashMap<String, Value>,
    path: &Path,
    is_dir: bool,
) -> Result<()> {
    let affected_uris = collect_cached_uris_for_path(open_documents.keys(), path, is_dir);
    for uri in affected_uris {
        open_documents.remove(&uri);
        diagnostics_by_uri.remove(&uri);
    }
    Ok(())
}

fn collect_cached_uris_for_path<'a>(
    uris: impl Iterator<Item = &'a String>,
    path: &Path,
    is_dir: bool,
) -> Vec<String> {
    uris.filter_map(|uri| {
        let Ok(candidate_path) = lsp_uri_to_path(uri) else {
            return None;
        };
        if path_matches_workspace_target(&candidate_path, path, is_dir) {
            Some(uri.clone())
        } else {
            None
        }
    })
    .collect()
}

fn path_matches_workspace_target(
    candidate_path: &Path,
    target_path: &Path,
    target_is_dir: bool,
) -> bool {
    if target_is_dir {
        candidate_path.starts_with(target_path)
    } else {
        candidate_path == target_path
    }
}

fn remap_workspace_path(
    path: &Path,
    old_prefix: &Path,
    new_prefix: &Path,
    old_is_dir: bool,
) -> Result<PathBuf> {
    if old_is_dir {
        let suffix = path.strip_prefix(old_prefix).with_context(|| {
            format!(
                "failed to remap workspace path {} from {}",
                path.display(),
                old_prefix.display()
            )
        })?;
        Ok(new_prefix.join(suffix))
    } else if path == old_prefix {
        Ok(new_prefix.to_path_buf())
    } else {
        bail!(
            "failed to remap workspace path {} from {}",
            path.display(),
            old_prefix.display()
        );
    }
}

fn apply_lsp_text_edits(text: &str, edits: &[Value]) -> Result<String> {
    #[derive(Debug)]
    struct ParsedTextEdit {
        start: usize,
        end: usize,
        new_text: String,
    }

    let mut parsed = Vec::with_capacity(edits.len());
    for edit in edits {
        let range = edit
            .get("range")
            .ok_or_else(|| anyhow!("workspace text edit missing range"))?;
        let start = lsp_position_to_byte_offset(
            text,
            range
                .get("start")
                .ok_or_else(|| anyhow!("workspace text edit missing start position"))?,
        )?;
        let end = lsp_position_to_byte_offset(
            text,
            range
                .get("end")
                .ok_or_else(|| anyhow!("workspace text edit missing end position"))?,
        )?;
        if start > end {
            bail!("workspace text edit has inverted range");
        }
        let new_text = edit
            .get("newText")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("workspace text edit missing newText"))?;
        parsed.push(ParsedTextEdit {
            start,
            end,
            new_text: new_text.to_string(),
        });
    }

    parsed.sort_by_key(|edit| (edit.start, edit.end));
    for window in parsed.windows(2) {
        if window[0].end > window[1].start {
            bail!("workspace/applyEdit received overlapping text edits");
        }
    }

    let mut updated = text.to_string();
    for edit in parsed.into_iter().rev() {
        updated.replace_range(edit.start..edit.end, &edit.new_text);
    }
    Ok(updated)
}

fn lsp_position_to_byte_offset(text: &str, position: &Value) -> Result<usize> {
    let line = position
        .get("line")
        .and_then(Value::as_u64)
        .ok_or_else(|| anyhow!("LSP position missing line"))?;
    let character = position
        .get("character")
        .and_then(Value::as_u64)
        .ok_or_else(|| anyhow!("LSP position missing character"))?;
    let line = usize::try_from(line).map_err(|_| anyhow!("LSP line out of range"))?;
    let character =
        usize::try_from(character).map_err(|_| anyhow!("LSP character out of range"))?;

    let line_start =
        line_start_byte_offset(text, line).ok_or_else(|| anyhow!("LSP line out of bounds"))?;
    let line_end = line_end_byte_offset(text, line_start);
    let line_text = &text[line_start..line_end];
    let character_offset = utf16_character_to_byte_offset(line_text, character)?;
    Ok(line_start + character_offset)
}

fn line_start_byte_offset(text: &str, target_line: usize) -> Option<usize> {
    if target_line == 0 {
        return Some(0);
    }

    let bytes = text.as_bytes();
    let mut line = 0usize;
    let mut index = 0usize;
    while index < bytes.len() {
        match bytes[index] {
            b'\n' => {
                line += 1;
                index += 1;
                if line == target_line {
                    return Some(index);
                }
            }
            b'\r' => {
                line += 1;
                index += 1;
                if index < bytes.len() && bytes[index] == b'\n' {
                    index += 1;
                }
                if line == target_line {
                    return Some(index);
                }
            }
            _ => {
                index += 1;
            }
        }
    }

    (line == target_line).then_some(text.len())
}

fn line_end_byte_offset(text: &str, line_start: usize) -> usize {
    let bytes = text.as_bytes();
    let mut index = line_start;
    while index < bytes.len() {
        if matches!(bytes[index], b'\n' | b'\r') {
            break;
        }
        index += 1;
    }
    index
}

fn utf16_character_to_byte_offset(line_text: &str, target_character: usize) -> Result<usize> {
    let mut utf16_offset = 0usize;
    for (byte_offset, ch) in line_text.char_indices() {
        if utf16_offset == target_character {
            return Ok(byte_offset);
        }
        utf16_offset += ch.len_utf16();
        if utf16_offset > target_character {
            bail!("LSP character offset splits a UTF-16 code point");
        }
    }

    if utf16_offset == target_character {
        Ok(line_text.len())
    } else {
        bail!("LSP character offset out of bounds")
    }
}

fn lsp_uri_to_path(uri: &str) -> Result<PathBuf> {
    if let Ok(parsed) = Url::parse(uri)
        && parsed.scheme() == "file"
    {
        return parsed
            .to_file_path()
            .map_err(|_| anyhow!("failed to convert file uri to path: {uri}"));
    }

    let path = PathBuf::from(uri);
    if path.is_absolute() {
        Ok(path)
    } else {
        bail!("unsupported non-file workspace edit uri: {uri}")
    }
}

fn server_supports_prepare_rename(initialize_result: &Value) -> bool {
    let Some(rename_provider) = initialize_result
        .get("capabilities")
        .and_then(|capabilities| capabilities.get("renameProvider"))
    else {
        return false;
    };

    match rename_provider {
        Value::Object(map) => map
            .get("prepareProvider")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        _ => false,
    }
}

fn request_supports_work_done_progress(method: &str) -> bool {
    matches!(
        method,
        "textDocument/definition"
            | "textDocument/references"
            | "textDocument/hover"
            | "textDocument/documentSymbol"
            | "workspace/symbol"
            | "textDocument/prepareRename"
            | "textDocument/rename"
            | "textDocument/completion"
            | "textDocument/signatureHelp"
            | "textDocument/codeAction"
    )
}

fn build_request_progress_token(method: &str, request_id: i64) -> String {
    format!("codex:{method}:{request_id}")
}

fn attach_work_done_progress_token(mut params: Value, token: &str) -> Value {
    if let Value::Object(map) = &mut params {
        map.insert(
            "workDoneToken".to_string(),
            Value::String(token.to_string()),
        );
    }
    params
}

fn progress_extension_budget(timeout: Duration) -> Duration {
    timeout.min(Duration::from_millis(MAX_PROGRESS_TIMEOUT_EXTENSION_MS))
}

fn next_request_deadline_for_progress(
    state: &WorkDoneProgressState,
    token: &str,
    now: Instant,
    current_deadline: Instant,
    max_deadline: Instant,
) -> Option<Instant> {
    if !state.active_tokens.contains(token) {
        return None;
    }

    let last_activity_at = state.token_last_activity_at.get(token).copied()?;
    if now.duration_since(last_activity_at)
        > Duration::from_millis(REQUEST_PROGRESS_GRACE_PERIOD_MS)
    {
        return None;
    }

    let candidate = now + Duration::from_millis(REQUEST_PROGRESS_GRACE_PERIOD_MS);
    let next_deadline = if candidate > max_deadline {
        max_deadline
    } else {
        candidate
    };

    (next_deadline > current_deadline).then_some(next_deadline)
}

fn text_document_sync_support(initialize_result: &Value) -> TextDocumentSyncSupport {
    let Some(sync_value) = initialize_result
        .get("capabilities")
        .and_then(|capabilities| capabilities.get("textDocumentSync"))
    else {
        return TextDocumentSyncSupport::default();
    };

    match sync_value {
        Value::Number(kind) => match kind.as_i64() {
            Some(0) => TextDocumentSyncSupport {
                open_close: false,
                supports_did_change: false,
            },
            Some(1 | 2) => TextDocumentSyncSupport {
                open_close: true,
                supports_did_change: true,
            },
            _ => TextDocumentSyncSupport::default(),
        },
        Value::Object(map) => TextDocumentSyncSupport {
            open_close: map
                .get("openClose")
                .and_then(Value::as_bool)
                .unwrap_or(true),
            supports_did_change: map
                .get("change")
                .and_then(Value::as_i64)
                .is_some_and(|value| value >= 1),
        },
        _ => TextDocumentSyncSupport::default(),
    }
}

fn load_workspace_settings(workspace_root: &Path) -> Value {
    let Some((settings_path, workspace_folder)) = find_workspace_settings_path(workspace_root)
    else {
        return Value::Object(serde_json::Map::new());
    };
    let content = match fs::read_to_string(&settings_path) {
        Ok(content) => content,
        Err(err) => {
            if err.kind() != std::io::ErrorKind::NotFound {
                warn!(
                    "failed to read VS Code workspace settings {}: {err}",
                    settings_path.display()
                );
            }
            return Value::Object(serde_json::Map::new());
        }
    };

    match parse_jsonc_value(&content) {
        Ok(parsed) => {
            normalize_workspace_settings(&expand_workspace_variables(&parsed, &workspace_folder))
        }
        Err(err) => {
            warn!(
                "failed to parse VS Code workspace settings {}: {err}",
                settings_path.display()
            );
            Value::Object(serde_json::Map::new())
        }
    }
}

fn find_workspace_settings_path(workspace_root: &Path) -> Option<(PathBuf, PathBuf)> {
    for ancestor in workspace_root.ancestors() {
        let settings_path = ancestor.join(".vscode").join("settings.json");
        if settings_path.is_file() {
            return Some((settings_path, ancestor.to_path_buf()));
        }
    }
    None
}

fn expand_workspace_variables(value: &Value, workspace_folder: &Path) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, value)| {
                    (
                        key.clone(),
                        expand_workspace_variables(value, workspace_folder),
                    )
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|value| expand_workspace_variables(value, workspace_folder))
                .collect(),
        ),
        Value::String(text) => {
            Value::String(expand_workspace_variables_in_string(text, workspace_folder))
        }
        _ => value.clone(),
    }
}

fn expand_workspace_variables_in_string(value: &str, workspace_folder: &Path) -> String {
    for variable in ["${workspaceFolder}", "${workspaceRoot}"] {
        if let Some(suffix) = value.strip_prefix(variable) {
            let trimmed = suffix.trim_start_matches(['/', '\\']);
            if trimmed.len() != suffix.len() {
                let normalized_suffix: String = trimmed
                    .chars()
                    .map(|ch| {
                        if matches!(ch, '/' | '\\') {
                            std::path::MAIN_SEPARATOR
                        } else {
                            ch
                        }
                    })
                    .collect();
                return workspace_folder
                    .join(normalized_suffix)
                    .display()
                    .to_string();
            }
        }
    }

    let workspace_folder_string = workspace_folder.display().to_string();
    let workspace_folder_basename = workspace_folder
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or_default();

    value
        .replace("${workspaceFolder}", &workspace_folder_string)
        .replace("${workspaceRoot}", &workspace_folder_string)
        .replace("${workspaceFolderBasename}", workspace_folder_basename)
}

fn parse_jsonc_value(content: &str) -> Result<Value> {
    let without_bom = content.trim_start_matches('\u{feff}');
    let without_comments = strip_jsonc_comments(without_bom);
    let without_trailing_commas = strip_trailing_json_commas(&without_comments);
    Ok(serde_json::from_str(&without_trailing_commas)?)
}

fn strip_jsonc_comments(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    let mut in_string = false;
    let mut escape = false;

    while let Some(ch) = chars.next() {
        if in_string {
            output.push(ch);
            if escape {
                escape = false;
                continue;
            }
            match ch {
                '\\' => escape = true,
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }

        if ch == '"' {
            in_string = true;
            output.push(ch);
            continue;
        }

        if ch == '/' {
            match chars.peek().copied() {
                Some('/') => {
                    chars.next();
                    for next in chars.by_ref() {
                        if next == '\n' {
                            output.push('\n');
                            break;
                        }
                    }
                    continue;
                }
                Some('*') => {
                    chars.next();
                    let mut previous = '\0';
                    for next in chars.by_ref() {
                        if next == '\n' {
                            output.push('\n');
                        }
                        if previous == '*' && next == '/' {
                            break;
                        }
                        previous = next;
                    }
                    continue;
                }
                _ => {}
            }
        }

        output.push(ch);
    }

    output
}

fn strip_trailing_json_commas(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let mut output = String::with_capacity(input.len());
    let mut in_string = false;
    let mut escape = false;

    for (index, ch) in chars.iter().copied().enumerate() {
        if in_string {
            output.push(ch);
            if escape {
                escape = false;
                continue;
            }
            match ch {
                '\\' => escape = true,
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }

        if ch == '"' {
            in_string = true;
            output.push(ch);
            continue;
        }

        if ch == ',' {
            let next_non_whitespace = chars[index + 1..]
                .iter()
                .copied()
                .find(|candidate| !candidate.is_whitespace());
            if matches!(next_non_whitespace, Some('}') | Some(']') | None) {
                continue;
            }
        }

        output.push(ch);
    }

    output
}

fn normalize_workspace_settings(value: &Value) -> Value {
    normalize_workspace_value(value)
}

fn normalize_workspace_value(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut normalized = serde_json::Map::new();
            for (key, entry_value) in map {
                insert_workspace_setting(
                    &mut normalized,
                    key,
                    normalize_workspace_value(entry_value),
                );
            }
            Value::Object(normalized)
        }
        Value::Array(items) => Value::Array(items.iter().map(normalize_workspace_value).collect()),
        _ => value.clone(),
    }
}

fn insert_workspace_setting(root: &mut serde_json::Map<String, Value>, key: &str, value: Value) {
    let segments: Vec<&str> = key
        .split('.')
        .filter(|segment| !segment.is_empty())
        .collect();
    if segments.is_empty() {
        return;
    }
    insert_workspace_setting_segments(root, &segments, value);
}

fn insert_workspace_setting_segments(
    root: &mut serde_json::Map<String, Value>,
    segments: &[&str],
    value: Value,
) {
    if segments.len() == 1 {
        match root.get_mut(segments[0]) {
            Some(existing) => merge_json_value(existing, value),
            None => {
                root.insert(segments[0].to_string(), value);
            }
        }
        return;
    }

    let child = root
        .entry(segments[0].to_string())
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    if !child.is_object() {
        *child = Value::Object(serde_json::Map::new());
    }

    if let Some(map) = child.as_object_mut() {
        insert_workspace_setting_segments(map, &segments[1..], value);
    }
}

fn merge_json_value(existing: &mut Value, incoming: Value) {
    match (existing, incoming) {
        (Value::Object(existing_map), Value::Object(incoming_map)) => {
            for (key, value) in incoming_map {
                match existing_map.get_mut(&key) {
                    Some(existing_value) => merge_json_value(existing_value, value),
                    None => {
                        existing_map.insert(key, value);
                    }
                }
            }
        }
        (existing_value, incoming_value) => {
            *existing_value = incoming_value;
        }
    }
}

fn workspace_configuration_response(settings: &Value, params: &Value) -> Value {
    let Some(items) = params.get("items").and_then(Value::as_array) else {
        return Value::Array(Vec::new());
    };

    Value::Array(
        items
            .iter()
            .map(|item| {
                let Some(section) = item
                    .get("section")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|section| !section.is_empty())
                else {
                    return settings.clone();
                };
                lookup_workspace_setting(settings, section).unwrap_or(Value::Null)
            })
            .collect(),
    )
}

fn lookup_workspace_setting(settings: &Value, section: &str) -> Option<Value> {
    let mut current = settings;
    for segment in section.split('.').filter(|segment| !segment.is_empty()) {
        current = current.get(segment)?;
    }
    Some(current.clone())
}

fn default_timeout_ms() -> u64 {
    env::var("CODEX_LSP_TIMEOUT_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_TIMEOUT_MS)
}

fn normalize_extension(value: &str) -> String {
    value.trim().trim_start_matches('.').to_ascii_lowercase()
}

fn progress_token_key(token: Option<&Value>) -> Option<String> {
    match token {
        Some(Value::String(value)) => Some(value.clone()),
        Some(Value::Number(value)) => Some(value.to_string()),
        _ => None,
    }
}

fn record_work_done_progress(state: &mut WorkDoneProgressState, params: Option<&Value>) {
    let Some(params) = params else {
        return;
    };
    let Some(token) = progress_token_key(params.get("token")) else {
        return;
    };
    let Some(kind) = params
        .get("value")
        .and_then(|value| value.get("kind"))
        .and_then(Value::as_str)
    else {
        return;
    };

    let now = Instant::now();
    state.last_activity_at = Some(now);
    match kind {
        "begin" | "report" => {
            state.active_tokens.insert(token.clone());
            state.token_last_activity_at.insert(token, now);
        }
        "end" => {
            state.active_tokens.remove(&token);
            state.token_last_activity_at.remove(&token);
        }
        _ => {}
    }
}

fn should_retry_empty_result(provider: &LspProvider, action: &LspAction, result: &Value) -> bool {
    if provider.id != "rust" {
        return false;
    }

    match action {
        LspAction::Hover => result.is_null(),
        LspAction::WorkspaceSymbols => {
            result.is_null() || result.as_array().is_some_and(Vec::is_empty)
        }
        _ => false,
    }
}

fn empty_result_retry_budget(timeout: Duration) -> Duration {
    (timeout / 2).min(Duration::from_millis(EMPTY_RESULT_RETRY_MAX_WAIT_MS))
}

fn is_empty_symbol_result(result: &Value) -> bool {
    result.is_null() || result.as_array().is_some_and(Vec::is_empty)
}

fn fallback_workspace_symbols_from_document_symbols(
    document_symbols: &Value,
    path: &Path,
    query: Option<&str>,
    limit: usize,
) -> Result<Value> {
    let Some(items) = document_symbols.as_array() else {
        return Ok(Value::Array(Vec::new()));
    };
    let uri = path_to_file_url(path)?;
    let normalized_query = query.unwrap_or_default().trim().to_ascii_lowercase();
    let mut matches = Vec::new();
    let mut next_index = 0usize;
    collect_matching_document_symbols(
        items,
        &uri,
        &normalized_query,
        &mut next_index,
        &mut matches,
    );
    matches.sort_by_key(|(score, index, _)| (*score, *index));
    Ok(Value::Array(
        matches
            .into_iter()
            .take(limit)
            .map(|(_, _, value)| value)
            .collect(),
    ))
}

fn collect_matching_document_symbols(
    items: &[Value],
    uri: &str,
    query: &str,
    next_index: &mut usize,
    matches: &mut Vec<(u8, usize, Value)>,
) {
    for item in items {
        if let Some(score) = document_symbol_query_score(item, query) {
            matches.push((
                score,
                *next_index,
                json!({
                    "name": item.get("name").cloned().unwrap_or(Value::Null),
                    "kind": item.get("kind").cloned().unwrap_or(Value::Null),
                    "location": {
                        "uri": uri,
                        "range": item
                            .get("selectionRange")
                            .cloned()
                            .or_else(|| item.get("range").cloned())
                            .unwrap_or(Value::Null)
                    }
                }),
            ));
            *next_index += 1;
        }

        if let Some(children) = item.get("children").and_then(Value::as_array) {
            collect_matching_document_symbols(children, uri, query, next_index, matches);
        }
    }
}

fn document_symbol_query_score(item: &Value, query: &str) -> Option<u8> {
    if query.is_empty() {
        return Some(0);
    }

    let name = item
        .get("name")
        .and_then(Value::as_str)
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    if name == query {
        return Some(0);
    }
    if name.starts_with(query) {
        return Some(1);
    }
    if name.contains(query) {
        return Some(2);
    }

    item.get("detail")
        .and_then(Value::as_str)
        .map(str::to_ascii_lowercase)
        .filter(|detail| detail.contains(query))
        .map(|_| 3)
}

fn hover_fallback_from_document_symbols(
    document_symbols: &Value,
    line: Option<u32>,
    column: Option<u32>,
) -> Option<Value> {
    let line = i64::from(line?.checked_sub(1)?);
    let column = i64::from(column?.checked_sub(1)?);
    let items = document_symbols.as_array()?;
    let symbol = find_deepest_document_symbol_at_position(items, line, column)?;
    let name = symbol.get("name").and_then(Value::as_str)?;
    let detail = symbol
        .get("detail")
        .and_then(Value::as_str)
        .filter(|detail| !detail.is_empty());
    let value = if let Some(detail) = detail {
        format!("```text\n{name}\n{detail}\n```")
    } else {
        format!("```text\n{name}\n```")
    };

    Some(json!({
        "contents": {
            "kind": "markdown",
            "value": value
        },
        "range": symbol
            .get("selectionRange")
            .cloned()
            .or_else(|| symbol.get("range").cloned())
            .unwrap_or(Value::Null)
    }))
}

fn find_deepest_document_symbol_at_position(
    items: &[Value],
    line: i64,
    column: i64,
) -> Option<&Value> {
    for item in items {
        if !document_symbol_contains_position(item, line, column) {
            continue;
        }

        if let Some(children) = item.get("children").and_then(Value::as_array)
            && let Some(child) = find_deepest_document_symbol_at_position(children, line, column)
        {
            return Some(child);
        }

        return Some(item);
    }
    None
}

fn document_symbol_contains_position(item: &Value, line: i64, column: i64) -> bool {
    let start_line = item
        .get("range")
        .and_then(|range| range.get("start"))
        .and_then(|start| start.get("line"))
        .and_then(Value::as_i64);
    let start_character = item
        .get("range")
        .and_then(|range| range.get("start"))
        .and_then(|start| start.get("character"))
        .and_then(Value::as_i64);
    let end_line = item
        .get("range")
        .and_then(|range| range.get("end"))
        .and_then(|end| end.get("line"))
        .and_then(Value::as_i64);
    let end_character = item
        .get("range")
        .and_then(|range| range.get("end"))
        .and_then(|end| end.get("character"))
        .and_then(Value::as_i64);

    let (Some(start_line), Some(start_character), Some(end_line), Some(end_character)) =
        (start_line, start_character, end_line, end_character)
    else {
        return false;
    };

    let starts_before_or_at =
        line > start_line || (line == start_line && column >= start_character);
    let ends_after_or_at = line < end_line || (line == end_line && column <= end_character);
    starts_before_or_at && ends_after_or_at
}

fn infer_auto_action(request: &LspToolRequest, resolved_path: Option<&Path>) -> LspAction {
    if request.new_name.is_some() {
        return LspAction::Rename;
    }
    if !request.only.is_empty() || request.end_line.is_some() || request.end_column.is_some() {
        return LspAction::CodeActions;
    }
    if request.trigger_character.is_some() {
        return LspAction::Completion;
    }

    let natural_language = request
        .goal
        .as_deref()
        .or(request.query.as_deref())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();

    let matches_any = |patterns: &[&str]| -> bool {
        patterns
            .iter()
            .any(|pattern| natural_language.contains(pattern))
    };

    if matches_any(&[
        "error",
        "errors",
        "warning",
        "warnings",
        "diagnostic",
        "diagnostics",
        "broken",
        "compile",
        "problem",
    ]) {
        return LspAction::Diagnostics;
    }
    if matches_any(&[
        "reference",
        "references",
        "usage",
        "usages",
        "used",
        "what uses",
        "who calls",
        "who uses",
        "callers",
        "impact",
        "affected",
    ]) {
        return LspAction::References;
    }
    if matches_any(&[
        "defined",
        "definition",
        "declared",
        "where is",
        "jump to",
        "go to",
    ]) {
        return LspAction::Definition;
    }
    if matches_any(&[
        "hover",
        "type",
        "what is",
        "what does",
        "explain",
        "signature",
        "parameter",
        "params",
        "argument",
        "doc",
    ]) {
        return if matches_any(&["signature", "parameter", "params", "argument"]) {
            LspAction::SignatureHelp
        } else {
            LspAction::Hover
        };
    }
    if matches_any(&[
        "complete",
        "completion",
        "suggest",
        "suggestion",
        "autocomplete",
    ]) {
        return LspAction::Completion;
    }
    if matches_any(&[
        "quick fix",
        "quickfix",
        "code action",
        "fix this",
        "organize imports",
        "refactor",
    ]) {
        return LspAction::CodeActions;
    }
    if request.query.is_some() {
        return if resolved_path.is_some() {
            LspAction::DocumentSymbols
        } else {
            LspAction::WorkspaceSymbols
        };
    }
    if request.line.is_some() && request.column.is_some() {
        return LspAction::Hover;
    }
    if resolved_path.is_some() {
        return LspAction::DocumentSymbols;
    }
    LspAction::Providers
}

fn resolve_command_path(command: &str) -> Option<PathBuf> {
    let candidate = Path::new(command);
    let resolved = if candidate.is_absolute() {
        candidate.exists().then(|| candidate.to_path_buf())
    } else {
        which::which(command).ok()
    }?;

    if is_rust_analyzer_proxy(&resolved, &cargo_bin_dirs_from_env())
        && let Some(actual) = resolve_rustup_rust_analyzer()
    {
        return Some(actual);
    }

    Some(resolved)
}

fn should_clear_rustup_toolchain(command: &Path) -> bool {
    env::var_os("RUSTUP_TOOLCHAIN").is_some()
        && is_rust_analyzer_proxy(command, &cargo_bin_dirs_from_env())
}

fn is_rust_analyzer_proxy(command: &Path, cargo_bin_dirs: &[PathBuf]) -> bool {
    command
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| matches!(name, "rust-analyzer" | "rust-analyzer.exe"))
        && cargo_bin_dirs.iter().any(|dir| command.starts_with(dir))
}

fn cargo_bin_dirs_from_env() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(cargo_home) = env::var_os("CARGO_HOME") {
        dirs.push(PathBuf::from(cargo_home).join("bin"));
    }
    if let Some(user_profile) = env::var_os("USERPROFILE") {
        dirs.push(PathBuf::from(user_profile).join(".cargo").join("bin"));
    }
    if let Some(home) = env::var_os("HOME") {
        dirs.push(PathBuf::from(home).join(".cargo").join("bin"));
    }
    dirs.sort();
    dirs.dedup();
    dirs
}

fn resolve_request_path(cwd: &Path, path: Option<&str>) -> Result<Option<PathBuf>> {
    let Some(path) = path else {
        return Ok(None);
    };

    let candidate = Path::new(path);
    let resolved = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        cwd.join(candidate)
    };
    let canonical = dunce::canonicalize(&resolved)
        .with_context(|| format!("failed to resolve path: {}", resolved.display()))?;
    Ok(Some(canonical))
}

fn resolve_provider(
    language: Option<&str>,
    path: Option<&Path>,
    providers: &[LspProvider],
) -> Result<LspProvider> {
    if let Some(language) = language
        && let Some(provider) = providers
            .iter()
            .find(|provider| provider.matches_language_hint(language))
    {
        return Ok(provider.clone());
    }

    if let Some(path) = path
        && let Some(provider) = providers
            .iter()
            .find(|provider| provider.supports_path(path))
    {
        return Ok(provider.clone());
    }

    if let Some(language) = language {
        bail!("no LSP provider registered for language: {language}");
    }

    bail!(
        "unable to infer LSP provider from path; provide language explicitly or install a provider plugin"
    )
}

fn resolve_workspace_root(
    cwd: &Path,
    path: Option<&Path>,
    provider: &LspProvider,
) -> Result<PathBuf> {
    let starting_dir = match path {
        Some(path) if path.is_dir() => path.to_path_buf(),
        Some(path) => path
            .parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| anyhow!("file has no parent directory"))?,
        None => cwd.to_path_buf(),
    };
    Ok(find_workspace_root(starting_dir, provider))
}

fn provider_registry_base(cwd: &Path, path: Option<&Path>) -> Result<PathBuf> {
    match path {
        Some(path) if path.is_dir() => Ok(path.to_path_buf()),
        Some(path) => path
            .parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| anyhow!("file has no parent directory")),
        None => Ok(cwd.to_path_buf()),
    }
}

fn find_workspace_root(start: PathBuf, provider: &LspProvider) -> PathBuf {
    let mut current = start.clone();
    let root = current
        .ancestors()
        .last()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| start.clone());
    let common_markers = [".git", ".hg", ".svn"];

    loop {
        if provider
            .workspace_markers
            .iter()
            .map(String::as_str)
            .chain(common_markers.iter().copied())
            .any(|marker| current.join(marker).exists())
        {
            return current;
        }

        if current == root {
            return start;
        }

        let Some(parent) = current.parent() else {
            return start;
        };
        current = parent.to_path_buf();
    }
}

fn position_params(uri: Option<&str>, line: Option<u32>, column: Option<u32>) -> Result<Value> {
    Ok(json!({
        "textDocument": {
            "uri": uri.ok_or_else(|| anyhow!("document uri is required"))?
        },
        "position": position_value(line, column)?,
    }))
}

fn position_value(line: Option<u32>, column: Option<u32>) -> Result<Value> {
    let line = line.ok_or_else(|| anyhow!("line is required"))?;
    let column = column.ok_or_else(|| anyhow!("column is required"))?;
    if line == 0 || column == 0 {
        bail!("line and column must be 1-indexed");
    }
    Ok(json!({
        "line": line - 1,
        "character": column - 1,
    }))
}

fn range_value(
    start_line: Option<u32>,
    start_column: Option<u32>,
    end_line: Option<u32>,
    end_column: Option<u32>,
) -> Result<Value> {
    let start = position_value(start_line, start_column)?;
    let end = position_value(end_line.or(start_line), end_column.or(start_column))?;
    Ok(json!({
        "start": start,
        "end": end,
    }))
}

fn path_to_file_url(path: &Path) -> Result<String> {
    Url::from_file_path(path)
        .map(|url| url.to_string())
        .map_err(|_| anyhow!("failed to convert path to file uri: {}", path.display()))
}

fn builtin_lsp_providers() -> Vec<LspProvider> {
    vec![
        LspProvider::builtin(
            "python",
            &["py"],
            &["py", "pyi"],
            &[
                "pyproject.toml",
                "setup.py",
                "setup.cfg",
                "requirements.txt",
            ],
            "pyright-langserver",
            &["--stdio"],
            "CODEX_LSP_PYTHON_COMMAND",
            "CODEX_LSP_PYTHON_ARGS",
            "python",
            &[],
        ),
        LspProvider::builtin(
            "go",
            &["golang"],
            &["go"],
            &["go.mod", "go.work"],
            "gopls",
            &["serve"],
            "CODEX_LSP_GO_COMMAND",
            "CODEX_LSP_GO_ARGS",
            "go",
            &[],
        ),
        LspProvider::builtin(
            "rust",
            &["rs"],
            &["rs"],
            &["Cargo.toml"],
            "rust-analyzer",
            &[],
            "CODEX_LSP_RUST_COMMAND",
            "CODEX_LSP_RUST_ARGS",
            "rust",
            &[],
        ),
        LspProvider::builtin(
            "typescript",
            &["ts", "javascript", "js"],
            &["ts", "tsx", "js", "jsx", "mjs", "cjs", "mts", "cts"],
            &["package.json", "tsconfig.json", "jsconfig.json"],
            "typescript-language-server",
            &["--stdio"],
            "CODEX_LSP_TYPESCRIPT_COMMAND",
            "CODEX_LSP_TYPESCRIPT_ARGS",
            "typescript",
            &[
                ("tsx", "typescriptreact"),
                ("jsx", "javascriptreact"),
                ("js", "javascript"),
                ("mjs", "javascript"),
                ("cjs", "javascript"),
            ],
        ),
    ]
}

fn load_provider_registry(cwd: &Path, codex_home: &Path) -> Vec<LspProvider> {
    let mut providers_by_id: HashMap<String, LspProvider> = builtin_lsp_providers()
        .into_iter()
        .map(|provider| (provider.id.clone(), provider))
        .collect();

    for dir in provider_directories(cwd, codex_home) {
        for provider in load_provider_plugins_from_dir(&dir) {
            providers_by_id.insert(provider.id.clone(), provider);
        }
    }

    let mut providers: Vec<LspProvider> = providers_by_id.into_values().collect();
    providers.sort_by(|left, right| left.id.cmp(&right.id));
    providers
}

pub fn list_provider_status(cwd: &Path, codex_home: &Path) -> Vec<LspProviderStatus> {
    load_provider_registry(cwd, codex_home)
        .into_iter()
        .map(|provider| {
            let resolved_config = provider.resolve_server_config().ok();
            let resolved_command = resolved_config
                .as_ref()
                .map(|config| config.command.clone())
                .unwrap_or_else(|| provider.default_command.clone());
            let command_path = resolve_command_path(&resolved_command);
            LspProviderStatus {
                id: provider.id,
                source: provider.source,
                file_extensions: provider.file_extensions,
                command: resolved_command,
                command_available: command_path.is_some(),
                command_path: command_path.clone(),
                status: if command_path.is_some() {
                    "configured".to_string()
                } else {
                    "unavailable".to_string()
                },
                error: None,
            }
        })
        .collect()
}

pub fn probe_provider_status(
    cwd: &Path,
    path: Option<&Path>,
    codex_home: &Path,
) -> Vec<LspProviderStatus> {
    let registry_base = provider_registry_base(cwd, path).unwrap_or_else(|_| cwd.to_path_buf());
    load_provider_registry(&registry_base, codex_home)
        .into_iter()
        .map(|provider| probe_single_provider_status(&registry_base, path, &provider))
        .collect()
}

fn probe_single_provider_status(
    cwd: &Path,
    path: Option<&Path>,
    provider: &LspProvider,
) -> LspProviderStatus {
    let resolved_command = provider.resolved_command();
    let command_path = resolve_command_path(&resolved_command);
    let mut status = LspProviderStatus {
        id: provider.id.clone(),
        source: provider.source.clone(),
        file_extensions: provider.file_extensions.clone(),
        command: resolved_command,
        command_available: command_path.is_some(),
        command_path,
        status: "unavailable".to_string(),
        error: None,
    };

    if !status.command_available {
        status.error = Some("language server command not found".to_string());
        return status;
    }

    let workspace_root = resolve_workspace_root(cwd, path, provider).unwrap_or_else(|_| {
        provider_registry_base(cwd, path).unwrap_or_else(|_| cwd.to_path_buf())
    });
    let server_config = match provider.resolve_server_config() {
        Ok(config) => config,
        Err(err) => {
            status.status = "error".to_string();
            status.error = Some(err.to_string());
            return status;
        }
    };

    let workspace_settings = load_workspace_settings(&workspace_root);
    match LspTransport::spawn(&server_config, &workspace_root, workspace_settings).and_then(
        |mut transport| {
            let initialize_result =
                transport.initialize(&workspace_root, Duration::from_millis(3_000));
            transport.shutdown();
            initialize_result
        },
    ) {
        Ok(()) => {
            status.status = "ready".to_string();
        }
        Err(err) => {
            status.status = "failed".to_string();
            status.error = Some(err.to_string());
        }
    }

    status
}

fn resolve_rustup_rust_analyzer() -> Option<PathBuf> {
    let output = Command::new("rustup")
        .args(["which", "rust-analyzer"])
        .env_remove("RUSTUP_TOOLCHAIN")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if path.is_empty() {
        return None;
    }

    let candidate = PathBuf::from(path);
    candidate.exists().then_some(candidate)
}

fn provider_directories(cwd: &Path, codex_home: &Path) -> Vec<PathBuf> {
    let mut directories = vec![
        cwd.join(PathBuf::from_iter(WORKSPACE_PROVIDER_DIR_RELATIVE)),
        codex_home.join(USER_PROVIDER_DIR_NAME),
    ];

    if let Some(raw_paths) = env::var_os(PROVIDER_DIRS_ENV_VAR) {
        directories.extend(env::split_paths(&raw_paths));
    }

    dedupe_paths(directories)
}

fn dedupe_paths(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut deduped: Vec<PathBuf> = Vec::new();
    for path in paths {
        if !deduped.iter().any(|existing| existing == &path) {
            deduped.push(path);
        }
    }
    deduped
}

fn load_provider_plugins_from_dir(dir: &Path) -> Vec<LspProvider> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };

    let mut files: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok().map(|value| value.path()))
        .filter(|path| {
            path.extension()
                .map(|value| value.to_string_lossy().eq_ignore_ascii_case("json"))
                .unwrap_or(false)
        })
        .collect();
    files.sort();

    let mut providers = Vec::new();
    for path in files {
        match load_provider_plugins_from_file(&path) {
            Ok(mut loaded) => providers.append(&mut loaded),
            Err(err) => warn!(
                "failed to load LSP provider plugin {}: {err}",
                path.display()
            ),
        }
    }
    providers
}

fn load_provider_plugins_from_file(path: &Path) -> Result<Vec<LspProvider>> {
    let content = fs::read_to_string(path)
        .with_context(|| format!("failed to read provider plugin file: {}", path.display()))?;
    let parsed: LspProviderPluginFile = serde_json::from_str(&content)
        .with_context(|| format!("failed to parse provider plugin file: {}", path.display()))?;

    let plugins = match parsed {
        LspProviderPluginFile::Single(plugin) => vec![plugin],
        LspProviderPluginFile::Many { providers } => providers,
    };

    let mut loaded = Vec::new();
    for plugin in plugins {
        loaded.push(LspProvider::from_plugin(
            plugin,
            format!("plugin:{}", path.display()),
        )?);
    }
    Ok(loaded)
}

fn spawn_reader(stdout: ChildStdout) -> Receiver<ReaderEvent> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        loop {
            match read_lsp_message(&mut reader) {
                Ok(Some(message)) => {
                    if sender.send(ReaderEvent::Message(message)).is_err() {
                        break;
                    }
                }
                Ok(None) => {
                    let _ = sender.send(ReaderEvent::Closed);
                    break;
                }
                Err(err) => {
                    let _ = sender.send(ReaderEvent::Error(err.to_string()));
                    break;
                }
            }
        }
    });
    receiver
}

fn drain_stderr(stderr: ChildStderr) -> Arc<Mutex<Vec<u8>>> {
    let tail = Arc::new(Mutex::new(Vec::new()));
    let tail_writer = Arc::clone(&tail);
    thread::spawn(move || {
        let mut reader = BufReader::new(stderr);
        let mut buf = [0u8; 1024];
        loop {
            let bytes_read = match std::io::Read::read(&mut reader, &mut buf) {
                Ok(0) => break,
                Ok(bytes_read) => bytes_read,
                Err(_) => break,
            };
            let Ok(mut tail) = tail_writer.lock() else {
                break;
            };
            tail.extend_from_slice(&buf[..bytes_read]);
            if tail.len() > LSP_STDERR_TAIL_LIMIT {
                let remove = tail.len() - LSP_STDERR_TAIL_LIMIT;
                tail.drain(..remove);
            }
        }
    });
    tail
}

fn read_lsp_message(reader: &mut impl BufRead) -> Result<Option<Value>> {
    let mut content_length: Option<usize> = None;

    loop {
        let mut line = String::new();
        let bytes_read = reader.read_line(&mut line)?;
        if bytes_read == 0 {
            if content_length.is_none() {
                return Ok(None);
            }
            bail!("unexpected EOF while reading LSP headers");
        }

        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }

        let mut header_parts = trimmed.splitn(2, ':');
        let Some(header_name) = header_parts.next() else {
            continue;
        };
        let Some(header_value) = header_parts.next() else {
            continue;
        };
        if header_name.eq_ignore_ascii_case("Content-Length") {
            content_length = Some(
                header_value
                    .trim()
                    .parse::<usize>()
                    .context("invalid Content-Length header")?,
            );
        }
    }

    let content_length = content_length.ok_or_else(|| anyhow!("missing Content-Length header"))?;
    let mut body = vec![0_u8; content_length];
    std::io::Read::read_exact(reader, &mut body)?;
    Ok(Some(serde_json::from_slice(&body)?))
}

fn normalize_value_for_output(value: &mut Value) {
    match value {
        Value::Array(items) => {
            for item in items {
                normalize_value_for_output(item);
            }
        }
        Value::Object(map) => {
            for key in URI_KEYS {
                if let Some(Value::String(uri)) = map.get_mut(key)
                    && let Ok(parsed) = Url::parse(uri)
                    && parsed.scheme() == "file"
                    && let Ok(path) = parsed.to_file_path()
                {
                    *uri = path.display().to_string();
                }
            }
            if let Some(line) = map.get_mut("line")
                && let Some(raw_line) = line.as_i64()
            {
                *line = json!(raw_line + 1);
            }
            if let Some(character) = map.get_mut("character")
                && let Some(raw_character) = character.as_i64()
            {
                *character = json!(raw_character + 1);
            }
            for nested in map.values_mut() {
                normalize_value_for_output(nested);
            }
        }
        _ => {}
    }
}

fn truncate_result(action: &LspAction, value: &mut Value, limit: usize) {
    if limit == 0 {
        return;
    }

    match action {
        LspAction::Completion => {
            if let Some(items) = value.get_mut("items").and_then(Value::as_array_mut) {
                items.truncate(limit);
                return;
            }
            if let Some(items) = value.as_array_mut() {
                items.truncate(limit);
            }
        }
        LspAction::Definition
        | LspAction::References
        | LspAction::Diagnostics
        | LspAction::DocumentSymbols
        | LspAction::WorkspaceSymbols
        | LspAction::CodeActions => {
            if let Some(items) = value.as_array_mut() {
                items.truncate(limit);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::LspAction;
    use super::OpenDocumentState;
    use super::WorkDoneProgressState;
    use super::apply_lsp_text_edits;
    use super::apply_workspace_edit;
    use super::fallback_workspace_symbols_from_document_symbols;
    use super::find_workspace_root;
    use super::hover_fallback_from_document_symbols;
    use super::load_provider_registry;
    use super::load_workspace_settings;
    use super::next_request_deadline_for_progress;
    use super::normalize_value_for_output;
    use super::parse_jsonc_value;
    use super::server_supports_prepare_rename;
    use super::should_retry_empty_result;
    use super::summarize_workspace_edit;
    use super::workspace_configuration_response;
    use serde_json::Value;
    use serde_json::json;
    use std::collections::HashMap;
    use std::path::Path;
    use std::path::PathBuf;
    use std::time::Duration;
    use std::time::Instant;
    use tempfile::tempdir;

    fn symbol_range(
        start_line: i64,
        start_character: i64,
        end_line: i64,
        end_character: i64,
    ) -> Value {
        json!({
            "start": {
                "line": start_line,
                "character": start_character
            },
            "end": {
                "line": end_line,
                "character": end_character
            }
        })
    }

    #[test]
    fn normalize_value_for_output_converts_file_uris_and_positions() {
        let sample_path = std::env::temp_dir().join("example.rs");
        let uri = super::path_to_file_url(&sample_path).expect("build sample uri");
        let mut value = json!({
            "uri": uri,
            "range": {
                "start": { "line": 0, "character": 1 },
                "end": { "line": 2, "character": 3 }
            }
        });

        normalize_value_for_output(&mut value);

        assert_eq!(value["uri"], json!(sample_path.display().to_string()));
        assert_eq!(value["range"]["start"]["line"], json!(1));
        assert_eq!(value["range"]["start"]["character"], json!(2));
        assert_eq!(value["range"]["end"]["line"], json!(3));
        assert_eq!(value["range"]["end"]["character"], json!(4));
    }

    #[test]
    fn parse_jsonc_value_accepts_comments_and_trailing_commas() {
        let parsed = parse_jsonc_value(
            r#"{
                // line comment
                "rust-analyzer.check.command": "clippy",
                "gopls": {
                    /* block comment */
                    "ui.semanticTokens": true,
                },
            }"#,
        )
        .expect("parse jsonc settings");

        assert_eq!(
            parsed,
            json!({
                "rust-analyzer.check.command": "clippy",
                "gopls": {
                    "ui.semanticTokens": true
                }
            })
        );
    }

    #[test]
    fn workspace_configuration_response_returns_nested_values_for_dotted_sections() {
        let settings = json!({
            "rust-analyzer": {
                "check": {
                    "command": "clippy",
                    "features": "all"
                }
            },
            "gopls": {
                "ui": {
                    "semanticTokens": true
                }
            }
        });

        let response = workspace_configuration_response(
            &settings,
            &json!({
                "items": [
                    { "section": "rust-analyzer" },
                    { "section": "rust-analyzer.check" },
                    { "section": "gopls.ui.semanticTokens" },
                    { "section": "python.analysis" },
                    {}
                ]
            }),
        );

        assert_eq!(
            response,
            json!([
                {
                    "check": {
                        "command": "clippy",
                        "features": "all"
                    }
                },
                {
                    "command": "clippy",
                    "features": "all"
                },
                true,
                null,
                settings
            ])
        );
    }

    #[test]
    fn apply_lsp_text_edits_handles_utf16_offsets_and_descending_ranges() {
        let updated = apply_lsp_text_edits(
            "const value = \"🙂x\";\n",
            &[
                json!({
                    "range": {
                        "start": { "line": 0, "character": 17 },
                        "end": { "line": 0, "character": 18 }
                    },
                    "newText": "y"
                }),
                json!({
                    "range": {
                        "start": { "line": 0, "character": 0 },
                        "end": { "line": 0, "character": 5 }
                    },
                    "newText": "let"
                }),
            ],
        )
        .expect("apply text edits");

        assert_eq!(updated, "let value = \"🙂y\";\n");
    }

    #[test]
    fn workspace_apply_edit_updates_disk_and_open_document_cache() {
        let tempdir = tempdir().expect("create tempdir");
        let file_path = tempdir.path().join("main.ts");
        std::fs::write(&file_path, "const answer = 1;\n").expect("write file");
        let uri = super::path_to_file_url(&file_path).expect("build file uri");
        let mut changes = serde_json::Map::new();
        changes.insert(
            uri.clone(),
            json!([{
                "range": {
                    "start": { "line": 0, "character": 15 },
                    "end": { "line": 0, "character": 16 }
                },
                "newText": "2"
            }]),
        );

        let mut open_documents = HashMap::from([(
            uri.clone(),
            OpenDocumentState {
                language_id: "typescript".to_string(),
                text: "const answer = 1;\n".to_string(),
                version: 1,
                last_accessed_at: Instant::now(),
            },
        )]);
        let mut diagnostics_by_uri =
            HashMap::from([(uri.clone(), json!([{ "message": "stale" }]))]);

        apply_workspace_edit(
            &json!({
                "changes": Value::Object(changes)
            }),
            &mut open_documents,
            &mut diagnostics_by_uri,
        )
        .expect("apply workspace edit");

        assert_eq!(
            std::fs::read_to_string(&file_path).expect("read updated file"),
            "const answer = 2;\n"
        );
        assert_eq!(
            open_documents
                .get(&uri)
                .expect("open document retained")
                .text,
            "const answer = 2;\n"
        );
        assert_eq!(
            open_documents
                .get(&uri)
                .expect("open document retained")
                .version,
            2
        );
        assert!(!diagnostics_by_uri.contains_key(&uri));
    }

    #[test]
    fn workspace_apply_edit_supports_create_then_text_edit_operations() {
        let tempdir = tempdir().expect("create tempdir");
        let file_path = tempdir.path().join("nested").join("main.ts");
        let uri = super::path_to_file_url(&file_path).expect("build file uri");

        apply_workspace_edit(
            &json!({
                "documentChanges": [
                    {
                        "kind": "create",
                        "uri": uri
                    },
                    {
                        "textDocument": {
                            "uri": uri
                        },
                        "edits": [{
                            "range": {
                                "start": { "line": 0, "character": 0 },
                                "end": { "line": 0, "character": 0 }
                            },
                            "newText": "export const value = 1;\n"
                        }]
                    }
                ]
            }),
            &mut HashMap::new(),
            &mut HashMap::new(),
        )
        .expect("apply workspace create/edit");

        assert_eq!(
            std::fs::read_to_string(&file_path).expect("read created file"),
            "export const value = 1;\n"
        );
    }

    #[test]
    fn workspace_apply_edit_supports_rename_and_delete_resource_operations() {
        let tempdir = tempdir().expect("create tempdir");
        let src_dir = tempdir.path().join("src");
        std::fs::create_dir_all(&src_dir).expect("create src dir");
        let old_path = src_dir.join("main.ts");
        let new_path = tempdir.path().join("renamed").join("app.ts");
        std::fs::write(&old_path, "export const value = 1;\n").expect("write source file");
        let old_uri = super::path_to_file_url(&old_path).expect("build old uri");
        let new_uri = super::path_to_file_url(&new_path).expect("build new uri");

        let mut open_documents = HashMap::from([(
            old_uri.clone(),
            OpenDocumentState {
                language_id: "typescript".to_string(),
                text: "export const value = 1;\n".to_string(),
                version: 1,
                last_accessed_at: Instant::now(),
            },
        )]);
        let mut diagnostics_by_uri =
            HashMap::from([(old_uri.clone(), json!([{ "message": "stale" }]))]);

        apply_workspace_edit(
            &json!({
                "documentChanges": [
                    {
                        "kind": "rename",
                        "oldUri": old_uri,
                        "newUri": new_uri
                    }
                ]
            }),
            &mut open_documents,
            &mut diagnostics_by_uri,
        )
        .expect("apply workspace rename");

        assert!(!old_path.exists());
        assert_eq!(
            std::fs::read_to_string(&new_path).expect("read renamed file"),
            "export const value = 1;\n"
        );
        assert!(open_documents.contains_key(&new_uri));
        assert!(!open_documents.contains_key(&old_uri));
        assert!(!diagnostics_by_uri.contains_key(&old_uri));

        apply_workspace_edit(
            &json!({
                "documentChanges": [
                    {
                        "kind": "delete",
                        "uri": new_uri
                    }
                ]
            }),
            &mut open_documents,
            &mut diagnostics_by_uri,
        )
        .expect("apply workspace delete");

        assert!(!new_path.exists());
        assert!(!open_documents.contains_key(&new_uri));
    }

    #[test]
    fn summarize_workspace_edit_builds_preview_for_text_and_resource_operations() {
        let tempdir = tempdir().expect("create tempdir");
        let old_path = tempdir.path().join("old.ts");
        let new_path = tempdir.path().join("new.ts");
        let old_uri = super::path_to_file_url(&old_path).expect("build old uri");
        let new_uri = super::path_to_file_url(&new_path).expect("build new uri");

        let summary = summarize_workspace_edit(
            &json!({
                "documentChanges": [
                    {
                        "kind": "rename",
                        "oldUri": old_uri,
                        "newUri": new_uri
                    },
                    {
                        "textDocument": {
                            "uri": new_uri
                        },
                        "edits": [
                            {
                                "range": {
                                    "start": { "line": 0, "character": 0 },
                                    "end": { "line": 0, "character": 0 }
                                },
                                "newText": "hello"
                            },
                            {
                                "range": {
                                    "start": { "line": 0, "character": 0 },
                                    "end": { "line": 0, "character": 0 }
                                },
                                "newText": "world"
                            }
                        ]
                    }
                ]
            }),
            10,
        )
        .expect("summarize workspace edit");

        assert_eq!(summary["kind"], json!("workspace_edit_preview"));
        assert_eq!(summary["operation_count"], json!(2));
        assert_eq!(summary["text_edit_count"], json!(2));
        assert_eq!(summary["resource_operation_count"], json!(1));
        assert_eq!(summary["touched_path_count"], json!(2));
        assert_eq!(summary["operations"][0]["kind"], json!("rename"));
        assert_eq!(summary["operations"][1]["kind"], json!("text_document"));
    }

    #[test]
    fn server_supports_prepare_rename_only_when_provider_advertises_it() {
        assert!(server_supports_prepare_rename(&json!({
            "capabilities": {
                "renameProvider": {
                    "prepareProvider": true
                }
            }
        })));
        assert!(!server_supports_prepare_rename(&json!({
            "capabilities": {
                "renameProvider": true
            }
        })));
    }

    #[test]
    fn next_request_deadline_for_progress_requires_active_recent_token() {
        let now = Instant::now();
        let mut state = WorkDoneProgressState::default();
        state.active_tokens.insert("token".to_string());
        state
            .token_last_activity_at
            .insert("token".to_string(), now - Duration::from_millis(500));

        let deadline = now;
        let max_deadline = now + Duration::from_secs(5);
        let next_deadline =
            next_request_deadline_for_progress(&state, "token", now, deadline, max_deadline)
                .expect("progress should extend deadline");
        assert!(next_deadline > deadline);
        assert!(next_deadline <= max_deadline);

        state
            .token_last_activity_at
            .insert("token".to_string(), now - Duration::from_secs(5));
        assert!(
            next_request_deadline_for_progress(&state, "token", now, deadline, max_deadline)
                .is_none()
        );
    }

    #[test]
    fn load_workspace_settings_normalizes_vscode_dotted_keys() {
        let tempdir = tempdir().expect("create tempdir");
        let workspace = tempdir.path().join("workspace");
        let vscode_dir = workspace.join(".vscode");
        std::fs::create_dir_all(&vscode_dir).expect("create .vscode dir");
        std::fs::write(
            vscode_dir.join("settings.json"),
            r#"{
                "rust-analyzer.check.command": "clippy",
                "rust-analyzer.check.features": "all",
                "gopls": {
                    "ui.semanticTokens": true
                }
            }"#,
        )
        .expect("write settings.json");

        let settings = super::load_workspace_settings(&workspace);
        assert_eq!(
            settings,
            json!({
                "rust-analyzer": {
                    "check": {
                        "command": "clippy",
                        "features": "all"
                    }
                },
                "gopls": {
                    "ui": {
                        "semanticTokens": true
                    }
                }
            })
        );
    }

    #[test]
    fn load_workspace_settings_uses_ancestor_vscode_dir_and_expands_variables() {
        let tempdir = tempdir().expect("create tempdir");
        let workspace = tempdir.path().join("workspace");
        let nested = workspace.join("crates").join("demo").join("src");
        let vscode_dir = workspace.join(".vscode");
        std::fs::create_dir_all(&nested).expect("create nested workspace");
        std::fs::create_dir_all(&vscode_dir).expect("create .vscode dir");
        std::fs::write(
            vscode_dir.join("settings.json"),
            r#"{
                "rust-analyzer.linkedProjects": [
                    "${workspaceFolder}/Cargo.toml",
                    "${workspaceRoot}\\crates\\demo\\Cargo.toml"
                ],
                "rust-analyzer.cargo.targetDir": "${workspaceFolderBasename}"
            }"#,
        )
        .expect("write settings.json");

        let settings = load_workspace_settings(&nested);
        assert_eq!(
            settings,
            json!({
                "rust-analyzer": {
                    "linkedProjects": [
                        workspace.join("Cargo.toml").display().to_string(),
                        workspace
                            .join("crates")
                            .join("demo")
                            .join("Cargo.toml")
                            .display()
                            .to_string()
                    ],
                    "cargo": {
                        "targetDir": "workspace"
                    }
                }
            })
        );
    }

    #[test]
    fn should_retry_empty_result_only_for_rust_hover_and_workspace_symbols() {
        let tempdir = tempdir().expect("create tempdir");
        let providers = load_provider_registry(tempdir.path(), tempdir.path());
        let rust = providers
            .iter()
            .find(|provider| provider.id == "rust")
            .expect("rust provider");
        let python = providers
            .iter()
            .find(|provider| provider.id == "python")
            .expect("python provider");

        assert!(should_retry_empty_result(
            rust,
            &LspAction::Hover,
            &Value::Null
        ));
        assert!(should_retry_empty_result(
            rust,
            &LspAction::WorkspaceSymbols,
            &json!([])
        ));
        assert!(!should_retry_empty_result(
            rust,
            &LspAction::WorkspaceSymbols,
            &json!([{ "name": "helper" }])
        ));
        assert!(!should_retry_empty_result(
            rust,
            &LspAction::DocumentSymbols,
            &Value::Null
        ));
        assert!(!should_retry_empty_result(
            python,
            &LspAction::Hover,
            &Value::Null
        ));
    }

    #[test]
    fn workspace_symbol_fallback_ranks_exact_prefix_contains_then_detail() {
        let tempdir = tempdir().expect("create tempdir");
        let file_path = tempdir.path().join("main.rs");
        std::fs::write(&file_path, "// fallback symbol source\n").expect("write source file");
        let document_symbols = json!([
            {
                "name": "alpha_target",
                "kind": 12,
                "range": symbol_range(0, 0, 0, 12),
                "selectionRange": symbol_range(0, 0, 0, 12)
            },
            {
                "name": "container",
                "kind": 5,
                "range": symbol_range(1, 0, 5, 0),
                "selectionRange": symbol_range(1, 0, 1, 9),
                "children": [
                    {
                        "name": "target",
                        "kind": 12,
                        "range": symbol_range(2, 0, 2, 6),
                        "selectionRange": symbol_range(2, 0, 2, 6)
                    }
                ]
            },
            {
                "name": "TargetWidget",
                "kind": 12,
                "range": symbol_range(6, 0, 6, 12),
                "selectionRange": symbol_range(6, 0, 6, 12)
            },
            {
                "name": "misc",
                "kind": 12,
                "detail": "returns target metadata",
                "range": symbol_range(7, 0, 7, 4),
                "selectionRange": symbol_range(7, 0, 7, 4)
            }
        ]);

        let result = fallback_workspace_symbols_from_document_symbols(
            &document_symbols,
            &file_path,
            Some("target"),
            10,
        )
        .expect("build workspace symbol fallback");
        let names: Vec<_> = result
            .as_array()
            .expect("result array")
            .iter()
            .filter_map(|item| item.get("name").and_then(Value::as_str))
            .collect();

        assert_eq!(
            names,
            vec!["target", "TargetWidget", "alpha_target", "misc"]
        );
    }

    #[test]
    fn hover_fallback_prefers_deepest_matching_document_symbol() {
        let document_symbols = json!([
            {
                "name": "outer",
                "kind": 2,
                "detail": "mod outer",
                "range": symbol_range(0, 0, 8, 0),
                "selectionRange": symbol_range(0, 0, 0, 5),
                "children": [
                    {
                        "name": "inner",
                        "kind": 12,
                        "detail": "fn inner()",
                        "range": symbol_range(2, 0, 4, 1),
                        "selectionRange": symbol_range(2, 3, 2, 8)
                    }
                ]
            }
        ]);

        let hover = hover_fallback_from_document_symbols(&document_symbols, Some(3), Some(4))
            .expect("hover fallback");
        assert_eq!(hover["contents"]["kind"], json!("markdown"));
        assert_eq!(
            hover["contents"]["value"],
            json!("```text\ninner\nfn inner()\n```")
        );
        assert_eq!(hover["range"], symbol_range(2, 3, 2, 8));
    }

    #[test]
    fn find_workspace_root_prefers_language_markers() {
        let tempdir = tempdir().expect("create tempdir");
        let workspace = tempdir.path().join("workspace");
        let nested = workspace.join("src").join("nested");
        std::fs::create_dir_all(&nested).expect("create nested dirs");
        std::fs::write(workspace.join("Cargo.toml"), "[package]\nname='demo'\n")
            .expect("write cargo toml");

        let provider = load_provider_registry(tempdir.path(), tempdir.path())
            .into_iter()
            .find(|provider| provider.id == "rust")
            .expect("rust provider");
        let detected = find_workspace_root(nested, &provider);
        assert_eq!(detected, workspace);
    }

    #[test]
    fn registry_loads_external_provider_plugins() {
        let tempdir = tempdir().expect("create tempdir");
        let workspace = tempdir.path().join("workspace");
        let codex_home = tempdir.path().join("codex-home");
        let plugins_dir = codex_home.join("lsp-providers");
        std::fs::create_dir_all(&workspace).expect("create workspace");
        std::fs::create_dir_all(&plugins_dir).expect("create plugin dir");
        std::fs::write(
            plugins_dir.join("java.json"),
            r#"{
                "id": "java",
                "aliases": ["java"],
                "fileExtensions": ["java"],
                "workspaceMarkers": ["pom.xml", "build.gradle"],
                "command": "jdtls",
                "args": ["-data", ".codex-java-workspace"],
                "languageId": "java"
            }"#,
        )
        .expect("write java provider plugin");

        let providers = load_provider_registry(&workspace, &codex_home);
        assert!(providers.iter().any(|provider| provider.id == "java"));
    }

    #[test]
    fn providers_action_lists_builtin_and_plugin_entries() {
        let tempdir = tempdir().expect("create tempdir");
        let workspace = tempdir.path().join("workspace");
        let codex_home = tempdir.path().join("codex-home");
        let plugins_dir = workspace.join(".codex").join("lsp-providers");
        std::fs::create_dir_all(&plugins_dir).expect("create plugin dir");
        std::fs::write(
            plugins_dir.join("java.json"),
            r#"{
                "id": "java",
                "aliases": ["java"],
                "fileExtensions": ["java"],
                "workspaceMarkers": ["pom.xml", "build.gradle"],
                "command": "jdtls",
                "args": ["-data", ".codex-java-workspace"],
                "languageId": "java"
            }"#,
        )
        .expect("write java provider plugin");

        let output = super::invoke(
            super::LspToolRequest {
                action: super::LspAction::Providers,
                path: None,
                language: None,
                goal: None,
                line: None,
                column: None,
                end_line: None,
                end_column: None,
                query: None,
                new_name: None,
                include_declaration: None,
                limit: Some(20),
                trigger_character: None,
                only: Vec::new(),
                apply: false,
                timeout_ms: None,
            },
            workspace,
            codex_home,
        )
        .expect("invoke providers action");

        let payload: serde_json::Value =
            serde_json::from_str(&output).expect("providers output should be valid json");
        let providers = payload["providers"]
            .as_array()
            .expect("providers should be an array");
        assert!(
            providers
                .iter()
                .any(|provider| provider["id"] == json!("python"))
        );
        assert!(
            providers
                .iter()
                .any(|provider| provider["id"] == json!("java"))
        );
    }

    #[test]
    fn providers_action_uses_request_path_for_workspace_plugins() {
        let tempdir = tempdir().expect("create tempdir");
        let workspace = tempdir.path().join("workspace");
        let other_cwd = tempdir.path().join("other-cwd");
        let codex_home = tempdir.path().join("codex-home");
        let plugins_dir = workspace.join(".codex").join("lsp-providers");
        std::fs::create_dir_all(&plugins_dir).expect("create plugin dir");
        std::fs::create_dir_all(&other_cwd).expect("create alternate cwd");
        std::fs::write(
            plugins_dir.join("java.json"),
            r#"{
                "id": "java",
                "aliases": ["java"],
                "fileExtensions": ["java"],
                "workspaceMarkers": ["pom.xml", "build.gradle"],
                "command": "jdtls",
                "args": ["-data", ".codex-java-workspace"],
                "languageId": "java"
            }"#,
        )
        .expect("write java provider plugin");

        let output = super::invoke(
            super::LspToolRequest {
                action: super::LspAction::Providers,
                path: Some(workspace.display().to_string()),
                language: None,
                goal: None,
                line: None,
                column: None,
                end_line: None,
                end_column: None,
                query: None,
                new_name: None,
                include_declaration: None,
                limit: Some(20),
                trigger_character: None,
                only: Vec::new(),
                apply: false,
                timeout_ms: None,
            },
            other_cwd,
            codex_home,
        )
        .expect("invoke providers action with request path");

        let payload: serde_json::Value =
            serde_json::from_str(&output).expect("providers output should be valid json");
        let providers = payload["providers"]
            .as_array()
            .expect("providers should be an array");
        assert!(
            providers
                .iter()
                .any(|provider| provider["id"] == json!("java"))
        );
    }

    #[test]
    fn auto_action_prefers_references_for_usage_questions() {
        let request = super::LspToolRequest {
            action: super::LspAction::Auto,
            path: Some("src/main.rs".to_string()),
            language: None,
            goal: Some("what uses this function".to_string()),
            line: Some(10),
            column: Some(5),
            end_line: None,
            end_column: None,
            query: None,
            new_name: None,
            include_declaration: None,
            limit: None,
            trigger_character: None,
            only: Vec::new(),
            apply: false,
            timeout_ms: None,
        };

        let action = super::infer_auto_action(&request, Some(Path::new("src/main.rs")));
        assert_eq!(action, super::LspAction::References);
    }

    #[test]
    fn document_symbols_returns_go_functions_when_gopls_available() {
        if super::resolve_command_path("gopls").is_none() {
            return;
        }

        let tempdir = tempdir().expect("create tempdir");
        let workspace = tempdir.path().join("workspace");
        std::fs::create_dir_all(&workspace).expect("create workspace");
        std::fs::write(workspace.join("go.mod"), "module demo\n\ngo 1.22\n").expect("write go.mod");
        let file_path = workspace.join("main.go");
        std::fs::write(
            &file_path,
            "package main\n\nfunc helper() int {\n\treturn 1\n}\n\nfunc main() {\n\t_ = helper()\n}\n",
        )
        .expect("write go source");

        let output = super::invoke(
            super::LspToolRequest {
                action: super::LspAction::DocumentSymbols,
                path: Some(file_path.display().to_string()),
                language: Some("go".to_string()),
                goal: None,
                line: None,
                column: None,
                end_line: None,
                end_column: None,
                query: None,
                new_name: None,
                include_declaration: None,
                limit: Some(20),
                trigger_character: None,
                only: Vec::new(),
                apply: false,
                timeout_ms: Some(30000),
            },
            workspace,
            tempdir.path().join("codex-home"),
        )
        .expect("invoke go document symbols");

        let payload: serde_json::Value =
            serde_json::from_str(&output).expect("go document symbols output should be valid json");
        assert_eq!(payload["provider"], json!("go"));
        assert_eq!(payload["resolved_action"], json!("document_symbols"));
    }
    #[test]
    fn document_symbols_returns_rust_functions_when_rust_analyzer_available() {
        if super::resolve_command_path("rust-analyzer").is_none() {
            return;
        }

        let tempdir = tempdir().expect("create tempdir");
        let workspace = tempdir.path().join("workspace");
        let src_dir = workspace.join("src");
        std::fs::create_dir_all(&src_dir).expect("create src dir");
        std::fs::write(
            workspace.join("Cargo.toml"),
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .expect("write cargo toml");
        let file_path = src_dir.join("main.rs");
        std::fs::write(
            &file_path,
            "fn helper() -> i32 {\n    1\n}\n\nfn main() {\n    let _ = helper();\n}\n",
        )
        .expect("write rust source");

        let output = super::invoke(
            super::LspToolRequest {
                action: super::LspAction::DocumentSymbols,
                path: Some(file_path.display().to_string()),
                language: Some("rust".to_string()),
                goal: None,
                line: None,
                column: None,
                end_line: None,
                end_column: None,
                query: None,
                new_name: None,
                include_declaration: None,
                limit: Some(20),
                trigger_character: None,
                only: Vec::new(),
                apply: false,
                timeout_ms: Some(30000),
            },
            workspace,
            tempdir.path().join("codex-home"),
        )
        .expect("invoke document symbols");

        let payload: serde_json::Value =
            serde_json::from_str(&output).expect("document symbols output should be valid json");
        let result = payload["result"]
            .as_array()
            .expect("document symbols result should be an array");
        let names: Vec<_> = result
            .iter()
            .filter_map(|item| item.get("name").and_then(serde_json::Value::as_str))
            .collect();
        assert!(names.contains(&"helper"));
        assert!(names.contains(&"main"));
    }

    #[test]
    fn detects_rust_analyzer_rustup_proxy_from_cargo_bin() {
        #[cfg(windows)]
        let command = PathBuf::from(r"C:\Users\alice\.cargo\bin\rust-analyzer.exe");
        #[cfg(windows)]
        let cargo_bin = PathBuf::from(r"C:\Users\alice\.cargo\bin");

        #[cfg(not(windows))]
        let command = PathBuf::from("/home/alice/.cargo/bin/rust-analyzer");
        #[cfg(not(windows))]
        let cargo_bin = PathBuf::from("/home/alice/.cargo/bin");

        assert!(super::is_rust_analyzer_proxy(&command, &[cargo_bin]));
    }

    #[test]
    fn ignores_non_rust_analyzer_commands_in_cargo_bin() {
        #[cfg(windows)]
        let command = PathBuf::from(r"C:\Users\alice\.cargo\bin\gopls.exe");
        #[cfg(windows)]
        let cargo_bin = PathBuf::from(r"C:\Users\alice\.cargo\bin");

        #[cfg(not(windows))]
        let command = PathBuf::from("/home/alice/.cargo/bin/gopls");
        #[cfg(not(windows))]
        let cargo_bin = PathBuf::from("/home/alice/.cargo/bin");

        assert!(!super::is_rust_analyzer_proxy(&command, &[cargo_bin]));
    }

    #[test]
    fn diagnostics_returns_python_type_errors_when_pyright_available() {
        if super::resolve_command_path("pyright-langserver").is_none() {
            return;
        }

        let tempdir = tempdir().expect("create tempdir");
        let workspace = tempdir.path().join("workspace");
        std::fs::create_dir_all(&workspace).expect("create workspace");
        let file_path = workspace.join("main.py");
        std::fs::write(&file_path, "def helper() -> int:\n    return \"bad\"\n")
            .expect("write python source");

        let output = super::invoke(
            super::LspToolRequest {
                action: super::LspAction::Diagnostics,
                path: Some(file_path.display().to_string()),
                language: Some("python".to_string()),
                goal: None,
                line: None,
                column: None,
                end_line: None,
                end_column: None,
                query: None,
                new_name: None,
                include_declaration: None,
                limit: Some(20),
                trigger_character: None,
                only: Vec::new(),
                apply: false,
                timeout_ms: Some(30000),
            },
            workspace,
            tempdir.path().join("codex-home"),
        )
        .expect("invoke python diagnostics");

        let payload: serde_json::Value =
            serde_json::from_str(&output).expect("python diagnostics output should be valid json");
        let _result = payload["result"]
            .as_array()
            .expect("python diagnostics result should be an array");
        assert_eq!(payload["provider"], json!("python"));
        assert_eq!(payload["resolved_action"], json!("diagnostics"));
    }

    #[test]
    fn document_symbols_returns_typescript_functions_when_ts_server_available() {
        if super::resolve_command_path("typescript-language-server").is_none() {
            return;
        }

        let tempdir = tempdir().expect("create tempdir");
        let workspace = tempdir.path().join("workspace");
        let src_dir = workspace.join("src");
        std::fs::create_dir_all(&src_dir).expect("create src dir");
        std::fs::write(
            workspace.join("package.json"),
            "{\n  \"name\": \"demo\",\n  \"private\": true\n}\n",
        )
        .expect("write package json");
        std::fs::write(
            workspace.join("tsconfig.json"),
            "{\n  \"compilerOptions\": {\n    \"target\": \"ES2020\",\n    \"module\": \"commonjs\"\n  }\n}\n",
        )
        .expect("write tsconfig");
        let file_path = src_dir.join("main.ts");
        std::fs::write(
            &file_path,
            "function helper(): number {\n  return 1;\n}\n\nfunction main(): number {\n  return helper();\n}\n",
        )
        .expect("write typescript source");

        let output = super::invoke(
            super::LspToolRequest {
                action: super::LspAction::DocumentSymbols,
                path: Some(file_path.display().to_string()),
                language: Some("typescript".to_string()),
                goal: None,
                line: None,
                column: None,
                end_line: None,
                end_column: None,
                query: None,
                new_name: None,
                include_declaration: None,
                limit: Some(20),
                trigger_character: None,
                only: Vec::new(),
                apply: false,
                timeout_ms: Some(30000),
            },
            workspace,
            tempdir.path().join("codex-home"),
        )
        .expect("invoke typescript document symbols");

        let payload: serde_json::Value = serde_json::from_str(&output)
            .expect("typescript document symbols output should be valid json");
        let result = payload["result"]
            .as_array()
            .expect("typescript document symbols result should be an array");
        let names: Vec<_> = result
            .iter()
            .filter_map(|item| item.get("name").and_then(serde_json::Value::as_str))
            .collect();
        assert!(names.contains(&"helper"));
        assert!(names.contains(&"main"));
    }
}
