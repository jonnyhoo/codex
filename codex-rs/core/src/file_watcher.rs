//! Watches skill roots for changes and broadcasts coarse-grained
//! `FileWatcherEvent`s that higher-level components react to on the next turn.

use std::collections::HashMap;
use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::RwLock;
use std::time::Duration;

use notify::Event;
use notify::EventKind;
use notify::RecommendedWatcher;
use notify::RecursiveMode;
use notify::Watcher;
use tokio::runtime::Handle;
use tokio::sync::broadcast;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio::time::sleep_until;
use tracing::warn;

use crate::config::Config;
use crate::lsp::LspWatchedFileChange;
use crate::lsp::LspWatchedFileChangeKind;
use crate::skills::SkillsManager;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileWatcherEvent {
    SkillsChanged { paths: Vec<PathBuf> },
    WorkspaceChanged { changes: Vec<LspWatchedFileChange> },
}

struct WatchState {
    skills_root_ref_counts: HashMap<PathBuf, usize>,
    workspace_root_ref_counts: HashMap<PathBuf, usize>,
}

struct FileWatcherInner {
    watcher: RecommendedWatcher,
    watched_paths: HashMap<PathBuf, RecursiveMode>,
}

const WATCHER_THROTTLE_INTERVAL: Duration = Duration::from_secs(10);

/// Coalesces bursts of paths and emits at most once per interval.
struct ThrottledPaths {
    pending: HashSet<PathBuf>,
    next_allowed_at: Instant,
}

impl ThrottledPaths {
    fn new(now: Instant) -> Self {
        Self {
            pending: HashSet::new(),
            next_allowed_at: now,
        }
    }

    fn add(&mut self, paths: Vec<PathBuf>) {
        self.pending.extend(paths);
    }

    fn next_deadline(&self, now: Instant) -> Option<Instant> {
        (!self.pending.is_empty() && now < self.next_allowed_at).then_some(self.next_allowed_at)
    }

    fn take_ready(&mut self, now: Instant) -> Option<Vec<PathBuf>> {
        if self.pending.is_empty() || now < self.next_allowed_at {
            return None;
        }
        Some(self.take_with_next_allowed(now))
    }

    fn take_pending(&mut self, now: Instant) -> Option<Vec<PathBuf>> {
        if self.pending.is_empty() {
            return None;
        }
        Some(self.take_with_next_allowed(now))
    }

    fn take_with_next_allowed(&mut self, now: Instant) -> Vec<PathBuf> {
        let mut paths: Vec<PathBuf> = self.pending.drain().collect();
        paths.sort_unstable_by(|a, b| a.as_os_str().cmp(b.as_os_str()));
        self.next_allowed_at = now + WATCHER_THROTTLE_INTERVAL;
        paths
    }
}

struct ThrottledWorkspaceChanges {
    pending: HashMap<PathBuf, LspWatchedFileChangeKind>,
    next_allowed_at: Instant,
}

impl ThrottledWorkspaceChanges {
    fn new(now: Instant) -> Self {
        Self {
            pending: HashMap::new(),
            next_allowed_at: now,
        }
    }

    fn add(&mut self, changes: Vec<LspWatchedFileChange>) {
        for change in changes {
            self.pending.insert(change.path, change.kind);
        }
    }

    fn next_deadline(&self, now: Instant) -> Option<Instant> {
        (!self.pending.is_empty() && now < self.next_allowed_at).then_some(self.next_allowed_at)
    }

    fn take_ready(&mut self, now: Instant) -> Option<Vec<LspWatchedFileChange>> {
        if self.pending.is_empty() || now < self.next_allowed_at {
            return None;
        }
        Some(self.take_with_next_allowed(now))
    }

    fn take_pending(&mut self, now: Instant) -> Option<Vec<LspWatchedFileChange>> {
        if self.pending.is_empty() {
            return None;
        }
        Some(self.take_with_next_allowed(now))
    }

    fn take_with_next_allowed(&mut self, now: Instant) -> Vec<LspWatchedFileChange> {
        let mut paths: Vec<_> = self.pending.drain().collect();
        paths.sort_unstable_by(|a, b| a.0.as_os_str().cmp(b.0.as_os_str()));
        self.next_allowed_at = now + WATCHER_THROTTLE_INTERVAL;
        paths
            .into_iter()
            .map(|(path, kind)| LspWatchedFileChange { path, kind })
            .collect()
    }
}

