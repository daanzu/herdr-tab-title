use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::env;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

const DEFAULT_INTERVAL_SECS: u64 = 10;
const MAX_INTERVAL_SECS: u64 = 3600;
const EVENT_SYNC_DEBOUNCE_MS: u64 = 250;
const SYNC_LOCK_STALE_SECS: u64 = 30;
const DEFAULT_DIRECTORY_DEPTH: usize = 1;
const MAX_DIRECTORY_DEPTH: usize = 8;
const LABEL_LIMIT: usize = 40;
const IDLE_SHELL_SEPARATOR: &str = " ❯ ";

#[derive(Debug, Clone, PartialEq, Eq)]
enum CliCommand {
    Start(Options),
    Autostart(Options),
    Watch(Options),
    Stop,
    Status,
    Sync(Options),
    Help,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Options {
    interval: Option<Duration>,
    force: bool,
    quiet: bool,
}

#[derive(Debug, Clone)]
struct Paths {
    state_dir: PathBuf,
    config_dir: PathBuf,
    config_file: PathBuf,
    pid_file: PathBuf,
    start_lock_file: PathBuf,
    sync_lock_file: PathBuf,
    event_sync_stamp_file: PathBuf,
    labels_file: PathBuf,
    log_file: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Tab {
    tab_id: String,
    workspace_id: String,
    label: String,
    number: usize,
    display_number: usize,
    focused: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Pane {
    pane_id: String,
    tab_id: String,
    focused: bool,
    cwd: Option<String>,
    foreground_cwd: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IdleLabelMode {
    Directory,
    Shell,
    DirectoryShell,
}

impl IdleLabelMode {
    fn parse(value: &str) -> Result<Self> {
        match value {
            "directory" => Ok(Self::Directory),
            "shell" => Ok(Self::Shell),
            "directory_shell" => Ok(Self::DirectoryShell),
            _ => bail!("idle_label_mode must be one of: directory, shell, directory_shell"),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Directory => "directory",
            Self::Shell => "shell",
            Self::DirectoryShell => "directory_shell",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ForegroundProcess {
    name: String,
    argv0: Option<String>,
    argv: Vec<String>,
    cmdline: Option<String>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct LabelState {
    labels: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PluginConfig {
    interval_seconds: u64,
    directory_depth: usize,
    show_tab_number: bool,
    idle_label_mode: IdleLabelMode,
    idle_shell_separator: String,
    shorten_home_directory: bool,
    set_window_title: bool,
}

#[derive(Debug, Default, Deserialize)]
struct RawPluginConfig {
    interval_seconds: Option<u64>,
    directory_depth: Option<usize>,
    show_tab_number: Option<bool>,
    idle_label_mode: Option<String>,
    idle_shell_separator: Option<String>,
    shorten_home_directory: Option<bool>,
    show_idle_shell: Option<bool>,
    set_window_title: Option<bool>,
}

impl Default for PluginConfig {
    fn default() -> Self {
        Self {
            interval_seconds: DEFAULT_INTERVAL_SECS,
            directory_depth: DEFAULT_DIRECTORY_DEPTH,
            show_tab_number: false,
            idle_label_mode: IdleLabelMode::Shell,
            idle_shell_separator: IDLE_SHELL_SEPARATOR.to_string(),
            shorten_home_directory: false,
            set_window_title: true,
        }
    }
}

fn main() {
    if let Err(err) = run() {
        eprintln!("herdr-tab-title: {err:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let command = parse_args(env::args_os().skip(1).collect())?;
    let paths = Paths::new()?;

    match command {
        CliCommand::Start(options) => start_watcher(&paths, options).map(|_| ()),
        CliCommand::Autostart(mut options) => {
            options.quiet = true;
            autostart(&paths, options)
        }
        CliCommand::Watch(options) => watch_loop(&paths, options),
        CliCommand::Stop => stop_watcher(&paths),
        CliCommand::Status => status(&paths),
        CliCommand::Sync(options) => {
            let _guard = SyncLock::acquire(&paths, Duration::from_secs(2))?
                .ok_or_else(|| anyhow!("another tab title sync is already running"))?;
            let changed = sync_now(&paths, options.force)?;
            println!("synced {changed} tab(s)");
            Ok(())
        }
        CliCommand::Help => {
            print_help();
            Ok(())
        }
    }
}

fn parse_args(args: Vec<OsString>) -> Result<CliCommand> {
    let Some(command) = args.first().and_then(|arg| arg.to_str()) else {
        return Ok(CliCommand::Help);
    };
    let options = parse_options(&args[1..])?;
    match command {
        "start" => Ok(CliCommand::Start(options)),
        "autostart" => Ok(CliCommand::Autostart(options)),
        "watch" => Ok(CliCommand::Watch(options)),
        "stop" => Ok(CliCommand::Stop),
        "status" => Ok(CliCommand::Status),
        "sync" => Ok(CliCommand::Sync(options)),
        "help" | "--help" | "-h" => Ok(CliCommand::Help),
        other => bail!("unknown command {other:?}"),
    }
}

fn parse_options(args: &[OsString]) -> Result<Options> {
    let mut interval = None;
    let mut force = false;
    let mut quiet = false;
    let mut index = 0;

    while index < args.len() {
        let arg = args[index]
            .to_str()
            .ok_or_else(|| anyhow!("option is not valid UTF-8"))?;
        match arg {
            "--interval-seconds" => {
                let value = args
                    .get(index + 1)
                    .and_then(|value| value.to_str())
                    .ok_or_else(|| anyhow!("missing value for --interval-seconds"))?;
                let seconds = value
                    .parse::<u64>()
                    .with_context(|| format!("invalid interval {value:?}"))?;
                if seconds == 0 {
                    bail!("--interval-seconds must be greater than zero");
                }
                interval = Some(Duration::from_secs(seconds));
                index += 2;
            }
            "--force" => {
                force = true;
                index += 1;
            }
            "--quiet" => {
                quiet = true;
                index += 1;
            }
            other => bail!("unknown option {other:?}"),
        }
    }

    Ok(Options {
        interval,
        force,
        quiet,
    })
}

fn print_help() {
    println!("herdr-tab-title commands:");
    println!("  herdr-tab-title start [--interval-seconds N] [--force]");
    println!("  herdr-tab-title stop");
    println!("  herdr-tab-title status");
    println!("  herdr-tab-title sync [--force]");
}

impl Paths {
    fn new() -> Result<Self> {
        let state_dir = env::var_os("HERDR_PLUGIN_STATE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(".herdr-tab-title-state"));
        let config_dir = env::var_os("HERDR_PLUGIN_CONFIG_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| state_dir.join("config"));
        fs::create_dir_all(&state_dir)
            .with_context(|| format!("create state directory {}", state_dir.display()))?;
        fs::create_dir_all(&config_dir)
            .with_context(|| format!("create config directory {}", config_dir.display()))?;
        Ok(Self {
            config_file: config_dir.join("config.toml"),
            config_dir,
            pid_file: state_dir.join("watcher.pid"),
            start_lock_file: state_dir.join("watcher.start.lock"),
            sync_lock_file: state_dir.join("sync.lock"),
            event_sync_stamp_file: state_dir.join("event-sync.stamp"),
            labels_file: state_dir.join("labels.json"),
            log_file: state_dir.join("watcher.log"),
            state_dir,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WatcherStart {
    Started,
    AlreadyRunning,
    AlreadyStarting,
}

fn autostart(paths: &Paths, options: Options) -> Result<()> {
    match start_watcher(paths, options.clone())? {
        WatcherStart::Started | WatcherStart::AlreadyStarting => Ok(()),
        WatcherStart::AlreadyRunning => {
            if let Err(err) = sync_after_event(paths, options.force) {
                eprintln!("event sync failed: {err:#}");
            }
            Ok(())
        }
    }
}

fn start_watcher(paths: &Paths, options: Options) -> Result<WatcherStart> {
    if let Some(pid) = read_pid(&paths.pid_file)? {
        if process_alive(pid) {
            if !options.quiet {
                println!("tab title watcher already running: pid {pid}");
            }
            return Ok(WatcherStart::AlreadyRunning);
        }
        remove_if_exists(&paths.pid_file)?;
    }

    let Some(_guard) = StartLock::acquire(paths)? else {
        if !options.quiet {
            println!("tab title watcher already starting");
        }
        return Ok(WatcherStart::AlreadyStarting);
    };

    if let Some(pid) = read_pid(&paths.pid_file)? {
        if process_alive(pid) {
            if !options.quiet {
                println!("tab title watcher already running: pid {pid}");
            }
            return Ok(WatcherStart::AlreadyRunning);
        }
        remove_if_exists(&paths.pid_file)?;
    }

    let exe = env::current_exe().context("resolve current executable")?;
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&paths.log_file)
        .with_context(|| format!("open log file {}", paths.log_file.display()))?;
    let err = log.try_clone().context("clone log handle")?;

    let mut command = Command::new(exe);
    command
        .arg("watch")
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(err));
    if let Some(interval) = options.interval {
        command
            .arg("--interval-seconds")
            .arg(interval.as_secs().to_string());
    }
    if options.force {
        command.arg("--force");
    }

    detach_command(&mut command);
    let child = command.spawn().context("start watcher process")?;
    write_pid(&paths.pid_file, child.id())?;

    if !options.quiet {
        println!(
            "started tab title watcher: pid {}, state {}",
            child.id(),
            paths.state_dir.display()
        );
    }
    Ok(WatcherStart::Started)
}

fn watch_loop(paths: &Paths, options: Options) -> Result<()> {
    let pid = std::process::id();
    write_pid(&paths.pid_file, pid)?;
    let mut config_cache = ConfigCache::new(paths)?;
    loop {
        if !process_owns_pid_file(paths, pid)? {
            eprintln!("watcher exiting because another watcher became owner");
            return Ok(());
        }
        let config = config_cache.load()?;
        let interval = effective_interval(&options, &config);
        let started = Instant::now();
        match SyncLock::acquire(paths, Duration::ZERO) {
            Ok(Some(_guard)) => {
                if let Err(err) = sync_now_with_config(paths, &config, options.force) {
                    eprintln!("sync failed: {err:#}");
                }
            }
            Ok(None) => {}
            Err(err) => eprintln!("sync failed: {err:#}"),
        }
        let elapsed = started.elapsed();
        if elapsed < interval {
            thread::sleep(interval - elapsed);
        }
    }
}

fn effective_interval(options: &Options, config: &PluginConfig) -> Duration {
    options
        .interval
        .unwrap_or_else(|| Duration::from_secs(config.interval_seconds))
}

fn stop_watcher(paths: &Paths) -> Result<()> {
    let Some(pid) = read_pid(&paths.pid_file)? else {
        println!("tab title watcher is not running");
        return Ok(());
    };
    if !process_alive(pid) {
        remove_if_exists(&paths.pid_file)?;
        println!("tab title watcher is not running");
        return Ok(());
    }

    terminate_process(pid)?;
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if !process_alive(pid) {
            remove_if_exists(&paths.pid_file)?;
            println!("stopped tab title watcher: pid {pid}");
            return Ok(());
        }
        thread::sleep(Duration::from_millis(100));
    }
    println!("sent stop signal to tab title watcher: pid {pid}");
    Ok(())
}

fn status(paths: &Paths) -> Result<()> {
    let pid = read_pid(&paths.pid_file)?;
    let running = pid.is_some_and(process_alive);
    let state = load_label_state(paths).unwrap_or_default();
    let config = load_plugin_config(paths)?;
    let payload = serde_json::json!({
        "running": running,
        "pid": pid,
        "managed_tabs": state.labels.len(),
        "interval_seconds": config.interval_seconds,
        "event_debounce_ms": EVENT_SYNC_DEBOUNCE_MS,
        "directory_depth": config.directory_depth,
        "show_tab_number": config.show_tab_number,
        "idle_label_mode": config.idle_label_mode.as_str(),
        "idle_shell_separator": config.idle_shell_separator,
        "shorten_home_directory": config.shorten_home_directory,
        "set_window_title": config.set_window_title,
        "config_dir": paths.config_dir,
        "config_file": paths.config_file,
        "state_dir": paths.state_dir,
        "log_file": paths.log_file,
    });
    println!("{}", serde_json::to_string_pretty(&payload)?);
    Ok(())
}

fn sync_once(config: &PluginConfig, state: &mut LabelState, force: bool) -> Result<usize> {
    let tabs = list_tabs()?;
    let panes = list_panes()?;
    let panes_by_tab = group_panes_by_tab(panes);
    let observed_tab_ids = tabs
        .iter()
        .map(|tab| tab.tab_id.clone())
        .collect::<HashSet<_>>();
    state
        .labels
        .retain(|tab_id, _| observed_tab_ids.contains(tab_id));

    let mut changed = 0;
    let mut focused_tab_label = None;
    for tab in tabs {
        if tab.focused {
            focused_tab_label = Some(tab.label.clone());
        }
        let Some(tab_panes) = panes_by_tab.get(&tab.tab_id) else {
            continue;
        };
        let manage = should_manage_tab(&tab, state, force);
        if !manage {
            state.labels.remove(&tab.tab_id);
            if let Some(desired) = desired_label_for_manual_tab(&tab, &config) {
                rename_tab(&tab.tab_id, &desired)
                    .with_context(|| format!("rename manual tab {} to {desired:?}", tab.tab_id))?;
                if tab.focused {
                    focused_tab_label = Some(desired);
                }
                changed += 1;
            }
            continue;
        }
        let Some(source_pane) = select_tab_source_pane(&tab, tab_panes)? else {
            continue;
        };
        let desired = desired_label_for_tab(&tab, source_pane, &config)?;
        if desired.is_empty() {
            continue;
        }
        if tab.label != desired {
            rename_tab(&tab.tab_id, &desired)
                .with_context(|| format!("rename tab {} to {desired:?}", tab.tab_id))?;
            changed += 1;
        }
        if tab.focused {
            focused_tab_label = Some(desired.clone());
        }
        state.labels.insert(tab.tab_id.clone(), desired);
    }

    if config.set_window_title {
        if let Some(label) = focused_tab_label {
            if let Err(err) = set_window_title(&label) {
                eprintln!("herdr-tab-title: set window title: {err:#}");
            }
        }
    }

    Ok(changed)
}

fn should_manage_tab(tab: &Tab, state: &LabelState, force: bool) -> bool {
    if force {
        return true;
    }
    if let Some(last) = state.labels.get(&tab.tab_id) {
        return *last == tab.label;
    }
    looks_like_default_numeric_label(&tab.label)
}

fn looks_like_default_numeric_label(label: &str) -> bool {
    !label.is_empty() && label.chars().all(|ch| ch.is_ascii_digit())
}

fn select_tab_source_pane<'a>(tab: &Tab, panes: &'a [Pane]) -> Result<Option<&'a Pane>> {
    if tab.focused {
        if let Some(pane) = panes.iter().find(|pane| pane.focused) {
            return Ok(Some(pane));
        }
    }

    let Some(first) = panes.first() else {
        return Ok(None);
    };
    if panes.len() == 1 {
        return Ok(Some(first));
    }
    if let Ok(focused_pane_id) = layout_focused_pane(&first.pane_id) {
        if let Some(pane) = panes.iter().find(|pane| pane.pane_id == focused_pane_id) {
            return Ok(Some(pane));
        }
    }

    Ok(Some(first))
}

fn desired_label_for_tab(tab: &Tab, pane: &Pane, config: &PluginConfig) -> Result<String> {
    let base = desired_label_for_pane(pane, config)?;
    Ok(format_tab_label(tab.display_number, &base, config))
}

fn desired_label_for_manual_tab(tab: &Tab, config: &PluginConfig) -> Option<String> {
    if !config.show_tab_number {
        return None;
    }
    let desired = format_tab_label(
        tab.display_number,
        strip_tab_number_prefix(&tab.label),
        config,
    );
    (desired != tab.label).then_some(desired)
}

fn desired_label_for_pane(pane: &Pane, config: &PluginConfig) -> Result<String> {
    let processes = pane_process_info(&pane.pane_id)?;
    if let Some(process) = select_foreground_process(&processes) {
        return Ok(sanitize_label(&process_label(process)));
    }

    let cwd = pane
        .foreground_cwd
        .as_deref()
        .or(pane.cwd.as_deref())
        .unwrap_or("/");
    let home = if config.shorten_home_directory {
        home_directory()
    } else {
        None
    };
    let directory =
        directory_label_with_home(cwd, config.directory_depth, home.as_deref(), cfg!(windows));
    let shell = select_idle_shell_process(&processes).map(process_label);
    Ok(format_idle_shell_label(
        &directory,
        shell.as_deref(),
        config.idle_label_mode,
        &config.idle_shell_separator,
    ))
}

fn format_tab_label(display_number: usize, base_label: &str, config: &PluginConfig) -> String {
    if config.show_tab_number {
        sanitize_label(&format!("{display_number}:{base_label}"))
    } else {
        base_label.to_string()
    }
}

fn strip_tab_number_prefix(label: &str) -> &str {
    let Some((prefix, rest)) = label.split_once(':') else {
        return label;
    };
    if !prefix.is_empty() && prefix.chars().all(|ch| ch.is_ascii_digit()) {
        rest.trim_start()
    } else {
        label
    }
}

fn select_foreground_process(processes: &[ForegroundProcess]) -> Option<&ForegroundProcess> {
    processes
        .iter()
        .filter(|process| !is_shell_process(process) && !is_internal_helper_process(process))
        .max_by_key(|process| process_score(process))
}

fn select_idle_shell_process(processes: &[ForegroundProcess]) -> Option<&ForegroundProcess> {
    processes
        .iter()
        .filter(|process| is_shell_process(process))
        .max_by_key(|process| process_score(process))
}

fn process_score(process: &ForegroundProcess) -> u8 {
    let label = process_label(process).to_lowercase();
    if label == process.name.to_lowercase() {
        2
    } else {
        3
    }
}

fn process_label(process: &ForegroundProcess) -> String {
    process
        .argv0
        .as_deref()
        .or_else(|| process.argv.first().map(String::as_str))
        .unwrap_or(&process.name)
        .rsplit(['/', '\\'])
        .find(|part| !part.is_empty())
        .unwrap_or(&process.name)
        .to_string()
}

fn is_shell_process(process: &ForegroundProcess) -> bool {
    let name = process_label(process)
        .trim()
        .trim_end_matches(".exe")
        .trim_end_matches(".cmd")
        .trim_end_matches(".bat")
        .to_lowercase();
    matches!(
        name.as_str(),
        "sh" | "bash"
            | "zsh"
            | "fish"
            | "dash"
            | "ksh"
            | "mksh"
            | "nu"
            | "elvish"
            | "xonsh"
            | "pwsh"
            | "powershell"
            | "cmd"
    )
}

fn is_internal_helper_process(process: &ForegroundProcess) -> bool {
    let name = process_label(process)
        .trim()
        .trim_end_matches(".exe")
        .trim_end_matches(".cmd")
        .trim_end_matches(".bat")
        .to_lowercase();
    matches!(name.as_str(), "exec_bridge")
}

fn directory_label(path: &str, depth: usize) -> String {
    let trimmed = path.trim_end_matches(['/', '\\']);
    let parts = trimmed
        .split(['/', '\\'])
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    if parts.is_empty() {
        "/".to_string()
    } else {
        let depth = depth.max(1);
        let start = parts.len().saturating_sub(depth);
        parts[start..].join("/")
    }
}

fn directory_label_with_home(
    path: &str,
    depth: usize,
    home: Option<&str>,
    case_insensitive: bool,
) -> String {
    let Some(home) = home else {
        return directory_label(path, depth);
    };
    let Some(relative) = relative_home_path(path, home, case_insensitive) else {
        return directory_label(path, depth);
    };
    if relative.is_empty() {
        "~".to_string()
    } else {
        format!("~/{}", directory_label(&relative, depth))
    }
}

fn relative_home_path(path: &str, home: &str, case_insensitive: bool) -> Option<String> {
    let path = normalize_path_for_comparison(path);
    let home = normalize_path_for_comparison(home);
    let prefix = if home == "/" {
        "/".to_string()
    } else {
        format!("{home}/")
    };
    let is_home = |candidate: &str, expected: &str| {
        if case_insensitive {
            candidate.eq_ignore_ascii_case(expected)
        } else {
            candidate == expected
        }
    };
    if is_home(&path, &home) {
        Some(String::new())
    } else if is_home_prefix(&path, &prefix, case_insensitive) {
        Some(path[prefix.len()..].to_string())
    } else {
        None
    }
}

fn is_home_prefix(path: &str, prefix: &str, case_insensitive: bool) -> bool {
    if path.len() < prefix.len() {
        return false;
    }
    let candidate = &path[..prefix.len()];
    if case_insensitive {
        candidate.eq_ignore_ascii_case(prefix)
    } else {
        candidate == prefix
    }
}

fn normalize_path_for_comparison(path: &str) -> String {
    let mut normalized = path.replace('\\', "/");
    while normalized.ends_with('/') && normalized.len() > 1 {
        normalized.pop();
    }
    normalized
}

fn home_directory() -> Option<String> {
    if cfg!(windows) {
        env::var("USERPROFILE")
            .ok()
            .or_else(|| env::var("HOME").ok())
    } else {
        env::var("HOME")
            .ok()
            .or_else(|| env::var("USERPROFILE").ok())
    }
}

fn format_idle_shell_label(
    directory: &str,
    shell: Option<&str>,
    mode: IdleLabelMode,
    separator: &str,
) -> String {
    let label = match (mode, shell) {
        (IdleLabelMode::Directory, _) | (_, None) => directory.to_string(),
        (IdleLabelMode::Shell, Some(shell)) => shell.to_string(),
        (IdleLabelMode::DirectoryShell, Some(shell)) => {
            format!("{directory}{separator}{shell}")
        }
    };
    sanitize_label(&label)
}

fn sanitize_label(label: &str) -> String {
    let mut normalized = String::new();
    let mut last_was_space = false;
    for ch in label.trim().chars() {
        let ch = if ch.is_control() { ' ' } else { ch };
        if ch.is_whitespace() {
            if !last_was_space {
                normalized.push(' ');
            }
            last_was_space = true;
        } else {
            normalized.push(ch);
            last_was_space = false;
        }
    }

    let mut out = normalized.trim().to_string();
    if out.chars().count() > LABEL_LIMIT {
        out = out.chars().take(LABEL_LIMIT.saturating_sub(3)).collect();
        out.push_str("...");
    }
    out
}

fn group_panes_by_tab(panes: Vec<Pane>) -> HashMap<String, Vec<Pane>> {
    let mut grouped: HashMap<String, Vec<Pane>> = HashMap::new();
    for pane in panes {
        grouped.entry(pane.tab_id.clone()).or_default().push(pane);
    }
    for panes in grouped.values_mut() {
        panes.sort_by(|a, b| a.pane_id.cmp(&b.pane_id));
    }
    grouped
}

fn list_tabs() -> Result<Vec<Tab>> {
    let json = herdr_json(&["tab", "list"])?;
    let tabs = json
        .pointer("/result/tabs")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("tab list response missing result.tabs"))?;

    let mut tabs = tabs
        .iter()
        .map(|tab| {
            Ok(Tab {
                tab_id: required_string(tab, "tab_id")?,
                workspace_id: required_string(tab, "workspace_id")?,
                label: required_string(tab, "label")?,
                number: required_u64(tab, "number")? as usize,
                display_number: 0,
                focused: optional_bool(tab, "focused"),
            })
        })
        .collect::<Result<Vec<_>>>()?;

    let mut workspace_counts = HashMap::<String, usize>::new();
    for tab in &mut tabs {
        let count = workspace_counts
            .entry(tab.workspace_id.clone())
            .or_default();
        *count += 1;
        tab.display_number = *count;
    }

    Ok(tabs)
}

fn list_panes() -> Result<Vec<Pane>> {
    let json = herdr_json(&["pane", "list"])?;
    let panes = json
        .pointer("/result/panes")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("pane list response missing result.panes"))?;

    panes
        .iter()
        .map(|pane| {
            Ok(Pane {
                pane_id: required_string(pane, "pane_id")?,
                tab_id: required_string(pane, "tab_id")?,
                focused: optional_bool(pane, "focused"),
                cwd: optional_string(pane, "cwd"),
                foreground_cwd: optional_string(pane, "foreground_cwd"),
            })
        })
        .collect()
}

fn layout_focused_pane(pane_id: &str) -> Result<String> {
    let json = herdr_json(&["pane", "layout", "--pane", pane_id])?;
    json.pointer("/result/layout/focused_pane_id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| anyhow!("pane layout response missing focused_pane_id"))
}

fn pane_process_info(pane_id: &str) -> Result<Vec<ForegroundProcess>> {
    let json = herdr_json(&["pane", "process-info", "--pane", pane_id])?;
    let processes = json
        .pointer("/result/process_info/foreground_processes")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("process-info response missing foreground_processes"))?;

    processes
        .iter()
        .map(|process| {
            Ok(ForegroundProcess {
                name: required_string(process, "name")?,
                argv0: optional_string(process, "argv0"),
                argv: process
                    .get("argv")
                    .and_then(Value::as_array)
                    .map(|argv| {
                        argv.iter()
                            .filter_map(Value::as_str)
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default(),
                cmdline: optional_string(process, "cmdline"),
            })
        })
        .collect()
}

fn rename_tab(tab_id: &str, label: &str) -> Result<()> {
    let _ = herdr_json(&["tab", "rename", tab_id, label])?;
    Ok(())
}

fn set_window_title(tab_label: &str) -> Result<()> {
    let title = window_title_for_tab(tab_label);
    let _ = herdr_json(&["terminal", "title", "set", &title])?;
    Ok(())
}

fn window_title_for_tab(tab_label: &str) -> String {
    format!("Herdr · {tab_label}")
}

fn herdr_json(args: &[&str]) -> Result<Value> {
    let herdr = env::var_os("HERDR_BIN_PATH").unwrap_or_else(|| OsString::from("herdr"));
    let output = Command::new(&herdr)
        .args(args)
        .output()
        .with_context(|| format!("run {} {}", PathBuf::from(&herdr).display(), args.join(" ")))?;

    if !output.status.success() {
        bail!(
            "herdr {} failed with status {}: {}",
            args.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    let value: Value = serde_json::from_slice(&output.stdout)
        .with_context(|| format!("parse herdr {} JSON response", args.join(" ")))?;
    if let Some(error) = value.get("error") {
        bail!("herdr {} returned error: {error}", args.join(" "));
    }
    Ok(value)
}

fn required_string(value: &Value, field: &str) -> Result<String> {
    value
        .get(field)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| anyhow!("missing string field {field}"))
}

fn optional_string(value: &Value, field: &str) -> Option<String> {
    value.get(field).and_then(Value::as_str).map(str::to_string)
}

fn required_u64(value: &Value, field: &str) -> Result<u64> {
    value
        .get(field)
        .and_then(Value::as_u64)
        .ok_or_else(|| anyhow!("missing integer field {field}"))
}

fn optional_bool(value: &Value, field: &str) -> bool {
    value.get(field).and_then(Value::as_bool).unwrap_or(false)
}

fn load_label_state(paths: &Paths) -> Result<LabelState> {
    if !paths.labels_file.is_file() {
        return Ok(LabelState::default());
    }
    let mut text = String::new();
    File::open(&paths.labels_file)
        .with_context(|| format!("open {}", paths.labels_file.display()))?
        .read_to_string(&mut text)
        .with_context(|| format!("read {}", paths.labels_file.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parse {}", paths.labels_file.display()))
}

fn load_plugin_config(paths: &Paths) -> Result<PluginConfig> {
    if !paths.config_file.is_file() {
        return Ok(PluginConfig::default());
    }
    let mut text = String::new();
    File::open(&paths.config_file)
        .with_context(|| format!("open {}", paths.config_file.display()))?
        .read_to_string(&mut text)
        .with_context(|| format!("read {}", paths.config_file.display()))?;
    parse_plugin_config(&text).with_context(|| format!("parse {}", paths.config_file.display()))
}

fn parse_plugin_config(text: &str) -> Result<PluginConfig> {
    let raw: RawPluginConfig = toml::from_str(text)?;
    let mut config = PluginConfig::default();
    if let Some(interval_seconds) = raw.interval_seconds {
        if interval_seconds == 0 || interval_seconds > MAX_INTERVAL_SECS {
            bail!("interval_seconds must be between 1 and {MAX_INTERVAL_SECS}");
        }
        config.interval_seconds = interval_seconds;
    }
    if let Some(directory_depth) = raw.directory_depth {
        if directory_depth == 0 || directory_depth > MAX_DIRECTORY_DEPTH {
            bail!("directory_depth must be between 1 and {MAX_DIRECTORY_DEPTH}");
        }
        config.directory_depth = directory_depth;
    }
    if let Some(show_tab_number) = raw.show_tab_number {
        config.show_tab_number = show_tab_number;
    }
    if let Some(idle_label_mode) = raw.idle_label_mode {
        config.idle_label_mode = IdleLabelMode::parse(&idle_label_mode)?;
    } else if let Some(show_idle_shell) = raw.show_idle_shell {
        config.idle_label_mode = if show_idle_shell {
            IdleLabelMode::DirectoryShell
        } else {
            IdleLabelMode::Directory
        };
    }
    if let Some(idle_shell_separator) = raw.idle_shell_separator {
        config.idle_shell_separator = idle_shell_separator;
    }
    if let Some(shorten_home_directory) = raw.shorten_home_directory {
        config.shorten_home_directory = shorten_home_directory;
    }
    if let Some(set_window_title) = raw.set_window_title {
        config.set_window_title = set_window_title;
    }
    Ok(config)
}

struct ConfigCache {
    path: PathBuf,
    modified: Option<SystemTime>,
    config: PluginConfig,
}

impl ConfigCache {
    fn new(paths: &Paths) -> Result<Self> {
        let config = load_plugin_config(paths)?;
        let modified = file_modified(&paths.config_file)?;
        Ok(Self {
            path: paths.config_file.clone(),
            modified,
            config,
        })
    }

    fn load(&mut self) -> Result<PluginConfig> {
        let modified = file_modified(&self.path)?;
        if modified != self.modified {
            self.config = if self.path.is_file() {
                let mut text = String::new();
                File::open(&self.path)
                    .with_context(|| format!("open {}", self.path.display()))?
                    .read_to_string(&mut text)
                    .with_context(|| format!("read {}", self.path.display()))?;
                parse_plugin_config(&text)
                    .with_context(|| format!("parse {}", self.path.display()))?
            } else {
                PluginConfig::default()
            };
            self.modified = modified;
        }
        Ok(self.config.clone())
    }
}

fn file_modified(path: &Path) -> Result<Option<SystemTime>> {
    match fs::metadata(path) {
        Ok(metadata) => Ok(metadata.modified().ok()),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).with_context(|| format!("stat {}", path.display())),
    }
}

fn save_label_state(paths: &Paths, state: &LabelState) -> Result<()> {
    let filename = paths
        .labels_file
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("labels.json");
    let tmp = paths
        .labels_file
        .with_file_name(format!(".{filename}.{}.tmp", std::process::id()));
    let json = serde_json::to_vec_pretty(state)?;
    fs::write(&tmp, json).with_context(|| format!("write {}", tmp.display()))?;
    fs::rename(&tmp, &paths.labels_file)
        .with_context(|| format!("replace {}", paths.labels_file.display()))
}

fn save_label_state_if_changed(
    paths: &Paths,
    previous: &LabelState,
    current: &LabelState,
) -> Result<()> {
    if previous == current {
        return Ok(());
    }
    save_label_state(paths, current)
}

fn sync_now(paths: &Paths, force: bool) -> Result<usize> {
    let config = load_plugin_config(paths)?;
    sync_now_with_config(paths, &config, force)
}

fn sync_now_with_config(paths: &Paths, config: &PluginConfig, force: bool) -> Result<usize> {
    let mut state = load_label_state(paths)?;
    let previous_state = state.clone();
    let changed = sync_once(config, &mut state, force)?;
    save_label_state_if_changed(paths, &previous_state, &state)?;
    Ok(changed)
}

fn sync_after_event(paths: &Paths, force: bool) -> Result<()> {
    if event_sync_recent(paths)? {
        return Ok(());
    }
    let Some(_guard) = SyncLock::acquire(paths, Duration::ZERO)? else {
        return Ok(());
    };
    if event_sync_recent(paths)? {
        return Ok(());
    }
    sync_now(paths, force)?;
    mark_event_sync(paths)
}

fn event_sync_recent(paths: &Paths) -> Result<bool> {
    let Some(modified) = file_modified(&paths.event_sync_stamp_file)? else {
        return Ok(false);
    };
    match SystemTime::now().duration_since(modified) {
        Ok(elapsed) => Ok(elapsed < Duration::from_millis(EVENT_SYNC_DEBOUNCE_MS)),
        Err(_) => Ok(true),
    }
}

fn mark_event_sync(paths: &Paths) -> Result<()> {
    fs::write(
        &paths.event_sync_stamp_file,
        format!("{}\n", std::process::id()),
    )
    .with_context(|| format!("write {}", paths.event_sync_stamp_file.display()))
}

fn process_owns_pid_file(paths: &Paths, pid: u32) -> Result<bool> {
    Ok(read_pid(&paths.pid_file)?.is_some_and(|owner| owner == pid))
}

struct StartLock {
    path: PathBuf,
}

impl StartLock {
    fn acquire(paths: &Paths) -> Result<Option<Self>> {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&paths.start_lock_file)
            {
                Ok(mut file) => {
                    writeln!(file, "{}", std::process::id())
                        .with_context(|| format!("write {}", paths.start_lock_file.display()))?;
                    return Ok(Some(Self {
                        path: paths.start_lock_file.clone(),
                    }));
                }
                Err(err) if err.kind() == ErrorKind::AlreadyExists => {
                    if let Some(pid) = read_pid(&paths.pid_file)? {
                        if process_alive(pid) {
                            return Ok(None);
                        }
                    }
                    if Instant::now() >= deadline {
                        remove_if_exists(&paths.start_lock_file)?;
                    } else {
                        thread::sleep(Duration::from_millis(50));
                    }
                }
                Err(err) => {
                    return Err(err)
                        .with_context(|| format!("create {}", paths.start_lock_file.display()));
                }
            }
        }
    }
}

impl Drop for StartLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

struct SyncLock {
    path: PathBuf,
}

impl SyncLock {
    fn acquire(paths: &Paths, wait: Duration) -> Result<Option<Self>> {
        let deadline = Instant::now() + wait;
        loop {
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&paths.sync_lock_file)
            {
                Ok(mut file) => {
                    writeln!(file, "{}", std::process::id())
                        .with_context(|| format!("write {}", paths.sync_lock_file.display()))?;
                    return Ok(Some(Self {
                        path: paths.sync_lock_file.clone(),
                    }));
                }
                Err(err) if err.kind() == ErrorKind::AlreadyExists => {
                    remove_stale_sync_lock(&paths.sync_lock_file)?;
                    if wait.is_zero() || Instant::now() >= deadline {
                        return Ok(None);
                    }
                    thread::sleep(Duration::from_millis(25));
                }
                Err(err) => {
                    return Err(err)
                        .with_context(|| format!("create {}", paths.sync_lock_file.display()));
                }
            }
        }
    }
}

impl Drop for SyncLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn remove_stale_sync_lock(path: &Path) -> Result<()> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err).with_context(|| format!("stat {}", path.display())),
    };
    let stale = metadata
        .modified()
        .ok()
        .and_then(|modified| modified.elapsed().ok())
        .is_some_and(|age| age > Duration::from_secs(SYNC_LOCK_STALE_SECS));
    if stale {
        remove_if_exists(path)?;
    }
    Ok(())
}

fn read_pid(path: &Path) -> Result<Option<u32>> {
    let Ok(text) = fs::read_to_string(path) else {
        return Ok(None);
    };
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let pid = trimmed
        .parse::<u32>()
        .with_context(|| format!("parse pid file {}", path.display()))?;
    Ok(Some(pid))
}

fn write_pid(path: &Path, pid: u32) -> Result<()> {
    fs::write(path, format!("{pid}\n")).with_context(|| format!("write {}", path.display()))
}

fn remove_if_exists(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| format!("remove {}", path.display())),
    }
}

#[cfg(unix)]
fn process_alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

#[cfg(windows)]
fn process_alive(pid: u32) -> bool {
    use std::os::windows::process::CommandExt;

    let filter = format!("PID eq {pid}");
    Command::new("tasklist")
        .args(["/FI", filter.as_str(), "/FO", "CSV", "/NH"])
        .creation_flags(0x0800_0000)
        .output()
        .map(|output| output.status.success() && tasklist_contains_pid(&output.stdout, pid))
        .unwrap_or(false)
}

#[cfg(windows)]
fn tasklist_contains_pid(output: &[u8], pid: u32) -> bool {
    String::from_utf8_lossy(output).lines().any(|line| {
        line.split(',')
            .nth(1)
            .and_then(|value| value.trim().trim_matches('"').parse::<u32>().ok())
            == Some(pid)
    })
}

#[cfg(unix)]
fn terminate_process(pid: u32) -> Result<()> {
    let rc = unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error()).with_context(|| format!("terminate pid {pid}"))
    }
}

#[cfg(windows)]
fn terminate_process(pid: u32) -> Result<()> {
    use std::os::windows::process::CommandExt;

    let pid_arg = pid.to_string();
    let output = Command::new("taskkill")
        .args(["/PID", pid_arg.as_str(), "/T", "/F"])
        .creation_flags(0x0800_0000)
        .output()
        .with_context(|| format!("terminate pid {pid}"))?;
    if output.status.success() {
        Ok(())
    } else {
        bail!(
            "taskkill failed for pid {pid}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )
    }
}

#[cfg(unix)]
fn detach_command(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(windows)]
fn detach_command(command: &mut Command) {
    use std::os::windows::process::CommandExt;

    // Do not create a console for the long-lived watcher, and put it in its
    // own process group so it is independent of the plugin action process.
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn process(name: &str, argv0: Option<&str>, argv: &[&str]) -> ForegroundProcess {
        ForegroundProcess {
            name: name.to_string(),
            argv0: argv0.map(str::to_string),
            argv: argv.iter().map(|value| value.to_string()).collect(),
            cmdline: None,
        }
    }

    #[test]
    fn directory_label_uses_last_path_component() {
        assert_eq!(directory_label("/home/me/project", 1), "project");
        assert_eq!(directory_label("/home/me/project/", 1), "project");
        assert_eq!(directory_label("/", 1), "/");
    }

    #[test]
    fn directory_label_uses_requested_tail_components() {
        assert_eq!(directory_label("/home/me/project", 2), "me/project");
        assert_eq!(directory_label("/home/me/project", 99), "home/me/project");
        assert_eq!(directory_label("C:\\Users\\me\\project", 2), "me/project");
    }

    #[test]
    fn plugin_config_controls_directory_depth() {
        assert_eq!(PluginConfig::default().interval_seconds, 10);
        assert_eq!(PluginConfig::default().directory_depth, 1);
        assert!(!PluginConfig::default().show_tab_number);
        assert_eq!(
            PluginConfig::default().idle_label_mode,
            IdleLabelMode::Shell
        );
        assert_eq!(PluginConfig::default().idle_shell_separator, " ❯ ");
        assert!(!PluginConfig::default().shorten_home_directory);
        assert!(PluginConfig::default().set_window_title);
        assert_eq!(parse_plugin_config("").unwrap().directory_depth, 1);
        assert_eq!(
            parse_plugin_config("directory_depth = 2")
                .unwrap()
                .directory_depth,
            2
        );
        assert!(parse_plugin_config("directory_depth = 0").is_err());
        assert!(parse_plugin_config("directory_depth = 99").is_err());
    }

    #[test]
    fn plugin_config_controls_poll_interval() {
        assert_eq!(parse_plugin_config("").unwrap().interval_seconds, 10);
        assert_eq!(
            parse_plugin_config("interval_seconds = 30")
                .unwrap()
                .interval_seconds,
            30
        );
        assert!(parse_plugin_config("interval_seconds = 0").is_err());
        assert!(parse_plugin_config("interval_seconds = 3601").is_err());
    }

    #[test]
    fn plugin_config_controls_tab_number_prefix() {
        assert!(
            parse_plugin_config("show_tab_number = true")
                .unwrap()
                .show_tab_number
        );
        assert_eq!(
            format_tab_label(
                3,
                "me/project",
                &PluginConfig {
                    interval_seconds: 10,
                    directory_depth: 2,
                    show_tab_number: true,
                    idle_label_mode: IdleLabelMode::Directory,
                    idle_shell_separator: " ❯ ".into(),
                    shorten_home_directory: false,
                    set_window_title: false,
                },
            ),
            "3:me/project"
        );
        assert_eq!(
            format_tab_label(
                3,
                "me/project",
                &PluginConfig {
                    interval_seconds: 10,
                    directory_depth: 2,
                    show_tab_number: false,
                    idle_label_mode: IdleLabelMode::Directory,
                    idle_shell_separator: " ❯ ".into(),
                    shorten_home_directory: false,
                    set_window_title: false,
                },
            ),
            "me/project"
        );
        assert_eq!(strip_tab_number_prefix("3:me/project"), "me/project");
        assert_eq!(strip_tab_number_prefix("3: me/project"), "me/project");
        assert_eq!(strip_tab_number_prefix("work:api"), "work:api");
    }

    #[test]
    fn plugin_config_controls_window_title() {
        assert!(
            parse_plugin_config("set_window_title = true")
                .unwrap()
                .set_window_title
        );
    }

    #[test]
    fn plugin_config_controls_idle_shell_labels() {
        let config =
            parse_plugin_config("idle_label_mode = \"shell\"\nidle_shell_separator = \" › \"")
                .unwrap();
        assert_eq!(config.idle_label_mode, IdleLabelMode::Shell);
        assert_eq!(config.idle_shell_separator, " › ");
        assert!(
            parse_plugin_config("shorten_home_directory = true")
                .unwrap()
                .shorten_home_directory
        );
        assert_eq!(
            parse_plugin_config("show_idle_shell = false")
                .unwrap()
                .idle_label_mode,
            IdleLabelMode::Directory
        );
        assert!(parse_plugin_config("idle_label_mode = \"unknown\"").is_err());
    }

    #[test]
    fn window_title_uses_herdr_prefix_and_tab_label() {
        let label = "3:herdr-tab-title";
        assert_eq!(window_title_for_tab(label), "Herdr · 3:herdr-tab-title");
    }

    #[test]
    fn manual_titles_keep_text_but_get_visual_tab_number() {
        let config = PluginConfig {
            interval_seconds: 10,
            directory_depth: 2,
            show_tab_number: true,
            idle_label_mode: IdleLabelMode::Directory,
            idle_shell_separator: " ❯ ".into(),
            shorten_home_directory: false,
            set_window_title: false,
        };
        let manual = Tab {
            tab_id: "w1:t2".into(),
            workspace_id: "w1".into(),
            label: "herdr sauce".into(),
            number: 2,
            display_number: 2,
            focused: false,
        };
        assert_eq!(
            desired_label_for_manual_tab(&manual, &config),
            Some("2:herdr sauce".into())
        );

        let already_numbered = Tab {
            label: "9:herdr sauce".into(),
            ..manual
        };
        assert_eq!(
            desired_label_for_manual_tab(&already_numbered, &config),
            Some("2:herdr sauce".into())
        );
    }

    #[test]
    fn process_label_prefers_executable_basename() {
        let p = process("python3", Some("/usr/bin/python3"), &["python3", "app.py"]);
        assert_eq!(process_label(&p), "python3");
    }

    #[test]
    fn idle_shell_label_combines_directory_and_shell() {
        assert_eq!(
            format_idle_shell_label(
                "me/project",
                Some("bash"),
                IdleLabelMode::DirectoryShell,
                " ❯ ",
            ),
            "me/project ❯ bash"
        );
        assert_eq!(
            format_idle_shell_label("project", Some("bash"), IdleLabelMode::Directory, " ❯ "),
            "project"
        );
        assert_eq!(
            format_idle_shell_label("project", Some("bash"), IdleLabelMode::Shell, " ❯ "),
            "bash"
        );
        assert_eq!(
            format_idle_shell_label("project", None, IdleLabelMode::DirectoryShell, " ❯ "),
            "project"
        );
    }

    #[test]
    fn directory_label_can_shorten_home_path() {
        assert_eq!(
            directory_label_with_home("/home/me/project/src", 2, Some("/home/me"), false,),
            "~/project/src"
        );
        assert_eq!(
            directory_label_with_home("/home/medium/project", 2, Some("/home/me"), false),
            "medium/project"
        );
        assert_eq!(
            directory_label_with_home("C:\\Users\\Me\\project", 2, Some("c:/users/me"), true),
            "~/project"
        );
    }

    #[test]
    fn shell_processes_are_idle_sources() {
        assert!(is_shell_process(&process("bash", Some("bash"), &["bash"])));
        assert!(is_shell_process(&process("zsh", None, &["/bin/zsh"])));
        assert!(!is_shell_process(&process(
            "cargo",
            None,
            &["cargo", "test"]
        )));
    }

    #[test]
    fn select_foreground_process_skips_shells() {
        let processes = vec![
            process("bash", None, &["bash"]),
            process("cargo", None, &["cargo", "test"]),
        ];
        assert_eq!(
            select_foreground_process(&processes).map(process_label),
            Some("cargo".to_string())
        );
    }

    #[test]
    fn select_idle_shell_process_finds_shell() {
        let processes = vec![
            process("bash", None, &["bash"]),
            process("cargo", None, &["cargo", "test"]),
        ];
        assert_eq!(
            select_idle_shell_process(&processes).map(process_label),
            Some("bash".to_string())
        );
    }

    #[test]
    fn select_foreground_process_skips_internal_helpers() {
        let processes = vec![
            process("pi", None, &["pi"]),
            process(
                "exec_bridge",
                Some("/tmp/pi/tools/exec/bin/linux-x64/exec_bridge"),
                &["/tmp/pi/tools/exec/bin/linux-x64/exec_bridge"],
            ),
        ];
        assert_eq!(
            select_foreground_process(&processes).map(process_label),
            Some("pi".to_string())
        );
    }

    #[test]
    fn sanitize_label_collapses_whitespace_controls_and_caps() {
        assert_eq!(sanitize_label("  cargo\n test\t"), "cargo test");
        let long = "a".repeat(80);
        assert_eq!(sanitize_label(&long).chars().count(), LABEL_LIMIT);
        assert!(sanitize_label(&long).ends_with("..."));
    }

    #[test]
    fn manual_name_policy_allows_default_or_last_managed_label() {
        let tab = Tab {
            tab_id: "w1:t1".into(),
            workspace_id: "w1".into(),
            label: "1".into(),
            number: 1,
            display_number: 1,
            focused: false,
        };
        let mut state = LabelState::default();
        assert!(should_manage_tab(&tab, &state, false));

        state.labels.insert("w1:t1".into(), "cargo".into());
        let managed = Tab {
            label: "cargo".into(),
            ..tab.clone()
        };
        assert!(should_manage_tab(&managed, &state, false));

        let manual = Tab {
            label: "logs".into(),
            ..tab
        };
        assert!(!should_manage_tab(&manual, &state, false));
        assert!(should_manage_tab(&manual, &state, true));
    }

    #[test]
    fn manual_name_policy_allows_compact_default_numeric_labels() {
        let tab = Tab {
            tab_id: "w1:t17".into(),
            workspace_id: "w1".into(),
            label: "6".into(),
            number: 17,
            display_number: 6,
            focused: false,
        };
        assert!(should_manage_tab(&tab, &LabelState::default(), false));
    }

    #[test]
    fn groups_panes_by_tab_and_sorts_by_pane_id() {
        let panes = vec![
            Pane {
                pane_id: "w1:p2".into(),
                tab_id: "w1:t1".into(),
                focused: false,
                cwd: None,
                foreground_cwd: None,
            },
            Pane {
                pane_id: "w1:p1".into(),
                tab_id: "w1:t1".into(),
                focused: true,
                cwd: None,
                foreground_cwd: None,
            },
        ];
        let grouped = group_panes_by_tab(panes);
        assert_eq!(grouped["w1:t1"][0].pane_id, "w1:p1");
    }
}
