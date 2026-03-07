use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use serde::Deserialize;
use serde_json::Value;
use serde_json::json;
use std::collections::HashMap;
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
enum LspProviderPluginFile {
    Single(LspProviderPlugin),
    Many { providers: Vec<LspProviderPlugin> },
}

#[derive(Debug, Clone)]
struct LspServerConfig {
    command: String,
    args: Vec<String>,
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
    next_id: i64,
    diagnostics_by_uri: HashMap<String, Value>,
}

impl Drop for LspTransport {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl LspTransport {
    fn spawn(config: &LspServerConfig, workspace_root: &Path) -> Result<Self> {
        let command = if Path::new(&config.command).is_absolute() {
            PathBuf::from(&config.command)
        } else {
            which::which(&config.command)
                .with_context(|| format!("language server command not found: {}", config.command))?
        };

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
            next_id: 1,
            diagnostics_by_uri: HashMap::new(),
        })
    }

    fn initialize(&mut self, workspace_root: &Path, timeout: Duration) -> Result<()> {
        let root_uri = path_to_file_url(workspace_root)?;
        let params = json!({
            "processId": std::process::id(),
            "rootUri": root_uri,
            "capabilities": {
                "workspace": {
                    "applyEdit": false,
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
                        "prepareSupport": false
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

        let _ = self.send_request("initialize", params, timeout)?;
        self.send_notification("initialized", json!({}))?;
        Ok(())
    }

    fn open_document(&mut self, path: &Path, language_id: &str) -> Result<String> {
        let uri = path_to_file_url(path)?;
        let text = String::from_utf8_lossy(&fs::read(path)?).into_owned();
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
        Ok(uri)
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

        self.write_message(&json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "method": method,
            "params": params,
        }))?;

        let started_at = Instant::now();
        loop {
            let elapsed = started_at.elapsed();
            if elapsed >= timeout {
                bail!("LSP request timed out: {method}");
            }

            let remaining_timeout = timeout.saturating_sub(elapsed);
            match self.recv_event(remaining_timeout)? {
                Some(ReaderEvent::Message(message)) => {
                    if let Some(response_id) = message.get("id")
                        && response_id.as_i64() == Some(request_id)
                    {
                        if let Some(error) = message.get("error") {
                            bail!("LSP request failed for {method}: {error}");
                        }
                        return Ok(message.get("result").cloned().unwrap_or(Value::Null));
                    }
                    let _ = self.handle_message(message)?;
                }
                Some(ReaderEvent::Closed) => return Err(self.closed_before_responding_error()),
                Some(ReaderEvent::Error(message)) => bail!(message),
                None => bail!("LSP request timed out: {method}"),
            }
        }
    }

    fn shutdown(&mut self) {
        let _ = self.send_request("shutdown", Value::Null, Duration::from_secs(2));
        let _ = self.send_notification("exit", json!({}));
    }

    fn recv_event(&self, timeout: Duration) -> Result<Option<ReaderEvent>> {
        match self.events.recv_timeout(timeout) {
            Ok(event) => Ok(Some(event)),
            Err(RecvTimeoutError::Timeout) => Ok(None),
            Err(RecvTimeoutError::Disconnected) => Ok(Some(ReaderEvent::Closed)),
        }
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
                let item_count = params
                    .get("items")
                    .and_then(Value::as_array)
                    .map_or(0, Vec::len);
                Value::Array((0..item_count).map(|_| Value::Null).collect())
            }
            "workspace/workspaceFolders" => Value::Array(Vec::new()),
            "workspace/applyEdit" => json!({ "applied": false }),
            "window/showDocument" => json!({ "success": false }),
            _ => Value::Null,
        };

        self.write_message(&json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "result": result,
        }))
    }

    fn write_message(&mut self, value: &Value) -> Result<()> {
        let payload = serde_json::to_vec(value)?;
        write!(self.stdin, "Content-Length: {}\r\n\r\n", payload.len())?;
        self.stdin.write_all(&payload)?;
        self.stdin.flush()?;
        Ok(())
    }
}