pub(crate) struct FileWatcher {
    inner: Option<Mutex<FileWatcherInner>>,
    state: Arc<RwLock<WatchState>>,
    tx: broadcast::Sender<FileWatcherEvent>,
}

pub(crate) struct WatchRegistration {
    file_watcher: std::sync::Weak<FileWatcher>,
    roots: Vec<WatchRegistrationEntry>,
}

#[derive(Clone)]
struct WatchRegistrationEntry {
    root: PathBuf,
    kind: WatchedRootKind,
}

#[derive(Clone, Copy)]
enum WatchedRootKind {
    Skills,
    Workspace,
}

impl Drop for WatchRegistration {
    fn drop(&mut self) {
        if let Some(file_watcher) = self.file_watcher.upgrade() {
            file_watcher.unregister_roots(&self.roots);
        }
    }
}

impl FileWatcher {
    pub(crate) fn new(_codex_home: PathBuf) -> notify::Result<Self> {
        let (raw_tx, raw_rx) = mpsc::unbounded_channel();
        let raw_tx_clone = raw_tx;
        let watcher = notify::recommended_watcher(move |res| {
            let _ = raw_tx_clone.send(res);
        })?;
        let inner = FileWatcherInner {
            watcher,
            watched_paths: HashMap::new(),
        };
        let (tx, _) = broadcast::channel(128);
        let state = Arc::new(RwLock::new(WatchState {
            skills_root_ref_counts: HashMap::new(),
            workspace_root_ref_counts: HashMap::new(),
        }));
        let file_watcher = Self {
            inner: Some(Mutex::new(inner)),
            state: Arc::clone(&state),
            tx: tx.clone(),
        };
        file_watcher.spawn_event_loop(raw_rx, state, tx);
        Ok(file_watcher)
    }

    pub(crate) fn noop() -> Self {
        let (tx, _) = broadcast::channel(1);
        Self {
            inner: None,
            state: Arc::new(RwLock::new(WatchState {
                skills_root_ref_counts: HashMap::new(),
                workspace_root_ref_counts: HashMap::new(),
            })),
            tx,
        }
    }

    pub(crate) fn subscribe(&self) -> broadcast::Receiver<FileWatcherEvent> {
        self.tx.subscribe()
    }

    pub(crate) fn register_config(
        self: &Arc<Self>,
        config: &Config,
        skills_manager: &SkillsManager,
    ) -> WatchRegistration {
        let deduped_roots: HashSet<PathBuf> = skills_manager
            .skill_roots_for_config(config)
            .into_iter()
            .map(|root| root.path)
            .collect();
        let mut registered_roots: Vec<PathBuf> = deduped_roots.into_iter().collect();
        registered_roots.sort_unstable_by(|a, b| a.as_os_str().cmp(b.as_os_str()));
        for root in &registered_roots {
            self.register_skills_root(root.clone());
        }

        WatchRegistration {
            file_watcher: Arc::downgrade(self),
            roots: registered_roots
                .into_iter()
                .map(|root| WatchRegistrationEntry {
                    root,
                    kind: WatchedRootKind::Skills,
                })
                .collect(),
        }
    }

    pub(crate) fn register_workspace_root(self: &Arc<Self>, root: PathBuf) -> WatchRegistration {
        self.register_root(root.clone(), WatchedRootKind::Workspace);
        WatchRegistration {
            file_watcher: Arc::downgrade(self),
            roots: vec![WatchRegistrationEntry {
                root,
                kind: WatchedRootKind::Workspace,
            }],
        }
    }

