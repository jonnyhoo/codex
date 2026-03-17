use std::collections::BTreeSet;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use codex_protocol::models::SandboxPermissions;
use serde::Deserialize;
use serde::Serialize;

use crate::codex::Session;
use crate::codex::TurnContext;
use crate::error::CodexErr;
use crate::error::SandboxErr;
use crate::exec::ExecExpiration;
use crate::exec::ExecParams;
use crate::exec::ExecToolCallOutput;
use crate::exec_env::create_env;
use crate::features::Feature;
use crate::function_tool::FunctionCallError;
use crate::parse_command::shlex_join;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::events::ToolEmitter;
use crate::tools::events::ToolEventCtx;
use crate::tools::events::ToolEventFailure;
use crate::tools::events::ToolEventStage;
use crate::tools::handlers::normalize_and_validate_additional_permissions;
use crate::tools::handlers::parse_arguments_with_base_path;
use crate::tools::handlers::resolve_workdir_base_path;
use crate::tools::orchestrator::ToolOrchestrator;
use crate::tools::registry::ToolHandler;
use crate::tools::registry::ToolKind;
use crate::tools::sandboxing::ToolError;
use crate::truncate::TruncationPolicy;
use crate::truncate::formatted_truncate_text;
use crate::util::ancestor_search_boundary;

const DEFAULT_STEP_TIMEOUT_MS: u64 = 15 * 60 * 1000;
const DESCENDANT_SEARCH_MAX_DEPTH: usize = 3;
const STEP_OUTPUT_EXCERPT_BYTES: usize = 4 * 1024;
const CARGO_LOCK_WAIT_TEXT: &str = "Blocking waiting for file lock";

pub struct RunProjectChecksHandler;

