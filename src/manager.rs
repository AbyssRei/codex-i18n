//! Create validated independent copies and atomically switch the active record.
use crate::{patch, platform};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::windows::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct State {
    pub schema: u32,
    pub source_full_name: String,
    pub version: [u16; 4],
    pub directory: String,
    pub exe_sha256: String,
    pub asar_sha256: String,
}

#[derive(Debug)]
pub struct Status {
    pub package: Result<platform::Package>,
    pub current: Result<Option<State>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LaunchDecision {
    Missing,
    Current,
    Changed,
    Unknown,
}

pub fn launch_decision(
    package: Option<&platform::Package>,
    state: Option<&State>,
) -> LaunchDecision {
    match (package, state) {
        (_, None) => LaunchDecision::Missing,
        (None, Some(_)) => LaunchDecision::Unknown,
        (Some(p), Some(s)) if p.full_name == s.source_full_name && p.version == s.version => {
            LaunchDecision::Current
        }
        (Some(_), Some(_)) => LaunchDecision::Changed,
    }
}

pub fn root() -> Result<PathBuf> {
    Ok(
        PathBuf::from(std::env::var_os("LOCALAPPDATA").context("未找到 LOCALAPPDATA")?)
            .join("CodexI18n"),
    )
}

fn safe_directory(root: &Path) -> Result<()> {
    fs::create_dir_all(root).with_context(|| format!("无法创建 {}", root.display()))?;
    let meta = fs::symlink_metadata(root)?;
    ensure!(
        meta.is_dir() && meta.file_attributes() & 0x400 == 0,
        "管理目录不能是重解析点: {}",
        root.display()
    );
    Ok(())
}

// Reading the record separately lets a new copy replace one whose files have
// been removed, while still refusing malformed or unsafe records.
fn read_record(root: &Path) -> Result<Option<State>> {
    if root.exists() {
        let meta = fs::symlink_metadata(root)?;
        ensure!(
            meta.is_dir() && meta.file_attributes() & 0x400 == 0,
            "管理目录不能是重解析点"
        );
    }
    let path = root.join("current.json");
    if let Ok(meta) = fs::symlink_metadata(&path) {
        ensure!(
            meta.is_file() && meta.file_attributes() & 0x400 == 0 && meta.len() <= 1024 * 1024,
            "当前副本记录不安全或过大"
        );
    }
    let data = match fs::read(&path) {
        Ok(v) => v,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let state: State = serde_json::from_slice(&data).context("当前副本记录损坏")?;
    ensure!(state.schema == 1, "不支持的副本记录格式");
    ensure!(
        state.directory.starts_with("version-")
            && matches!(
                Path::new(&state.directory)
                    .components()
                    .collect::<Vec<_>>()
                    .as_slice(),
                [Component::Normal(_)]
            ),
        "副本路径无效"
    );
    Ok(Some(state))
}

pub fn load(root: &Path) -> Result<Option<State>> {
    let Some(state) = read_record(root)? else {
        return Ok(None);
    };
    let versions = root.join("versions");
    let version_dir = versions.join(&state.directory);
    for path in [
        &versions,
        &version_dir,
        &version_dir.join("app"),
        &version_dir.join("app").join("resources"),
    ] {
        let m = fs::symlink_metadata(path)
            .with_context(|| format!("副本目录不存在: {}", path.display()))?;
        ensure!(
            m.is_dir() && m.file_attributes() & 0x400 == 0,
            "副本目录不安全: {}",
            path.display()
        );
    }
    let app = version_dir.join("app");
    for path in [
        app.join("ChatGPT.exe"),
        app.join("resources").join("app.asar"),
    ] {
        let meta = fs::symlink_metadata(&path)
            .with_context(|| format!("副本文件缺失: {}", path.display()))?;
        ensure!(
            meta.is_file() && meta.file_attributes() & 0x400 == 0,
            "副本文件不安全: {}",
            path.display()
        );
    }
    Ok(Some(state))
}

pub fn status(root: &Path) -> Status {
    Status {
        package: platform::find_package(),
        current: load(root),
    }
}
pub fn app_path(root: &Path, state: &State) -> PathBuf {
    root.join("versions").join(&state.directory).join("app")
}

fn cancelled(cancel: &AtomicBool) -> Result<()> {
    if cancel.load(Ordering::Relaxed) {
        bail!("操作已取消");
    }
    Ok(())
}

fn collect(
    source: &Path,
    relative: &Path,
    dirs: &mut Vec<PathBuf>,
    files: &mut Vec<(PathBuf, u64)>,
    cancel: &AtomicBool,
) -> Result<()> {
    cancelled(cancel)?;
    for entry in fs::read_dir(source.join(relative))? {
        let entry = entry?;
        let meta = fs::symlink_metadata(entry.path())?;
        ensure!(
            meta.file_attributes() & 0x400 == 0,
            "源文件包含不支持的重解析点: {}",
            entry.path().display()
        );
        let next = relative.join(entry.file_name());
        if meta.is_dir() {
            dirs.push(next.clone());
            collect(source, &next, dirs, files, cancel)?;
        } else if meta.is_file() {
            files.push((next, meta.len()));
        } else {
            bail!("源目录包含不支持的文件类型: {}", entry.path().display());
        }
    }
    Ok(())
}

fn copy_files(
    source: &Path,
    dest: &Path,
    cancel: &AtomicBool,
    progress: &impl Fn(String, u8),
) -> Result<()> {
    let mut files = Vec::new();
    let mut dirs = Vec::new();
    collect(source, Path::new(""), &mut dirs, &mut files, cancel)?;
    for dir in dirs {
        fs::create_dir_all(dest.join(dir))?;
    }
    let total = files
        .iter()
        .try_fold(0u64, |sum, (_, n)| sum.checked_add(*n))
        .context("文件总大小溢出")?
        .max(1);
    let mut done = 0u64;
    let mut buffer = vec![0; 1024 * 1024];
    for (relative, length) in files {
        cancelled(cancel)?;
        let target = dest.join(&relative);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut input = File::open(source.join(&relative))?;
        let mut output =
            File::create(&target).with_context(|| format!("无法复制 {}", relative.display()))?;
        let mut copied = 0u64;
        loop {
            cancelled(cancel)?;
            let n = input.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            output.write_all(&buffer[..n])?;
            copied += n as u64;
            done += n as u64;
            progress(
                format!("正在复制 {}", relative.display()),
                ((done.saturating_mul(75) / total).min(75)) as u8,
            );
        }
        ensure!(
            copied == length,
            "源文件在复制期间发生变化: {}",
            relative.display()
        );
    }
    Ok(())
}

/// Generates a new version without modifying the Store package or the active copy.
pub fn build(root: &Path, cancel: &AtomicBool, progress: impl Fn(String, u8)) -> Result<State> {
    let _lock = platform::build_lock()?;
    safe_directory(root)?;
    // An intact record can be replaced even when its copy is missing; refuse
    // unrecognized records and leave the old record in place on failure.
    let _ = read_record(root)?;
    cancelled(cancel)?;
    let versions = root.join("versions");
    safe_directory(&versions)?;
    let package = platform::find_package()?;
    let source = package.app_dir.canonicalize()?;
    let managed = root.canonicalize()?;
    ensure!(
        !managed.starts_with(&source) && !source.starts_with(&managed),
        "管理目录与 Store 安装目录重叠"
    );
    progress("正在验证官方签名…".into(), 0);
    platform::verify_official(&source.join("ChatGPT.exe"))?;
    cancelled(cancel)?;
    let staging = tempfile::Builder::new()
        .prefix("version-")
        .tempdir_in(&versions)?;
    let app = staging.path().join("app");
    fs::create_dir(&app)?;
    copy_files(&source, &app, cancel, &progress)?;
    cancelled(cancel)?;
    progress("正在校验复制文件和资源完整性…".into(), 77);
    let src_exe = source.join("ChatGPT.exe");
    let src_asar = source.join("resources").join("app.asar");
    let exe = app.join("ChatGPT.exe");
    let asar = app.join("resources").join("app.asar");
    platform::verify_official(&exe)?;
    ensure!(
        patch::hash_file(&src_exe)? == patch::hash_file(&exe)?,
        "复制的 EXE 与源文件不一致"
    );
    cancelled(cancel)?;
    ensure!(
        patch::hash_file(&src_asar)? == patch::hash_file(&asar)?,
        "复制的 ASAR 与源文件不一致"
    );
    cancelled(cancel)?;
    let plan = patch::inspect(&asar, &exe)?;
    progress(format!("正在修复 {}", plan.resource), 85);
    patch::apply(&plan, &asar, &exe)?;
    cancelled(cancel)?;
    progress("正在验证修复结果…".into(), 90);
    let exe_sha256 = patch::hash_file(&exe)?;
    let asar_sha256 = patch::hash_file(&asar)?;
    cancelled(cancel)?;
    ensure!(
        platform::find_package()? == package,
        "生成期间 Store 应用发生变化，未切换副本"
    );
    let directory = staging
        .path()
        .file_name()
        .context("暂存目录名缺失")?
        .to_string_lossy()
        .into_owned();
    let state = State {
        schema: 1,
        source_full_name: package.full_name,
        version: package.version,
        directory,
        exe_sha256,
        asar_sha256,
    };
    // Cancellation stops before this commit section. A failed state update leaves
    // the previous record intact; the verified directory can be recovered manually.
    let mut record = tempfile::NamedTempFile::new_in(root)?;
    serde_json::to_writer_pretty(&mut record, &state)?;
    record.write_all(b"\n")?;
    record.as_file().sync_all()?;
    let _kept = staging.keep();
    record
        .persist(root.join("current.json"))
        .map_err(|e| e.error)
        .context("无法更新当前副本记录")?;
    progress("修复版已生成".into(), 100);
    Ok(state)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LaunchOutcome {
    Started,
    RunningConflict,
    Recheck,
}

/// Recheck a previously selected copy before starting it. `Recheck` tells the
/// UI to reload its status and present the version choice if necessary.
pub fn launch(
    root: &Path,
    state: &State,
    allow_stale: bool,
    allow_running: bool,
) -> Result<LaunchOutcome> {
    launch_with(
        root,
        state,
        allow_stale,
        allow_running,
        platform::find_package,
    )
}

fn launch_with(
    root: &Path,
    state: &State,
    allow_stale: bool,
    allow_running: bool,
    mut find_package: impl FnMut() -> Result<platform::Package>,
) -> Result<LaunchOutcome> {
    if load(root)?.as_ref() != Some(state) {
        return Ok(LaunchOutcome::Recheck);
    }
    if !allow_stale
        && launch_decision(find_package().ok().as_ref(), Some(state)) != LaunchDecision::Current
    {
        return Ok(LaunchOutcome::Recheck);
    }
    if !allow_running && platform::chatgpt_running()? {
        return Ok(LaunchOutcome::RunningConflict);
    }
    let app = app_path(root, state);
    let exe = app.join("ChatGPT.exe");
    let asar = app.join("resources").join("app.asar");
    ensure!(
        patch::hash_file(&exe)? == state.exe_sha256
            && patch::hash_file(&asar)? == state.asar_sha256,
        "修复版文件与记录不一致，请重新生成"
    );
    if load(root)?.as_ref() != Some(state)
        || (!allow_stale
            && launch_decision(find_package().ok().as_ref(), Some(state))
                != LaunchDecision::Current)
    {
        return Ok(LaunchOutcome::Recheck);
    }
    if !allow_running && platform::chatgpt_running()? {
        return Ok(LaunchOutcome::RunningConflict);
    }
    std::process::Command::new(&exe)
        .current_dir(&app)
        .spawn()
        .context("无法启动修复版")?;
    Ok(LaunchOutcome::Started)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn launch_rechecks_store_version_after_hashing() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let app = root.join("versions").join("version-fixture").join("app");
        fs::create_dir_all(app.join("resources")).unwrap();
        let exe = app.join("ChatGPT.exe");
        let asar = app.join("resources").join("app.asar");
        fs::write(&exe, b"not an executable").unwrap();
        fs::write(&asar, b"fixture").unwrap();
        let state = State {
            schema: 1,
            source_full_name: "package".into(),
            version: [1, 2, 3, 4],
            directory: "version-fixture".into(),
            exe_sha256: patch::hash_file(&exe).unwrap(),
            asar_sha256: patch::hash_file(&asar).unwrap(),
        };
        fs::write(
            root.join("current.json"),
            serde_json::to_vec(&state).unwrap(),
        )
        .unwrap();
        let mut calls = 0;
        let result = launch_with(root, &state, false, true, || {
            calls += 1;
            Ok(platform::Package {
                full_name: "package".into(),
                version: if calls == 1 {
                    [1, 2, 3, 4]
                } else {
                    [1, 2, 3, 5]
                },
                app_dir: PathBuf::new(),
            })
        })
        .unwrap();
        assert_eq!(result, LaunchOutcome::Recheck);
        assert_eq!(calls, 2);
    }
    #[test]
    fn launch_starts_an_isolated_fixture_when_user_allows_running() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let app = root.join("versions").join("version-fixture").join("app");
        fs::create_dir_all(app.join("resources")).unwrap();
        let source = root.join("fixture.rs");
        fs::write(
            &source,
            "fn main() { std::fs::write(\"started.txt\", b\"started\").unwrap(); }",
        )
        .unwrap();
        let exe = app.join("ChatGPT.exe");
        let status = std::process::Command::new("rustc")
            .arg("--crate-name")
            .arg("launch_fixture")
            .arg(&source)
            .arg("-o")
            .arg(&exe)
            .status()
            .unwrap();
        assert!(status.success());
        let asar = app.join("resources").join("app.asar");
        fs::write(&asar, b"isolated fixture").unwrap();
        let state = State {
            schema: 1,
            source_full_name: "fixture".into(),
            version: [1, 2, 3, 4],
            directory: "version-fixture".into(),
            exe_sha256: patch::hash_file(&exe).unwrap(),
            asar_sha256: patch::hash_file(&asar).unwrap(),
        };
        fs::write(
            root.join("current.json"),
            serde_json::to_vec(&state).unwrap(),
        )
        .unwrap();
        assert_eq!(
            launch(root, &state, true, true).unwrap(),
            LaunchOutcome::Started
        );
        let marker = app.join("started.txt");
        for _ in 0..50 {
            if marker.is_file() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        assert_eq!(fs::read(&marker).unwrap(), b"started");
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    #[test]
    #[ignore = "reads the current user's existing managed copy"]
    fn current_user_copy_matches_its_record() {
        let root = root().unwrap();
        let state = load(&root).unwrap().expect("no current managed copy");
        let app = app_path(&root, &state);
        assert_eq!(
            patch::hash_file(&app.join("ChatGPT.exe")).unwrap(),
            state.exe_sha256
        );
        assert_eq!(
            patch::hash_file(&app.join("resources").join("app.asar")).unwrap(),
            state.asar_sha256
        );
        eprintln!("recorded source: {}", state.source_full_name);
    }
    #[test]
    #[ignore = "requires an existing managed copy and another running ChatGPT process"]
    fn running_chatgpt_requests_a_decision_before_launch() {
        assert!(
            platform::chatgpt_running().unwrap(),
            "requires an existing ChatGPT process"
        );
        let root = root().unwrap();
        let state = load(&root).unwrap().expect("no current managed copy");
        assert_eq!(
            launch(&root, &state, true, false).unwrap(),
            LaunchOutcome::RunningConflict
        );
    }
    #[test]
    fn launch_decision_distinguishes_version_and_unavailable_package() {
        let package = platform::Package {
            full_name: "package".into(),
            version: [1, 2, 3, 4],
            app_dir: PathBuf::new(),
        };
        let mut state = State {
            schema: 1,
            source_full_name: "package".into(),
            version: [1, 2, 3, 4],
            directory: "version-test".into(),
            exe_sha256: "x".into(),
            asar_sha256: "y".into(),
        };
        assert_eq!(
            launch_decision(Some(&package), Some(&state)),
            LaunchDecision::Current
        );
        state.version[3] += 1;
        assert_eq!(
            launch_decision(Some(&package), Some(&state)),
            LaunchDecision::Changed
        );
        assert_eq!(launch_decision(None, Some(&state)), LaunchDecision::Unknown);
        assert_eq!(
            launch_decision(Some(&package), None),
            LaunchDecision::Missing
        );
    }
    #[test]
    fn record_is_confined_to_managed_directory() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let app = root.join("versions").join("version-test").join("app");
        fs::create_dir_all(app.join("resources")).unwrap();
        fs::write(app.join("ChatGPT.exe"), b"exe").unwrap();
        fs::write(app.join("resources").join("app.asar"), b"asar").unwrap();
        let mut state = State {
            schema: 1,
            source_full_name: "package".into(),
            version: [1, 2, 3, 4],
            directory: "version-test".into(),
            exe_sha256: "x".into(),
            asar_sha256: "y".into(),
        };
        fs::write(
            root.join("current.json"),
            serde_json::to_vec(&state).unwrap(),
        )
        .unwrap();
        assert_eq!(load(root).unwrap(), Some(state.clone()));
        let original = fs::read(root.join("current.json")).unwrap();
        assert!(build(root, &AtomicBool::new(true), |_, _| {}).is_err());
        assert_eq!(fs::read(root.join("current.json")).unwrap(), original);
        state.directory = "version-test/../../outside".into();
        fs::write(
            root.join("current.json"),
            serde_json::to_vec(&state).unwrap(),
        )
        .unwrap();
        assert!(load(root).is_err());
        assert!(read_record(root).is_err());
    }
    #[test]
    fn missing_copy_does_not_make_its_record_unreadable_for_rebuild() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let state = State {
            schema: 1,
            source_full_name: "package".into(),
            version: [1, 2, 3, 4],
            directory: "version-missing".into(),
            exe_sha256: "x".into(),
            asar_sha256: "y".into(),
        };
        let record = serde_json::to_vec(&state).unwrap();
        fs::write(root.join("current.json"), &record).unwrap();
        assert!(load(root).is_err());
        assert_eq!(read_record(root).unwrap(), Some(state));
        assert!(build(root, &AtomicBool::new(true), |_, _| {}).is_err());
        assert_eq!(fs::read(root.join("current.json")).unwrap(), record);
    }
    #[test]
    fn cancelled_copy_preserves_existing_record() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        let target = dir.path().join("target");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&target).unwrap();
        fs::write(source.join("data"), [42; 256]).unwrap();
        fs::write(target.join("current.json"), b"old record").unwrap();
        let cancel = AtomicBool::new(true);
        assert!(copy_files(&source, &target, &cancel, &|_, _| {}).is_err());
        assert_eq!(
            fs::read(target.join("current.json")).unwrap(),
            b"old record"
        );
    }
    #[test]
    #[ignore = "uses the current user's Store package to test cancellation during a staged copy"]
    fn cancelling_an_update_preserves_the_previous_copy() {
        let parent = std::env::current_dir().unwrap().join("target");
        fs::create_dir_all(&parent).unwrap();
        let parent = parent.canonicalize().unwrap();
        let temp = tempfile::Builder::new()
            .prefix("manager-cancel-test-")
            .tempdir_in(&parent)
            .unwrap();
        assert_eq!(
            temp.path().canonicalize().unwrap().parent(),
            Some(parent.as_path())
        );
        let root = temp.path().join("managed");
        let app = root.join("versions").join("version-previous").join("app");
        fs::create_dir_all(app.join("resources")).unwrap();
        fs::write(app.join("ChatGPT.exe"), b"old exe").unwrap();
        fs::write(app.join("resources").join("app.asar"), b"old asar").unwrap();
        let state = State {
            schema: 1,
            source_full_name: "old-package".into(),
            version: [1, 2, 3, 4],
            directory: "version-previous".into(),
            exe_sha256: "old exe hash".into(),
            asar_sha256: "old asar hash".into(),
        };
        let record = serde_json::to_vec(&state).unwrap();
        fs::write(root.join("current.json"), &record).unwrap();
        let cancel = AtomicBool::new(false);
        let error = build(&root, &cancel, |message, _| {
            if message.starts_with("正在复制") {
                cancel.store(true, Ordering::Relaxed);
            }
        })
        .unwrap_err();
        assert!(error.to_string().contains("取消"));
        assert_eq!(fs::read(root.join("current.json")).unwrap(), record);
        assert_eq!(load(&root).unwrap(), Some(state));
        let versions: Vec<_> = fs::read_dir(root.join("versions"))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(versions, vec![std::ffi::OsString::from("version-previous")]);
    }
    #[test]
    #[ignore = "copies the installed Store app into a temporary workspace directory"]
    fn installed_package_builds_an_independent_copy() {
        let package = platform::find_package().unwrap();
        let original_exe = patch::hash_file(&package.app_dir.join("ChatGPT.exe")).unwrap();
        let parent = std::env::current_dir().unwrap().join("target");
        fs::create_dir_all(&parent).unwrap();
        let parent = parent.canonicalize().unwrap();
        let temp = tempfile::Builder::new()
            .prefix("manager-test-")
            .tempdir_in(&parent)
            .unwrap();
        assert_eq!(
            temp.path().canonicalize().unwrap().parent(),
            Some(parent.as_path())
        );
        let root = temp.path().join("managed");
        fs::create_dir_all(&root).unwrap();
        let old_state = State {
            schema: 1,
            source_full_name: "old-package".into(),
            version: [1, 2, 3, 4],
            directory: "version-missing".into(),
            exe_sha256: "old exe hash".into(),
            asar_sha256: "old asar hash".into(),
        };
        fs::write(
            root.join("current.json"),
            serde_json::to_vec(&old_state).unwrap(),
        )
        .unwrap();
        assert!(load(&root).is_err());
        let state = build(&root, &AtomicBool::new(false), |message, n| {
            if n == 0 || n >= 75 {
                eprintln!("{n}% {message}");
            }
        })
        .unwrap();
        assert_eq!(load(&root).unwrap(), Some(state.clone()));
        let app = app_path(&root, &state);
        assert_eq!(
            patch::hash_file(&app.join("ChatGPT.exe")).unwrap(),
            state.exe_sha256
        );
        assert_eq!(
            patch::hash_file(&app.join("resources").join("app.asar")).unwrap(),
            state.asar_sha256
        );
        assert_ne!(state.exe_sha256, original_exe);
        assert_eq!(
            patch::hash_file(&package.app_dir.join("ChatGPT.exe")).unwrap(),
            original_exe
        );
        platform::verify_official(&package.app_dir.join("ChatGPT.exe")).unwrap();
    }
}