pub(crate) fn invoke(request: LspToolRequest, cwd: PathBuf, codex_home: PathBuf) -> Result<String> {
    let timeout = Duration::from_millis(request.timeout_ms.unwrap_or_else(default_timeout_ms));
    let limit = request.limit.unwrap_or(DEFAULT_LIMIT).max(1);
    let resolved_path = resolve_request_path(&cwd, request.path.as_deref())?;
    let providers = load_provider_registry(&cwd, &codex_home);

    if request.action == LspAction::Providers {
        let mut result: Vec<Value> = probe_provider_status(&cwd, &codex_home)
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

    let mut transport = LspTransport::spawn(&server_config, &workspace_root)?;
    transport.initialize(&workspace_root, timeout)?;

    let mut opened_document_uri: Option<String> = None;
    let mut opened_document_path: Option<PathBuf> = None;
    if effective_action != LspAction::WorkspaceSymbols {
        let path = resolved_path
            .as_deref()
            .ok_or_else(|| anyhow!("path is required for {}", effective_action.as_str()))?;
        if !path.is_file() {
            bail!(
                "path must point to a file for {}",
                effective_action.as_str()
            );
        }
        let language_id = provider.language_id_for_path(path);
        let uri = transport.open_document(path, &language_id)?;
        opened_document_uri = Some(uri);
        opened_document_path = Some(path.to_path_buf());
    }

    let raw_result = match effective_action {
        LspAction::Auto => unreachable!("auto action resolves before request dispatch"),
        LspAction::Providers => unreachable!("providers action returns before request dispatch"),
        LspAction::Diagnostics => transport.collect_diagnostics(
            opened_document_uri
                .as_deref()
                .ok_or_else(|| anyhow!("diagnostics requires an opened document"))?,
            timeout,
            Duration::from_millis(DIAGNOSTICS_QUIET_PERIOD_MS),
        )?,
        LspAction::Definition => transport.send_request(
            "textDocument/definition",
            position_params(
                opened_document_uri.as_deref(),
                request.line,
                request.column,
            )?,
            timeout,
        )?,
        LspAction::References => transport.send_request(
            "textDocument/references",
            json!({
                "textDocument": {
                    "uri": opened_document_uri.as_deref().ok_or_else(|| anyhow!("references requires an opened document"))?
                },
                "position": position_value(request.line, request.column)?,
                "context": {
                    "includeDeclaration": request.include_declaration.unwrap_or(true)
                }
            }),
            timeout,
        )?,
        LspAction::Hover => transport.send_request(
            "textDocument/hover",
            position_params(
                opened_document_uri.as_deref(),
                request.line,
                request.column,
            )?,
            timeout,
        )?,
        LspAction::DocumentSymbols => transport.send_request(
            "textDocument/documentSymbol",
            json!({
                "textDocument": {
                    "uri": opened_document_uri.as_deref().ok_or_else(|| anyhow!("document_symbols requires an opened document"))?
                }
            }),
            timeout,
        )?,
        LspAction::WorkspaceSymbols => transport.send_request(
            "workspace/symbol",
            json!({
                "query": request.query.clone().unwrap_or_default()
            }),
            timeout,
        )?,
        LspAction::Rename => transport.send_request(
            "textDocument/rename",
            json!({
                "textDocument": {
                    "uri": opened_document_uri.as_deref().ok_or_else(|| anyhow!("rename requires an opened document"))?
                },
                "position": position_value(request.line, request.column)?,
                "newName": request.new_name.ok_or_else(|| anyhow!("rename requires new_name"))?
            }),
            timeout,
        )?,
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
                        "uri": opened_document_uri.as_deref().ok_or_else(|| anyhow!("completion requires an opened document"))?
                    },
                    "position": position_value(request.line, request.column)?,
                    "context": context,
                }),
                timeout,
            )?
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
                        "uri": opened_document_uri.as_deref().ok_or_else(|| anyhow!("signature_help requires an opened document"))?
                    },
                    "position": position_value(request.line, request.column)?,
                    "context": context,
                }),
                timeout,
            )?
        }
        LspAction::CodeActions => {
            let document_uri = opened_document_uri
                .as_deref()
                .ok_or_else(|| anyhow!("code_actions requires an opened document"))?;
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
            )?
        }
    };

    transport.shutdown();

    let mut normalized_result = raw_result;
    normalize_value_for_output(&mut normalized_result);
    truncate_result(&effective_action, &mut normalized_result, limit);

    let payload = json!({
        "requested_action": request.action.as_str(),
        "resolved_action": effective_action.as_str(),
        "provider": provider.id,
        "workspace_root": workspace_root.display().to_string(),
        "path": opened_document_path.map(|path| path.display().to_string()),
        "server_command": server_config.command,
        "server_args": server_config.args,
        "result": normalized_result,
    });
    Ok(serde_json::to_string_pretty(&payload)?)
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
    if candidate.is_absolute() {
        return candidate.exists().then(|| candidate.to_path_buf());
    }
    which::which(command).ok()
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

pub fn probe_provider_status(cwd: &Path, codex_home: &Path) -> Vec<LspProviderStatus> {
    load_provider_registry(cwd, codex_home)
        .into_iter()
        .map(|provider| probe_single_provider_status(cwd, &provider))
        .collect()
}

fn probe_single_provider_status(cwd: &Path, provider: &LspProvider) -> LspProviderStatus {
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

    let workspace_root =
        resolve_workspace_root(cwd, None, provider).unwrap_or_else(|_| cwd.to_path_buf());
    let server_config = match provider.resolve_server_config() {
        Ok(config) => config,
        Err(err) => {
            status.status = "error".to_string();
            status.error = Some(err.to_string());
            return status;
        }
    };

    match LspTransport::spawn(&server_config, &workspace_root).and_then(|mut transport| {
        let initialize_result = transport.initialize(&workspace_root, Duration::from_millis(3_000));
        transport.shutdown();
        initialize_result
    }) {
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
    use super::find_workspace_root;
    use super::load_provider_registry;
    use super::normalize_value_for_output;
    use serde_json::json;
    use std::path::Path;
    use std::path::PathBuf;
    use tempfile::tempdir;

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