pub(crate) fn run_project_checks_tool_description() -> String {
    "Detects common project types (Rust, Node, Python, Go), plans build/test commands, executes them through the normal shell approval+sandbox pipeline, and returns a structured JSON summary for each step.".to_string()
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum ProjectChecksTask {
    Detect,
    Build,
    Test,
    Verify,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
enum RequestedProjectType {
    #[default]
    Auto,
    Rust,
    Node,
    Python,
    Go,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum DetectedProjectType {
    Rust,
    Node,
    Python,
    Go,
    Unknown,
}

#[derive(Debug, Deserialize)]
pub struct RunProjectChecksArgs {
    task: ProjectChecksTask,
    #[serde(default)]
    project_type: RequestedProjectType,
    #[serde(default)]
    target: Option<String>,
    #[serde(default)]
    test_filter: Option<String>,
    #[serde(default = "default_quick")]
    quick: bool,
    #[serde(default)]
    continue_on_error: bool,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

fn default_quick() -> bool {
    true
}

#[derive(Debug)]
struct ProjectDetection {
    project_type: DetectedProjectType,
    project_root: PathBuf,
    package_manager: Option<String>,
    scripts: BTreeSet<String>,
    warnings: Vec<String>,
}

#[derive(Debug)]
struct PlannedStep {
    label: String,
    command: String,
}

#[derive(Debug)]
struct PlanningFailure {
    detection: ProjectDetection,
    warnings: Vec<String>,
    message: String,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum StepStatus {
    Passed,
    Failed,
    TimedOut,
    Rejected,
    Error,
}

#[derive(Debug, Serialize)]
struct StepResult {
    label: String,
    command: String,
    status: StepStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    duration_seconds: Option<f32>,
    timed_out: bool,
    lock_wait_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    output_excerpt: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, Serialize)]
struct RunProjectChecksResponse {
    task: ProjectChecksTask,
    requested_project_type: RequestedProjectType,
    detected_project_type: DetectedProjectType,
    project_root: String,
    quick: bool,
    success: bool,
    stopped_after_failure: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    package_manager: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    available_scripts: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    warnings: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    steps: Vec<StepResult>,
    summary: String,
}

#[async_trait]
impl ToolHandler for RunProjectChecksHandler {
    type Output = FunctionToolOutput;

    fn kind(&self) -> ToolKind {
        ToolKind::Function
    }

    async fn handle(&self, invocation: ToolInvocation) -> Result<Self::Output, FunctionCallError> {
        let ToolInvocation {
            session,
            turn,
            call_id,
            payload,
            ..
        } = invocation;

        let arguments = match payload {
            ToolPayload::Function { arguments } => arguments,
            _ => {
                return Err(FunctionCallError::RespondToModel(
                    "run_project_checks handler received unsupported payload".to_string(),
                ));
            }
        };

        let cwd = resolve_workdir_base_path(&arguments, turn.cwd.as_path())?;
        let args: RunProjectChecksArgs = parse_arguments_with_base_path(&arguments, cwd.as_path())?;
        let detection = match detect_project(cwd.as_path(), args.project_type) {
            Ok(detection) => detection,
            Err(FunctionCallError::RespondToModel(message)) => {
                return serialize_error_response(
                    &args,
                    error_project_root(cwd.as_path()),
                    DetectedProjectType::Unknown,
                    None,
                    BTreeSet::new(),
                    Vec::new(),
                    message,
                );
            }
            Err(err) => return Err(err),
        };

        if matches!(args.task, ProjectChecksTask::Detect) {
            let response = RunProjectChecksResponse {
                task: args.task,
                requested_project_type: args.project_type,
                detected_project_type: detection.project_type,
                project_root: detection.project_root.display().to_string(),
                quick: args.quick,
                success: detection.project_type != DetectedProjectType::Unknown,
                stopped_after_failure: false,
                package_manager: detection.package_manager,
                available_scripts: detection.scripts.into_iter().collect(),
                warnings: detection.warnings,
                error: None,
                steps: Vec::new(),
                summary: format!(
                    "Detected {} project at {}",
                    detection.project_type.as_str(),
                    detection.project_root.display()
                ),
            };
            return serialize_response(response);
        }

        let (detection, steps, warnings) =
            match plan_steps_with_fallback(cwd.as_path(), &args, detection) {
                Ok(result) => result,
                Err(PlanningFailure {
                    detection,
                    warnings,
                    message,
                }) => {
                    return serialize_error_response(
                        &args,
                        detection.project_root,
                        detection.project_type,
                        detection.package_manager,
                        detection.scripts,
                        warnings,
                        message,
                    );
                }
            };
        let total_steps = steps.len();
        let mut results = Vec::with_capacity(steps.len());
        let mut success = true;
        let mut stopped_after_failure = false;

        for (index, step) in steps.into_iter().enumerate() {
            let step_call_id = format!("{call_id}:step:{}", index + 1);
            let step_result = run_planned_step(
                &session,
                &turn,
                &detection.project_root,
                &step_call_id,
                &step,
                args.timeout_ms,
            )
            .await?;
            let step_ok = matches!(step_result.status, StepStatus::Passed);
            if !step_ok {
                success = false;
            }
            results.push(step_result);
            if !step_ok && !args.continue_on_error {
                stopped_after_failure = index + 1 < total_steps;
                break;
            }
        }

        let summary = build_summary(&results, success, stopped_after_failure);
        let response = RunProjectChecksResponse {
            task: args.task,
            requested_project_type: args.project_type,
            detected_project_type: detection.project_type,
            project_root: detection.project_root.display().to_string(),
            quick: args.quick,
            success,
            stopped_after_failure,
            package_manager: detection.package_manager,
            available_scripts: detection.scripts.into_iter().collect(),
            warnings,
            error: None,
            steps: results,
            summary,
        };
        serialize_response_with_success(response, success)
    }
}

fn serialize_response(
    response: RunProjectChecksResponse,
) -> Result<FunctionToolOutput, FunctionCallError> {
    let success = response.success;
    serialize_response_with_success(response, success)
}

fn serialize_response_with_success(
    response: RunProjectChecksResponse,
    success: bool,
) -> Result<FunctionToolOutput, FunctionCallError> {
    let text = serde_json::to_string_pretty(&response).map_err(|err| {
        FunctionCallError::Fatal(format!(
            "failed to serialize run_project_checks response: {err}"
        ))
    })?;
    Ok(FunctionToolOutput::from_text(text, Some(success)))
}

fn serialize_error_response(
    args: &RunProjectChecksArgs,
    project_root: PathBuf,
    detected_project_type: DetectedProjectType,
    package_manager: Option<String>,
    scripts: BTreeSet<String>,
    warnings: Vec<String>,
    error: String,
) -> Result<FunctionToolOutput, FunctionCallError> {
    let summary = error.clone();
    serialize_response_with_success(
        RunProjectChecksResponse {
            task: args.task,
            requested_project_type: args.project_type,
            detected_project_type,
            project_root: project_root.display().to_string(),
            quick: args.quick,
            success: false,
            stopped_after_failure: false,
            package_manager,
            available_scripts: scripts.into_iter().collect(),
            warnings,
            error: Some(error),
            steps: Vec::new(),
            summary,
        },
        false,
    )
}

fn detect_project(
    start: &Path,
    requested: RequestedProjectType,
) -> Result<ProjectDetection, FunctionCallError> {
    let start = normalize_project_search_start(start);
    let boundary = ancestor_search_boundary(&start);

    for ancestor in start.ancestors() {
        if let Some(boundary) = boundary.as_deref()
            && !ancestor.starts_with(boundary)
        {
            break;
        }
        if let Some(detection) = detect_at_ancestor(ancestor, requested)? {
            return Ok(detection);
        }
        if boundary.as_deref() == Some(ancestor) {
            break;
        }
    }

    if requested != RequestedProjectType::Auto {
        return Err(FunctionCallError::RespondToModel(format!(
            "run_project_checks could not find a {} project above {}",
            requested.as_str(),
            start.display()
        )));
    }

    let project_root = crate::git_info::get_git_repo_root(&start).unwrap_or(start);
    Ok(ProjectDetection {
        project_type: DetectedProjectType::Unknown,
        project_root,
        package_manager: None,
        scripts: BTreeSet::new(),
        warnings: vec!["Could not detect a supported project type automatically.".to_string()],
    })
}

fn normalize_project_search_start(start: &Path) -> PathBuf {
    if start.is_dir() {
        start.to_path_buf()
    } else {
        start
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| start.to_path_buf())
    }
}

fn error_project_root(start: &Path) -> PathBuf {
    let start = normalize_project_search_start(start);
    crate::git_info::get_git_repo_root(&start).unwrap_or(start)
}

fn detect_at_ancestor(
    ancestor: &Path,
    requested: RequestedProjectType,
) -> Result<Option<ProjectDetection>, FunctionCallError> {
    let candidates = match requested {
        RequestedProjectType::Auto => [
            RequestedProjectType::Rust,
            RequestedProjectType::Go,
            RequestedProjectType::Node,
            RequestedProjectType::Python,
        ]
        .as_slice(),
        _ => std::slice::from_ref(&requested),
    };

    for candidate in candidates {
        match candidate {
            RequestedProjectType::Rust if ancestor.join("Cargo.toml").is_file() => {
                return Ok(Some(ProjectDetection {
                    project_type: DetectedProjectType::Rust,
                    project_root: ancestor.to_path_buf(),
                    package_manager: Some("cargo".to_string()),
                    scripts: BTreeSet::new(),
                    warnings: Vec::new(),
                }));
            }
            RequestedProjectType::Go
                if ancestor.join("go.mod").is_file() || ancestor.join("go.work").is_file() =>
            {
                return Ok(Some(ProjectDetection {
                    project_type: DetectedProjectType::Go,
                    project_root: ancestor.to_path_buf(),
                    package_manager: Some("go".to_string()),
                    scripts: BTreeSet::new(),
                    warnings: Vec::new(),
                }));
            }
            RequestedProjectType::Node if ancestor.join("package.json").is_file() => {
                let package_json = ancestor.join("package.json");
                let node_info = read_node_project_info(&package_json)?;
                return Ok(Some(ProjectDetection {
                    project_type: DetectedProjectType::Node,
                    project_root: ancestor.to_path_buf(),
                    package_manager: Some(node_info.package_manager),
                    scripts: node_info.scripts,
                    warnings: node_info.warnings,
                }));
            }
            RequestedProjectType::Python
                if [
                    "pyproject.toml",
                    "setup.py",
                    "setup.cfg",
                    "requirements.txt",
                ]
                .into_iter()
                .any(|marker| ancestor.join(marker).is_file()) =>
            {
                return Ok(Some(ProjectDetection {
                    project_type: DetectedProjectType::Python,
                    project_root: ancestor.to_path_buf(),
                    package_manager: Some(detect_python_runner(ancestor).to_string()),
                    scripts: BTreeSet::new(),
                    warnings: Vec::new(),
                }));
            }
            _ => {}
        }
    }

    Ok(None)
}

struct NodeProjectInfo {
    package_manager: String,
    scripts: BTreeSet<String>,
    warnings: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct PackageJsonFile {
    #[serde(rename = "packageManager")]
    package_manager: Option<String>,
    #[serde(default)]
    scripts: HashMap<String, String>,
}

fn read_node_project_info(package_json_path: &Path) -> Result<NodeProjectInfo, FunctionCallError> {
    let package_json_text = std::fs::read_to_string(package_json_path).map_err(|err| {
        FunctionCallError::RespondToModel(format!(
            "failed to read {}: {err}",
            package_json_path.display()
        ))
    })?;
    let package_json: PackageJsonFile =
        serde_json::from_str(&package_json_text).unwrap_or_else(|_| PackageJsonFile {
            package_manager: None,
            scripts: HashMap::new(),
        });
    let package_manager = package_json
        .package_manager
        .as_deref()
        .map(parse_node_package_manager)
        .unwrap_or_else(|| detect_node_package_manager_from_lockfiles(package_json_path.parent()));

    Ok(NodeProjectInfo {
        package_manager: package_manager.to_string(),
        scripts: package_json.scripts.into_keys().collect(),
        warnings: Vec::new(),
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NodePackageManager {
    Npm,
    Pnpm,
    Yarn,
    Bun,
}

impl NodePackageManager {
    fn command_for_script(self, script: &str) -> String {
        match self {
            Self::Npm => shlex_join(&["npm".to_string(), "run".to_string(), script.to_string()]),
            Self::Pnpm => shlex_join(&["pnpm".to_string(), "run".to_string(), script.to_string()]),
            Self::Yarn => shlex_join(&["yarn".to_string(), script.to_string()]),
            Self::Bun => shlex_join(&["bun".to_string(), "run".to_string(), script.to_string()]),
        }
    }
}

impl std::fmt::Display for NodePackageManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let text = match self {
            Self::Npm => "npm",
            Self::Pnpm => "pnpm",
            Self::Yarn => "yarn",
            Self::Bun => "bun",
        };
        f.write_str(text)
    }
}

fn parse_node_package_manager(value: &str) -> NodePackageManager {
    if value.starts_with("pnpm@") {
        NodePackageManager::Pnpm
    } else if value.starts_with("yarn@") {
        NodePackageManager::Yarn
    } else if value.starts_with("bun@") {
        NodePackageManager::Bun
    } else {
        NodePackageManager::Npm
    }
}

fn detect_node_package_manager_from_lockfiles(dir: Option<&Path>) -> NodePackageManager {
    let Some(dir) = dir else {
        return NodePackageManager::Npm;
    };
    if dir.join("pnpm-lock.yaml").is_file() {
        NodePackageManager::Pnpm
    } else if dir.join("yarn.lock").is_file() {
        NodePackageManager::Yarn
    } else if dir.join("bun.lock").is_file() || dir.join("bun.lockb").is_file() {
        NodePackageManager::Bun
    } else {
        NodePackageManager::Npm
    }
}

#[derive(Debug, Clone, Copy)]
enum PythonRunner {
    Uv,
    Poetry,
    Python,
}

impl PythonRunner {
    fn to_string(self) -> String {
        match self {
            Self::Uv => "uv".to_string(),
            Self::Poetry => "poetry".to_string(),
            Self::Python => "python".to_string(),
        }
    }

    fn command(self, args: &[String]) -> String {
        let mut tokens = match self {
            Self::Uv => vec!["uv".to_string(), "run".to_string()],
            Self::Poetry => vec!["poetry".to_string(), "run".to_string()],
            Self::Python => vec!["python".to_string()],
        };
        tokens.extend_from_slice(args);
        shlex_join(&tokens)
    }
}

fn detect_python_runner(root: &Path) -> PythonRunner {
    if root.join("uv.lock").is_file() {
        PythonRunner::Uv
    } else if root.join("poetry.lock").is_file() {
        PythonRunner::Poetry
    } else {
        PythonRunner::Python
    }
}

fn plan_steps(
    args: &RunProjectChecksArgs,
    detection: &ProjectDetection,
    warnings: &mut Vec<String>,
) -> Result<Vec<PlannedStep>, FunctionCallError> {
    match detection.project_type {
        DetectedProjectType::Rust => Ok(plan_rust_steps(args)),
        DetectedProjectType::Go => Ok(plan_go_steps(args)),
        DetectedProjectType::Node => plan_node_steps(args, detection),
        DetectedProjectType::Python => Ok(plan_python_steps(args, &detection.project_root)),
        DetectedProjectType::Unknown => Err(FunctionCallError::RespondToModel(
            "run_project_checks could not detect a supported project type; use task=detect or set project_type explicitly".to_string(),
        )),
    }
    .map(|steps| {
        if args.target.is_some()
            && !matches!(
                detection.project_type,
                DetectedProjectType::Rust | DetectedProjectType::Go
            )
        {
            warnings.push(format!(
                "Ignored `target` for {} projects.",
                detection.project_type.as_str()
            ));
        }
        if args.test_filter.is_some()
            && !matches!(
                detection.project_type,
                DetectedProjectType::Rust | DetectedProjectType::Go | DetectedProjectType::Python
            )
        {
            warnings.push(format!(
                "Ignored `test_filter` for {} projects.",
                detection.project_type.as_str()
            ));
        }
        steps
    })
}

fn plan_steps_with_fallback(
    start: &Path,
    args: &RunProjectChecksArgs,
    detection: ProjectDetection,
) -> Result<(ProjectDetection, Vec<PlannedStep>, Vec<String>), PlanningFailure> {
    let mut warnings = detection.warnings.clone();
    match plan_steps(args, &detection, &mut warnings) {
        Ok(steps) => Ok((detection, steps, warnings)),
        Err(FunctionCallError::RespondToModel(message))
            if args.project_type == RequestedProjectType::Auto
                && !matches!(args.task, ProjectChecksTask::Detect) =>
        {
            if let Some((fallback_detection, fallback_steps, mut fallback_warnings)) =
                find_descendant_project_fallback(start, args, &detection)
            {
                fallback_warnings.push(format!(
                    "Auto-detected {} project at {} but it could not satisfy task {}; using descendant {} project at {} instead.",
                    detection.project_type.as_str(),
                    detection.project_root.display(),
                    args.task.as_str(),
                    fallback_detection.project_type.as_str(),
                    fallback_detection.project_root.display(),
                ));
                Ok((fallback_detection, fallback_steps, fallback_warnings))
            } else {
                Err(PlanningFailure {
                    detection,
                    warnings,
                    message,
                })
            }
        }
        Err(FunctionCallError::RespondToModel(message)) => Err(PlanningFailure {
            detection,
            warnings,
            message,
        }),
        Err(FunctionCallError::Fatal(message)) => Err(PlanningFailure {
            detection,
            warnings,
            message,
        }),
        Err(FunctionCallError::MissingLocalShellCallId) => Err(PlanningFailure {
            detection,
            warnings,
            message: "missing local shell call id".to_string(),
        }),
    }
}

fn find_descendant_project_fallback(
    start: &Path,
    args: &RunProjectChecksArgs,
    current_detection: &ProjectDetection,
) -> Option<(ProjectDetection, Vec<PlannedStep>, Vec<String>)> {
    let start = normalize_project_search_start(start);
    let mut queue = VecDeque::from([(start, 0usize)]);
    let mut best: Option<(
        (usize, usize, String),
        ProjectDetection,
        Vec<PlannedStep>,
        Vec<String>,
    )> = None;

    while let Some((dir, depth)) = queue.pop_front() {
        if depth >= DESCENDANT_SEARCH_MAX_DEPTH {
            continue;
        }

        let mut child_dirs = match std::fs::read_dir(&dir) {
            Ok(entries) => entries
                .filter_map(Result::ok)
                .filter_map(|entry| {
                    let path = entry.path();
                    path.is_dir().then_some(path)
                })
                .filter(|path| {
                    path.file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| !skip_descendant_search_dir(name))
                })
                .collect::<Vec<_>>(),
            Err(_) => continue,
        };
        child_dirs.sort();

        for child_dir in child_dirs {
            if let Ok(Some(candidate_detection)) =
                detect_at_ancestor(child_dir.as_path(), RequestedProjectType::Auto)
            {
                if candidate_detection.project_root != current_detection.project_root {
                    let mut candidate_warnings = candidate_detection.warnings.clone();
                    if let Ok(candidate_steps) =
                        plan_steps(args, &candidate_detection, &mut candidate_warnings)
                    {
                        let candidate_key = (
                            detected_project_type_rank(candidate_detection.project_type),
                            depth + 1,
                            candidate_detection.project_root.display().to_string(),
                        );
                        let replace = best
                            .as_ref()
                            .is_none_or(|best_candidate| candidate_key < best_candidate.0);
                        if replace {
                            best = Some((
                                candidate_key,
                                candidate_detection,
                                candidate_steps,
                                candidate_warnings,
                            ));
                        }
                    }
                }
            }
            queue.push_back((child_dir, depth + 1));
        }
    }

    best.map(|(_, detection, steps, warnings)| (detection, steps, warnings))
}

fn detected_project_type_rank(project_type: DetectedProjectType) -> usize {
    match project_type {
        DetectedProjectType::Rust => 0,
        DetectedProjectType::Go => 1,
        DetectedProjectType::Node => 2,
        DetectedProjectType::Python => 3,
        DetectedProjectType::Unknown => 4,
    }
}

fn skip_descendant_search_dir(name: &str) -> bool {
    matches!(
        name,
        ".git"
            | ".hg"
            | ".svn"
            | ".venv"
            | "__pycache__"
            | "build"
            | "dist"
            | "node_modules"
            | "target"
            | "vendor"
    )
}

fn plan_rust_steps(args: &RunProjectChecksArgs) -> Vec<PlannedStep> {
    let package = args.target.as_deref();
    match args.task {
        ProjectChecksTask::Build => vec![PlannedStep {
            label: if args.quick {
                "cargo check".to_string()
            } else {
                "cargo build".to_string()
            },
            command: cargo_command(
                if args.quick { "check" } else { "build" },
                package,
                None,
                &["--message-format", "short"],
            ),
        }],
        ProjectChecksTask::Test => vec![PlannedStep {
            label: "cargo test".to_string(),
            command: cargo_command("test", package, args.test_filter.as_deref(), &[]),
        }],
        ProjectChecksTask::Verify => {
            let mut steps = vec![PlannedStep {
                label: "cargo check".to_string(),
                command: cargo_command("check", package, None, &["--message-format", "short"]),
            }];
            let (label, extra) = if args.quick {
                (
                    "cargo test --no-run",
                    vec!["--no-run", "--message-format", "short"],
                )
            } else {
                ("cargo test", Vec::new())
            };
            steps.push(PlannedStep {
                label: label.to_string(),
                command: cargo_command("test", package, args.test_filter.as_deref(), &extra),
            });
            steps
        }
        ProjectChecksTask::Detect => Vec::new(),
    }
}

fn cargo_command(
    subcommand: &str,
    package: Option<&str>,
    filter: Option<&str>,
    extra_args: &[&str],
) -> String {
    let mut tokens = vec!["cargo".to_string(), subcommand.to_string()];
    if let Some(package) = package {
        tokens.push("-p".to_string());
        tokens.push(package.to_string());
    }
    tokens.extend(extra_args.iter().map(|arg| (*arg).to_string()));
    if let Some(filter) = filter {
        tokens.push(filter.to_string());
    }
    shlex_join(&tokens)
}

fn plan_go_steps(args: &RunProjectChecksArgs) -> Vec<PlannedStep> {
    let target = args.target.as_deref().unwrap_or("./...");
    match args.task {
        ProjectChecksTask::Build => vec![PlannedStep {
            label: "go build".to_string(),
            command: shlex_join(&["go".to_string(), "build".to_string(), target.to_string()]),
        }],
        ProjectChecksTask::Test => vec![PlannedStep {
            label: "go test".to_string(),
            command: go_test_command(target, args.test_filter.as_deref()),
        }],
        ProjectChecksTask::Verify => {
            if args.quick {
                vec![PlannedStep {
                    label: "go test".to_string(),
                    command: go_test_command(target, args.test_filter.as_deref()),
                }]
            } else {
                vec![
                    PlannedStep {
                        label: "go build".to_string(),
                        command: shlex_join(&[
                            "go".to_string(),
                            "build".to_string(),
                            target.to_string(),
                        ]),
                    },
                    PlannedStep {
                        label: "go test".to_string(),
                        command: go_test_command(target, args.test_filter.as_deref()),
                    },
                ]
            }
        }
        ProjectChecksTask::Detect => Vec::new(),
    }
}

fn go_test_command(target: &str, filter: Option<&str>) -> String {
    let mut tokens = vec!["go".to_string(), "test".to_string(), target.to_string()];
    if let Some(filter) = filter {
        tokens.push("-run".to_string());
        tokens.push(filter.to_string());
    }
    shlex_join(&tokens)
}

fn plan_node_steps(
    args: &RunProjectChecksArgs,
    detection: &ProjectDetection,
) -> Result<Vec<PlannedStep>, FunctionCallError> {
    let package_manager = detection
        .package_manager
        .as_deref()
        .map(parse_node_package_manager)
        .unwrap_or(NodePackageManager::Npm);
    let scripts = &detection.scripts;
    let script = |candidates: &[&str]| {
        candidates
            .iter()
            .find(|candidate| scripts.contains(**candidate))
            .map(|candidate| (*candidate).to_string())
    };

    let steps = match args.task {
        ProjectChecksTask::Build => script(&["build", "typecheck", "check"]).map(|script| {
            vec![PlannedStep {
                label: format!("{script} script"),
                command: package_manager.command_for_script(&script),
            }]
        }),
        ProjectChecksTask::Test => script(&["test"]).map(|script| {
            vec![PlannedStep {
                label: format!("{script} script"),
                command: package_manager.command_for_script(&script),
            }]
        }),
        ProjectChecksTask::Verify => {
            let candidates: &[&str] = if args.quick {
                &["typecheck", "lint", "test"]
            } else {
                &["typecheck", "lint", "build", "test"]
            };
            let selected = candidates
                .iter()
                .copied()
                .filter(|candidate| scripts.contains(*candidate))
                .map(|script| PlannedStep {
                    label: format!("{script} script"),
                    command: package_manager.command_for_script(script),
                })
                .collect::<Vec<_>>();
            (!selected.is_empty()).then_some(selected)
        }
        ProjectChecksTask::Detect => Some(Vec::new()),
    };

    steps.ok_or_else(|| {
        FunctionCallError::RespondToModel(format!(
            "run_project_checks could not find a matching script for {} in package.json",
            match args.task {
                ProjectChecksTask::Build => "build",
                ProjectChecksTask::Test => "test",
                ProjectChecksTask::Verify => "verify",
                ProjectChecksTask::Detect => "detect",
            }
        ))
    })
}

fn plan_python_steps(args: &RunProjectChecksArgs, root: &Path) -> Vec<PlannedStep> {
    let runner = detect_python_runner(root);
    let test_command = {
        let mut command_args = vec!["-m".to_string(), "pytest".to_string()];
        if let Some(filter) = args.test_filter.as_deref() {
            command_args.push("-k".to_string());
            command_args.push(filter.to_string());
        }
        runner.command(&command_args)
    };

    match args.task {
        ProjectChecksTask::Build => vec![PlannedStep {
            label: "python compileall".to_string(),
            command: runner.command(&["-m".to_string(), "compileall".to_string(), ".".to_string()]),
        }],
        ProjectChecksTask::Test => vec![PlannedStep {
            label: "pytest".to_string(),
            command: test_command,
        }],
        ProjectChecksTask::Verify => {
            if args.quick {
                vec![PlannedStep {
                    label: "pytest".to_string(),
                    command: test_command,
                }]
            } else {
                vec![
                    PlannedStep {
                        label: "python compileall".to_string(),
                        command: runner.command(&[
                            "-m".to_string(),
                            "compileall".to_string(),
                            ".".to_string(),
                        ]),
                    },
                    PlannedStep {
                        label: "pytest".to_string(),
                        command: test_command,
                    },
                ]
            }
        }
        ProjectChecksTask::Detect => Vec::new(),
    }
}

async fn run_planned_step(
    session: &std::sync::Arc<Session>,
    turn: &std::sync::Arc<TurnContext>,
    project_root: &Path,
    call_id: &str,
    step: &PlannedStep,
    timeout_ms: Option<u64>,
) -> Result<StepResult, FunctionCallError> {
    let command = session
        .user_shell()
        .derive_exec_args(&step.command, turn.tools_config.allow_login_shell);
    let mut exec_params = ExecParams {
        command: command.clone(),
        cwd: project_root.to_path_buf(),
        expiration: ExecExpiration::Timeout(Duration::from_millis(
            timeout_ms.unwrap_or(DEFAULT_STEP_TIMEOUT_MS),
        )),
        env: create_env(
            &turn.shell_environment_policy,
            Some(session.conversation_id),
        ),
        network: turn.network.clone(),
        sandbox_permissions: SandboxPermissions::UseDefault,
        windows_sandbox_level: turn.windows_sandbox_level,
        justification: Some(format!("Runs project checks step: {}", step.label)),
        arg0: None,
    };

    let dependency_env = session.dependency_env().await;
    if !dependency_env.is_empty() {
        exec_params.env.extend(dependency_env.clone());
    }

    let mut explicit_env_overrides = turn.shell_environment_policy.r#set.clone();
    for key in dependency_env.keys() {
        if let Some(value) = exec_params.env.get(key) {
            explicit_env_overrides.insert(key.clone(), value.clone());
        }
    }

    let request_permission_enabled = session.features().enabled(Feature::RequestPermissions);
    let effective_additional_permissions = super::apply_granted_turn_permissions(
        session.as_ref(),
        exec_params.sandbox_permissions,
        None,
    )
    .await;
    let normalized_additional_permissions = normalize_and_validate_additional_permissions(
        request_permission_enabled,
        turn.approval_policy.value(),
        effective_additional_permissions.sandbox_permissions,
        effective_additional_permissions.additional_permissions,
        effective_additional_permissions.permissions_preapproved,
        &exec_params.cwd,
    )
    .map_err(FunctionCallError::RespondToModel)?;

    let emitter = ToolEmitter::shell(
        exec_params.command.clone(),
        exec_params.cwd.clone(),
        crate::protocol::ExecCommandSource::Agent,
        false,
    );
    let event_ctx = ToolEventCtx::new(session.as_ref(), turn.as_ref(), call_id, None);
    emitter.begin(event_ctx).await;

    let exec_approval_requirement = session
        .services
        .exec_policy
        .create_exec_approval_requirement_for_command(crate::exec_policy::ExecApprovalRequest {
            command: &exec_params.command,
            approval_policy: turn.approval_policy.value(),
            sandbox_policy: turn.sandbox_policy.get(),
            sandbox_permissions: if effective_additional_permissions.permissions_preapproved {
                SandboxPermissions::UseDefault
            } else {
                effective_additional_permissions.sandbox_permissions
            },
            prefix_rule: None,
        })
        .await;

    let request = crate::tools::runtimes::shell::ShellRequest {
        command: exec_params.command.clone(),
        cwd: exec_params.cwd.clone(),
        timeout_ms: exec_params.expiration.timeout_ms(),
        env: exec_params.env.clone(),
        explicit_env_overrides,
        network: exec_params.network.clone(),
        sandbox_permissions: effective_additional_permissions.sandbox_permissions,
        additional_permissions: normalized_additional_permissions,
        justification: exec_params.justification.clone(),
        exec_approval_requirement,
    };
    let tool_ctx = crate::tools::sandboxing::ToolCtx {
        session: session.clone(),
        turn: turn.clone(),
        call_id: call_id.to_string(),
        tool_name: "run_project_checks".to_string(),
    };
    let mut orchestrator = ToolOrchestrator::new();
    let mut runtime = crate::tools::runtimes::shell::ShellRuntime::new();
    let outcome = orchestrator
        .run(
            &mut runtime,
            &request,
            &tool_ctx,
            turn.as_ref(),
            turn.approval_policy.value(),
        )
        .await
        .map(|result| result.output);

    let (stage, result) = step_result_from_outcome(step, outcome);
    emitter.emit(event_ctx, stage).await;
    Ok(result)
}

fn step_result_from_outcome(
    step: &PlannedStep,
    outcome: Result<ExecToolCallOutput, ToolError>,
) -> (ToolEventStage, StepResult) {
    match outcome {
        Ok(output) => {
            let status = if output.exit_code == 0 {
                StepStatus::Passed
            } else {
                StepStatus::Failed
            };
            let result = step_result_from_exec_output(step, status, output.clone(), None);
            (ToolEventStage::Success(output), result)
        }
        Err(ToolError::Codex(CodexErr::Sandbox(SandboxErr::Timeout { output }))) => {
            let output = *output;
            let result =
                step_result_from_exec_output(step, StepStatus::TimedOut, output.clone(), None);
            (
                ToolEventStage::Failure(ToolEventFailure::Output(output)),
                result,
            )
        }
        Err(ToolError::Codex(CodexErr::Sandbox(SandboxErr::Denied { output, .. }))) => {
            let output = *output;
            let result =
                step_result_from_exec_output(step, StepStatus::Failed, output.clone(), None);
            (
                ToolEventStage::Failure(ToolEventFailure::Output(output)),
                result,
            )
        }
        Err(ToolError::Codex(err)) => {
            let message = format!("execution error: {err:?}");
            let result = StepResult {
                label: step.label.clone(),
                command: step.command.clone(),
                status: StepStatus::Error,
                exit_code: None,
                duration_seconds: None,
                timed_out: false,
                lock_wait_count: 0,
                output_excerpt: None,
                error: Some(message.clone()),
            };
            (
                ToolEventStage::Failure(ToolEventFailure::Message(message)),
                result,
            )
        }
        Err(ToolError::Rejected(message)) => {
            let normalized = if message == "rejected by user" {
                "exec command rejected by user".to_string()
            } else {
                message
            };
            let result = StepResult {
                label: step.label.clone(),
                command: step.command.clone(),
                status: StepStatus::Rejected,
                exit_code: None,
                duration_seconds: None,
                timed_out: false,
                lock_wait_count: 0,
                output_excerpt: None,
                error: Some(normalized.clone()),
            };
            (
                ToolEventStage::Failure(ToolEventFailure::Rejected(normalized)),
                result,
            )
        }
    }
}

fn step_result_from_exec_output(
    step: &PlannedStep,
    status: StepStatus,
    output: ExecToolCallOutput,
    error: Option<String>,
) -> StepResult {
    let excerpt_policy = TruncationPolicy::Bytes(STEP_OUTPUT_EXCERPT_BYTES);
    let excerpt = formatted_truncate_text(&output.aggregated_output.text, excerpt_policy);
    StepResult {
        label: step.label.clone(),
        command: step.command.clone(),
        status,
        exit_code: Some(output.exit_code),
        duration_seconds: Some(((output.duration.as_secs_f32()) * 10.0).round() / 10.0),
        timed_out: output.timed_out,
        lock_wait_count: output
            .aggregated_output
            .text
            .matches(CARGO_LOCK_WAIT_TEXT)
            .count(),
        output_excerpt: (!excerpt.is_empty()).then_some(excerpt),
        error,
    }
}

fn build_summary(results: &[StepResult], success: bool, stopped_after_failure: bool) -> String {
    if results.is_empty() {
        return "No steps were executed.".to_string();
    }

    if success {
        return format!("Ran {} step(s) successfully.", results.len());
    }

    let failed = results
        .iter()
        .find(|result| !matches!(result.status, StepStatus::Passed));
    match failed {
        Some(step) if stopped_after_failure => {
            format!(
                "Stopped after {} failed with {:?}.",
                step.label, step.status
            )
        }
        Some(step) => format!("{} finished with {:?}.", step.label, step.status),
        None => "One or more project check steps failed.".to_string(),
    }
}

impl DetectedProjectType {
    fn as_str(self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::Node => "node",
            Self::Python => "python",
            Self::Go => "go",
            Self::Unknown => "unknown",
        }
    }
}

impl RequestedProjectType {
    fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Rust => "rust",
            Self::Node => "node",
            Self::Python => "python",
            Self::Go => "go",
        }
    }
}

impl ProjectChecksTask {
    fn as_str(self) -> &'static str {
        match self {
            Self::Detect => "detect",
            Self::Build => "build",
            Self::Test => "test",
            Self::Verify => "verify",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use tempfile::tempdir;

    #[test]
    fn detect_project_prefers_nearest_rust_marker() {
        let temp = tempdir().expect("tempdir");
        let workspace = temp.path().join("workspace");
        let nested = workspace.join("crates/demo/src");
        std::fs::create_dir_all(&nested).expect("create dirs");
        std::fs::write(workspace.join("Cargo.toml"), "[workspace]\n").expect("write root cargo");
        std::fs::write(
            workspace.join("crates/demo/Cargo.toml"),
            "[package]\nname='demo'\nversion='0.1.0'\n",
        )
        .expect("write nested cargo");

        let detection = detect_project(&nested, RequestedProjectType::Auto).expect("detect");

        assert_eq!(detection.project_type, DetectedProjectType::Rust);
        assert_eq!(detection.project_root, workspace.join("crates/demo"));
    }

    #[test]
    fn detect_node_package_manager_prefers_package_manager_field() {
        let temp = tempdir().expect("tempdir");
        let package_json = temp.path().join("package.json");
        std::fs::write(
            &package_json,
            r#"{"packageManager":"pnpm@10.0.0","scripts":{"test":"vitest"}}"#,
        )
        .expect("write package json");

        let info = read_node_project_info(&package_json).expect("read node info");

        assert_eq!(info.package_manager, "pnpm");
        assert!(info.scripts.contains("test"));
    }

    #[test]
    fn plan_rust_verify_quick_uses_check_and_no_run() {
        let args = RunProjectChecksArgs {
            task: ProjectChecksTask::Verify,
            project_type: RequestedProjectType::Rust,
            target: Some("codex-core".to_string()),
            test_filter: None,
            quick: true,
            continue_on_error: false,
            timeout_ms: None,
        };

        let steps = plan_rust_steps(&args);

        assert_eq!(steps.len(), 2);
        assert!(steps[0].command.contains("cargo check"));
        assert!(steps[1].command.contains("--no-run"));
    }

    #[test]
    fn plan_node_verify_uses_available_scripts_in_order() {
        let detection = ProjectDetection {
            project_type: DetectedProjectType::Node,
            project_root: PathBuf::from("repo"),
            package_manager: Some("npm".to_string()),
            scripts: ["lint", "test"]
                .into_iter()
                .map(ToString::to_string)
                .collect(),
            warnings: Vec::new(),
        };
        let mut warnings = Vec::new();
        let args = RunProjectChecksArgs {
            task: ProjectChecksTask::Verify,
            project_type: RequestedProjectType::Node,
            target: None,
            test_filter: None,
            quick: true,
            continue_on_error: false,
            timeout_ms: None,
        };

        let steps = plan_steps(&args, &detection, &mut warnings).expect("plan steps");

        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].label, "lint script");
        assert_eq!(steps[1].label, "test script");
    }

    #[test]
    fn auto_verify_falls_back_to_descendant_rust_workspace_when_root_node_lacks_scripts() {
        let temp = tempdir().expect("tempdir");
        let repo = temp.path().join("repo");
        let rust_workspace = repo.join("codex-rs");
        std::fs::create_dir_all(&repo).expect("create repo");
        std::fs::create_dir_all(&rust_workspace).expect("create rust workspace");
        std::fs::write(
            repo.join("package.json"),
            r#"{"private":true,"scripts":{"format":"prettier --check .","lint:docs":"markdownlint ."}} "#,
        )
        .expect("write package json");
        std::fs::write(rust_workspace.join("Cargo.toml"), "[workspace]\n").expect("write cargo");

        let args = RunProjectChecksArgs {
            task: ProjectChecksTask::Verify,
            project_type: RequestedProjectType::Auto,
            target: None,
            test_filter: None,
            quick: true,
            continue_on_error: false,
            timeout_ms: None,
        };

        let detection = detect_project(&repo, RequestedProjectType::Auto).expect("detect");
        assert_eq!(detection.project_type, DetectedProjectType::Node);

        let (fallback_detection, steps, warnings) =
            plan_steps_with_fallback(&repo, &args, detection).expect("fallback plan");

        assert_eq!(fallback_detection.project_type, DetectedProjectType::Rust);
        assert_eq!(fallback_detection.project_root, rust_workspace);
        assert_eq!(steps[0].label, "cargo check");
        assert!(warnings.iter().any(|warning| {
            warning.contains("using descendant rust project")
                && warning.contains("could not satisfy task verify")
        }));
    }

    #[test]
    fn serialize_error_response_returns_structured_json() {
        let args = RunProjectChecksArgs {
            task: ProjectChecksTask::Verify,
            project_type: RequestedProjectType::Auto,
            target: None,
            test_filter: None,
            quick: true,
            continue_on_error: false,
            timeout_ms: None,
        };

        let output = serialize_error_response(
            &args,
            PathBuf::from("repo"),
            DetectedProjectType::Node,
            Some("pnpm".to_string()),
            ["format".to_string()].into_iter().collect(),
            vec!["warning".to_string()],
            "planning failed".to_string(),
        )
        .expect("serialize response");
        assert_eq!(output.success, Some(false));

        let value: serde_json::Value =
            serde_json::from_str(&output.into_text()).expect("structured error json");
        assert_eq!(value["success"], false);
        assert_eq!(value["error"], "planning failed");
        assert_eq!(value["detected_project_type"], "node");
        assert_eq!(value["available_scripts"][0], "format");
    }

    #[test]
    fn step_result_detects_lock_wait_lines() {
        let step = PlannedStep {
            label: "cargo check".to_string(),
            command: "cargo check".to_string(),
        };
        let output = ExecToolCallOutput {
            exit_code: 0,
            stdout: crate::exec::StreamOutput::new(String::new()),
            stderr: crate::exec::StreamOutput::new(String::new()),
            aggregated_output: crate::exec::StreamOutput::new(format!(
                "{CARGO_LOCK_WAIT_TEXT}\n{CARGO_LOCK_WAIT_TEXT}\nfinished"
            )),
            duration: Duration::from_secs(2),
            timed_out: false,
        };

        let result = step_result_from_exec_output(&step, StepStatus::Passed, output, None);

        assert_eq!(result.lock_wait_count, 2);
    }
}