    // Bridge `notify`'s callback-based events into the Tokio runtime and
    // broadcast coarse-grained change signals to subscribers.
    fn spawn_event_loop(
        &self,
        mut raw_rx: mpsc::UnboundedReceiver<notify::Result<Event>>,
        state: Arc<RwLock<WatchState>>,
        tx: broadcast::Sender<FileWatcherEvent>,
    ) {
        if let Ok(handle) = Handle::try_current() {
            handle.spawn(async move {
                let now = Instant::now();
                let mut skills = ThrottledPaths::new(now);
                let mut workspace = ThrottledWorkspaceChanges::new(now);

                loop {
                    let now = Instant::now();
                    let next_deadline = [skills.next_deadline(now), workspace.next_deadline(now)]
                        .into_iter()
                        .flatten()
                        .min();
                    let timer_deadline = next_deadline
                        .unwrap_or_else(|| now + Duration::from_secs(60 * 60 * 24 * 365));
                    let timer = sleep_until(timer_deadline);
                    tokio::pin!(timer);

                    tokio::select! {
                        res = raw_rx.recv() => {
                            match res {
                                Some(Ok(event)) => {
                                    let classified = classify_event(&event, &state);
                                    let now = Instant::now();
                                    skills.add(classified.skills_paths);
                                    workspace.add(classified.workspace_changes);

                                    if let Some(paths) = skills.take_ready(now) {
                                        let _ = tx.send(FileWatcherEvent::SkillsChanged { paths });
                                    }
                                    if let Some(changes) = workspace.take_ready(now) {
                                        let _ = tx.send(FileWatcherEvent::WorkspaceChanged { changes });
                                    }
                                }
                                Some(Err(err)) => {
                                    warn!("file watcher error: {err}");
                                }
                                None => {
                                    // Flush any pending changes before shutdown so subscribers
                                    // see the latest state.
                                    let now = Instant::now();
                                    if let Some(paths) = skills.take_pending(now) {
                                        let _ = tx.send(FileWatcherEvent::SkillsChanged { paths });
                                    }
                                    if let Some(changes) = workspace.take_pending(now) {
                                        let _ = tx.send(FileWatcherEvent::WorkspaceChanged { changes });
                                    }
                                    break;
                                }
                            }
                        }
                        _ = &mut timer => {
                            let now = Instant::now();
                            if let Some(paths) = skills.take_ready(now) {
                                let _ = tx.send(FileWatcherEvent::SkillsChanged { paths });
                            }
                            if let Some(changes) = workspace.take_ready(now) {
                                let _ = tx.send(FileWatcherEvent::WorkspaceChanged { changes });
                            }
                        }
                    }
                }
            });
        } else {
            warn!("file watcher loop skipped: no Tokio runtime available");
        }
    }

    fn register_skills_root(&self, root: PathBuf) {
        self.register_root(root, WatchedRootKind::Skills);
    }

    fn register_root(&self, root: PathBuf, kind: WatchedRootKind) {
        let mut state = self
            .state
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let counts = root_counts_mut(&mut state, kind);
        let count = counts.entry(root.clone()).or_insert(0);
        *count += 1;
        if *count == 1 {
            self.watch_path(root, RecursiveMode::Recursive);
        }
    }

    fn unregister_roots(&self, roots: &[WatchRegistrationEntry]) {
        let mut state = self
            .state
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut inner_guard: Option<std::sync::MutexGuard<'_, FileWatcherInner>> = None;

        for entry in roots {
            let mut should_unwatch = false;
            let counts = root_counts_mut(&mut state, entry.kind);
            if let Some(count) = counts.get_mut(&entry.root) {
                if *count > 1 {
                    *count -= 1;
                } else {
                    counts.remove(&entry.root);
                    should_unwatch = true;
                }
            }

            if !should_unwatch {
                continue;
            }
            let Some(inner) = &self.inner else {
                continue;
            };
            if inner_guard.is_none() {
                let guard = inner
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                inner_guard = Some(guard);
            }

            let Some(guard) = inner_guard.as_mut() else {
                continue;
            };
            if guard.watched_paths.remove(&entry.root).is_none() {
                continue;
            }
            if let Err(err) = guard.watcher.unwatch(&entry.root) {
                warn!("failed to unwatch {}: {err}", entry.root.display());
            }
        }
    }

    fn watch_path(&self, path: PathBuf, mode: RecursiveMode) {
        let Some(inner) = &self.inner else {
            return;
        };
        if !path.exists() {
            return;
        }
        let watch_path = path;
        let mut guard = inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(existing) = guard.watched_paths.get(&watch_path) {
            if *existing == RecursiveMode::Recursive || *existing == mode {
                return;
            }
            if let Err(err) = guard.watcher.unwatch(&watch_path) {
                warn!("failed to unwatch {}: {err}", watch_path.display());
            }
        }
        if let Err(err) = guard.watcher.watch(&watch_path, mode) {
            warn!("failed to watch {}: {err}", watch_path.display());
            return;
        }
        guard.watched_paths.insert(watch_path, mode);
    }
}

struct ClassifiedEvent {
    skills_paths: Vec<PathBuf>,
    workspace_changes: Vec<LspWatchedFileChange>,
}

fn classify_event(event: &Event, state: &RwLock<WatchState>) -> ClassifiedEvent {
    let Some(default_change_kind) = event_kind_to_workspace_change_kind(event.kind) else {
        return ClassifiedEvent {
            skills_paths: Vec::new(),
            workspace_changes: Vec::new(),
        };
    };

    let (skills_roots, workspace_roots) = match state.read() {
        Ok(state) => (
            state
                .skills_root_ref_counts
                .keys()
                .cloned()
                .collect::<HashSet<_>>(),
            state
                .workspace_root_ref_counts
                .keys()
                .cloned()
                .collect::<HashSet<_>>(),
        ),
        Err(err) => {
            let state = err.into_inner();
            (
                state
                    .skills_root_ref_counts
                    .keys()
                    .cloned()
                    .collect::<HashSet<_>>(),
                state
                    .workspace_root_ref_counts
                    .keys()
                    .cloned()
                    .collect::<HashSet<_>>(),
            )
        }
    };

    let mut skills_paths = Vec::new();
    let mut workspace_changes = Vec::new();
    for (path, change_kind) in workspace_changes_for_event(event, default_change_kind) {
        if is_path_under_roots(&path, &skills_roots) {
            skills_paths.push(path.clone());
        }
        if is_path_under_roots(&path, &workspace_roots) {
            workspace_changes.push(LspWatchedFileChange {
                path,
                kind: change_kind,
            });
        }
    }

    ClassifiedEvent {
        skills_paths,
        workspace_changes,
    }
}

fn is_path_under_roots(path: &Path, roots: &HashSet<PathBuf>) -> bool {
    roots.iter().any(|root| path.starts_with(root))
}

fn root_counts_mut(state: &mut WatchState, kind: WatchedRootKind) -> &mut HashMap<PathBuf, usize> {
    match kind {
        WatchedRootKind::Skills => &mut state.skills_root_ref_counts,
        WatchedRootKind::Workspace => &mut state.workspace_root_ref_counts,
    }
}

fn event_kind_to_workspace_change_kind(kind: EventKind) -> Option<LspWatchedFileChangeKind> {
    match kind {
        EventKind::Create(_) => Some(LspWatchedFileChangeKind::Created),
        EventKind::Modify(_) => Some(LspWatchedFileChangeKind::Changed),
        EventKind::Remove(_) => Some(LspWatchedFileChangeKind::Deleted),
        _ => None,
    }
}

fn workspace_changes_for_event(
    event: &Event,
    default_kind: LspWatchedFileChangeKind,
) -> Vec<(PathBuf, LspWatchedFileChangeKind)> {
    if matches!(
        event.kind,
        EventKind::Modify(notify::event::ModifyKind::Name(_))
    ) && event.paths.len() >= 2
    {
        return vec![
            (event.paths[0].clone(), LspWatchedFileChangeKind::Deleted),
            (event.paths[1].clone(), LspWatchedFileChangeKind::Created),
        ];
    }

    event
        .paths
        .iter()
        .cloned()
        .map(|path| (path, default_kind))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::EventKind;
    use notify::event::AccessKind;
    use notify::event::AccessMode;
    use notify::event::CreateKind;
    use notify::event::ModifyKind;
    use notify::event::RemoveKind;
    use pretty_assertions::assert_eq;
    use tokio::time::timeout;

    fn path(name: &str) -> PathBuf {
        PathBuf::from(name)
    }

    fn notify_event(kind: EventKind, paths: Vec<PathBuf>) -> Event {
        let mut event = Event::new(kind);
        for path in paths {
            event = event.add_path(path);
        }
        event
    }

    #[test]
    fn throttles_and_coalesces_within_interval() {
        let start = Instant::now();
        let mut throttled = ThrottledPaths::new(start);

        throttled.add(vec![path("a")]);
        let first = throttled.take_ready(start).expect("first emit");
        assert_eq!(first, vec![path("a")]);

        throttled.add(vec![path("b"), path("c")]);
        assert_eq!(throttled.take_ready(start), None);

        let second = throttled
            .take_ready(start + WATCHER_THROTTLE_INTERVAL)
            .expect("coalesced emit");
        assert_eq!(second, vec![path("b"), path("c")]);
    }

    #[test]
    fn flushes_pending_on_shutdown() {
        let start = Instant::now();
        let mut throttled = ThrottledPaths::new(start);

        throttled.add(vec![path("a")]);
        let _ = throttled.take_ready(start).expect("first emit");

        throttled.add(vec![path("b")]);
        assert_eq!(throttled.take_ready(start), None);

        let flushed = throttled
            .take_pending(start)
            .expect("shutdown flush emits pending paths");
        assert_eq!(flushed, vec![path("b")]);
    }

    #[test]
    fn classify_event_filters_to_skills_roots() {
        let root = path("/tmp/skills");
        let state = RwLock::new(WatchState {
            skills_root_ref_counts: HashMap::from([(root.clone(), 1)]),
            workspace_root_ref_counts: HashMap::new(),
        });
        let event = notify_event(
            EventKind::Create(CreateKind::Any),
            vec![
                root.join("demo/SKILL.md"),
                path("/tmp/other/not-a-skill.txt"),
            ],
        );

        let classified = classify_event(&event, &state);
        assert_eq!(classified.skills_paths, vec![root.join("demo/SKILL.md")]);
        assert!(classified.workspace_changes.is_empty());
    }

    #[test]
    fn classify_event_supports_multiple_roots_without_prefix_false_positives() {
        let root_a = path("/tmp/skills");
        let root_b = path("/tmp/workspace/.codex/skills");
        let state = RwLock::new(WatchState {
            skills_root_ref_counts: HashMap::from([(root_a.clone(), 1), (root_b.clone(), 1)]),
            workspace_root_ref_counts: HashMap::new(),
        });
        let event = notify_event(
            EventKind::Modify(ModifyKind::Any),
            vec![
                root_a.join("alpha/SKILL.md"),
                path("/tmp/skills-extra/not-under-skills.txt"),
                root_b.join("beta/SKILL.md"),
            ],
        );

        let classified = classify_event(&event, &state);
        assert_eq!(
            classified.skills_paths,
            vec![root_a.join("alpha/SKILL.md"), root_b.join("beta/SKILL.md")]
        );
        assert!(classified.workspace_changes.is_empty());
    }

    #[test]
    fn classify_event_ignores_non_mutating_event_kinds() {
        let root = path("/tmp/skills");
        let state = RwLock::new(WatchState {
            skills_root_ref_counts: HashMap::from([(root.clone(), 1)]),
            workspace_root_ref_counts: HashMap::new(),
        });
        let path = root.join("demo/SKILL.md");

        let access_event = notify_event(
            EventKind::Access(AccessKind::Open(AccessMode::Any)),
            vec![path.clone()],
        );
        let access_classified = classify_event(&access_event, &state);
        assert!(access_classified.skills_paths.is_empty());
        assert!(access_classified.workspace_changes.is_empty());

        let any_event = notify_event(EventKind::Any, vec![path.clone()]);
        let any_classified = classify_event(&any_event, &state);
        assert!(any_classified.skills_paths.is_empty());
        assert!(any_classified.workspace_changes.is_empty());

        let other_event = notify_event(EventKind::Other, vec![path]);
        let other_classified = classify_event(&other_event, &state);
        assert!(other_classified.skills_paths.is_empty());
        assert!(other_classified.workspace_changes.is_empty());
    }

    #[test]
    fn register_skills_root_dedupes_state_entries() {
        let watcher = FileWatcher::noop();
        let root = path("/tmp/skills");
        watcher.register_skills_root(root.clone());
        watcher.register_skills_root(root);
        watcher.register_skills_root(path("/tmp/other-skills"));

        let state = watcher.state.read().expect("state lock");
        assert_eq!(state.skills_root_ref_counts.len(), 2);
    }

    #[test]
    fn watch_registration_drop_unregisters_roots() {
        let watcher = Arc::new(FileWatcher::noop());
        let root = path("/tmp/skills");
        watcher.register_skills_root(root.clone());
        let registration = WatchRegistration {
            file_watcher: Arc::downgrade(&watcher),
            roots: vec![WatchRegistrationEntry {
                root,
                kind: WatchedRootKind::Skills,
            }],
        };

        drop(registration);

        let state = watcher.state.read().expect("state lock");
        assert_eq!(state.skills_root_ref_counts.len(), 0);
    }

    #[test]
    fn unregister_holds_state_lock_until_unwatch_finishes() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let root = temp_dir.path().join("skills");
        std::fs::create_dir(&root).expect("create root");

        let watcher = Arc::new(FileWatcher::new(temp_dir.path().to_path_buf()).expect("watcher"));
        watcher.register_skills_root(root.clone());

        let inner = watcher.inner.as_ref().expect("watcher inner");
        let inner_guard = inner.lock().expect("inner lock");

        let unregister_watcher = Arc::clone(&watcher);
        let unregister_root = root.clone();
        let unregister_thread = std::thread::spawn(move || {
            unregister_watcher.unregister_roots(&[WatchRegistrationEntry {
                root: unregister_root,
                kind: WatchedRootKind::Skills,
            }]);
        });

        let state_lock_observed = (0..100).any(|_| {
            let locked = watcher.state.try_write().is_err();
            if !locked {
                std::thread::sleep(Duration::from_millis(10));
            }
            locked
        });
        assert_eq!(state_lock_observed, true);

        let register_watcher = Arc::clone(&watcher);
        let register_root = root.clone();
        let register_thread = std::thread::spawn(move || {
            register_watcher.register_skills_root(register_root);
        });

        drop(inner_guard);

        unregister_thread.join().expect("unregister join");
        register_thread.join().expect("register join");

        let state = watcher.state.read().expect("state lock");
        assert_eq!(state.skills_root_ref_counts.get(&root), Some(&1));
        drop(state);

        let inner = watcher.inner.as_ref().expect("watcher inner");
        let inner = inner.lock().expect("inner lock");
        assert_eq!(
            inner.watched_paths.get(&root),
            Some(&RecursiveMode::Recursive)
        );
    }

    #[tokio::test]
    async fn spawn_event_loop_flushes_pending_changes_on_shutdown() {
        let watcher = FileWatcher::noop();
        let root = path("/tmp/skills");
        {
            let mut state = watcher.state.write().expect("state lock");
            state.skills_root_ref_counts.insert(root.clone(), 1);
        }

        let (raw_tx, raw_rx) = mpsc::unbounded_channel();
        let (tx, mut rx) = broadcast::channel(8);
        watcher.spawn_event_loop(raw_rx, Arc::clone(&watcher.state), tx);

        raw_tx
            .send(Ok(notify_event(
                EventKind::Create(CreateKind::File),
                vec![root.join("a/SKILL.md")],
            )))
            .expect("send first event");
        let first = timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("first watcher event")
            .expect("broadcast recv first");
        assert_eq!(
            first,
            FileWatcherEvent::SkillsChanged {
                paths: vec![root.join("a/SKILL.md")]
            }
        );

        raw_tx
            .send(Ok(notify_event(
                EventKind::Remove(RemoveKind::File),
                vec![root.join("b/SKILL.md")],
            )))
            .expect("send second event");
        drop(raw_tx);

        let second = timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("second watcher event")
            .expect("broadcast recv second");
        assert_eq!(
            second,
            FileWatcherEvent::SkillsChanged {
                paths: vec![root.join("b/SKILL.md")]
            }
        );
    }

    #[test]
    fn classify_event_emits_workspace_changes_for_workspace_roots() {
        let root = path("/tmp/workspace");
        let state = RwLock::new(WatchState {
            skills_root_ref_counts: HashMap::new(),
            workspace_root_ref_counts: HashMap::from([(root.clone(), 1)]),
        });
        let event = notify_event(
            EventKind::Create(CreateKind::File),
            vec![root.join("src/main.rs"), path("/tmp/other/file.txt")],
        );

        let classified = classify_event(&event, &state);
        assert!(classified.skills_paths.is_empty());
        assert_eq!(
            classified.workspace_changes,
            vec![LspWatchedFileChange {
                path: root.join("src/main.rs"),
                kind: LspWatchedFileChangeKind::Created,
            }]
        );
    }

    #[test]
    fn classify_event_maps_rename_to_delete_and_create_workspace_changes() {
        let root = path("/tmp/workspace");
        let state = RwLock::new(WatchState {
            skills_root_ref_counts: HashMap::new(),
            workspace_root_ref_counts: HashMap::from([(root.clone(), 1)]),
        });
        let event = notify_event(
            EventKind::Modify(ModifyKind::Name(notify::event::RenameMode::Both)),
            vec![root.join("old.rs"), root.join("new.rs")],
        );

        let classified = classify_event(&event, &state);
        assert_eq!(
            classified.workspace_changes,
            vec![
                LspWatchedFileChange {
                    path: root.join("old.rs"),
                    kind: LspWatchedFileChangeKind::Deleted,
                },
                LspWatchedFileChange {
                    path: root.join("new.rs"),
                    kind: LspWatchedFileChangeKind::Created,
                }
            ]
        );
    }
}
