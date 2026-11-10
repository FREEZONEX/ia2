use super::*;
use crate::ProjectStore;

mod fixture_sources {
    include!("../tests/fixtures/library_sources.rs");
}
use fixture_sources::{FIRST_NEW, FIRST_OLD, MAIN, SECOND_NEW, SECOND_OLD};
use std::cell::RefCell;
use std::process::{Child, Command};
use std::sync::mpsc;
use std::time::{Duration, Instant};

type Hook = Box<dyn Fn(Phase) -> io::Result<()>>;
thread_local! {
    static FAULT: RefCell<Option<Hook>> = RefCell::new(None);
}

pub(super) fn checkpoint(phase: Phase) -> io::Result<()> {
    FAULT.with(|slot| slot.borrow().as_ref().map_or(Ok(()), |hook| hook(phase)))
}

fn with_fault<T>(hook: impl Fn(Phase) -> io::Result<()> + 'static, body: impl FnOnce() -> T) -> T {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            FAULT.with(|slot| slot.replace(None));
        }
    }
    FAULT.with(|slot| assert!(slot.replace(Some(Box::new(hook))).is_none()));
    let _reset = Reset;
    body()
}

fn fail_at(phase: Phase, failures: &[Phase]) -> io::Result<()> {
    if failures.contains(&phase) {
        Err(io::Error::other(format!("fixture fault at {phase:?}")))
    } else {
        Ok(())
    }
}

fn blocks(first: &str, second: &str) -> Vec<(String, String)> {
    vec![
        ("first.st".into(), first.into()),
        ("second.st".into(), second.into()),
    ]
}

fn seeded(root: &Path) -> ProjectStore {
    let store = ProjectStore::create(root.into(), "fixture").unwrap();
    store.write_pou_source("main", MAIN).unwrap();
    let dir = root.join("pous/lib/controls");
    fs::create_dir_all(&dir).unwrap();
    for (name, text) in blocks(FIRST_OLD, SECOND_OLD) {
        fs::write(dir.join(name), text).unwrap();
    }
    fs::write(
        dir.join("spare.st"),
        "FUNCTION_BLOCK FB_Spare\nEND_FUNCTION_BLOCK\n",
    )
    .unwrap();
    fs::write(
        root.join(MANIFEST),
        "name = 'fixture'\nversion = '1.0'\nextra = 'keep me'\n\
         [libraries]\ncontrols = '1.0'\nother = '9.0'\n[governance]\nwrite_mode = 'open'\n",
    )
    .unwrap();
    store
}

fn update(store: &ProjectStore) -> Result<(), StoreError> {
    store.import_library("controls", "2.0", &blocks(FIRST_NEW, SECOND_NEW))
}

fn identity(root: &Path) -> Vec<String> {
    [
        "pous/lib/controls/first.st",
        "pous/lib/controls/second.st",
        MANIFEST,
    ]
    .iter()
    .map(|path| current(&root.join(path)).unwrap().unwrap().sha256)
    .collect()
}

fn assert_new(root: &Path) {
    let opened = ProjectStore::open(root.into()).unwrap();
    assert_eq!(opened.read_libraries().unwrap()["controls"], "2.0");
    for (name, text) in blocks(FIRST_NEW, SECOND_NEW) {
        assert_eq!(
            fs::read_to_string(root.join("pous/lib/controls").join(name)).unwrap(),
            text
        );
    }
}

#[test]
fn rolls_back_every_precommit_failure_to_original_hashes() {
    for phase in [
        Phase::BeforeStage,
        Phase::BeforeBackup(1),
        Phase::BeforeStageContents(1),
        Phase::Prepared,
        Phase::BeforeInstall(0),
        Phase::BeforeInstall(1),
        Phase::AfterInstall(0),
        Phase::BeforeInstall(2),
        Phase::AfterInstall(2),
        Phase::BeforeCommit,
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let store = seeded(tmp.path());
        let before = identity(tmp.path());
        let error =
            with_fault(move |point| fail_at(point, &[phase]), || update(&store)).unwrap_err();
        assert!(matches!(error, StoreError::Io(_)), "{error}");
        let after = identity(tmp.path());
        eprintln!("{phase:?}: {error}; before {before:?}, after {after:?}");
        assert_eq!(after, before);
        assert!(!tmp.path().join(ACTIVE).exists());
        ProjectStore::open(tmp.path().into()).unwrap();
    }
}

#[test]
fn failed_first_import_removes_new_blocks_and_created_directories() {
    let tmp = tempfile::tempdir().unwrap();
    let store = ProjectStore::create(tmp.path().into(), "fixture").unwrap();
    let old_manifest = fs::read(tmp.path().join(MANIFEST)).unwrap();
    with_fault(
        |point| fail_at(point, &[Phase::AfterInstall(0)]),
        || update(&store),
    )
    .unwrap_err();
    assert!(!tmp.path().join("pous/lib").exists());
    assert_eq!(fs::read(tmp.path().join(MANIFEST)).unwrap(), old_manifest);
}

#[test]
fn ordinary_rollback_failure_is_retried_and_unknown_external_edit_is_preserved() {
    let tmp = tempfile::tempdir().unwrap();
    let store = seeded(tmp.path());
    let before = identity(tmp.path());
    let original_first = tmp.path().join("pous/lib/controls/first.st");
    let error = with_fault(
        |point| fail_at(point, &[Phase::BeforeInstall(1), Phase::Rollback(0)]),
        || update(&store),
    )
    .unwrap_err();
    assert!(matches!(error, StoreError::LibraryRecoveryRequired(_)));
    let active = tmp.path().join(ACTIVE);
    assert!(active.join("old-0").is_file());

    fs::write(&original_first, "external edit").unwrap();
    for _ in 0..2 {
        let error = match ProjectStore::open(tmp.path().into()) {
            Ok(_) => panic!("mixed project opened"),
            Err(error) => error,
        };
        assert!(error
            .to_string()
            .contains("changed outside the library import"));
        assert!(store.clone().read_pous().is_err());
        assert_eq!(
            fs::read_to_string(&original_first).unwrap(),
            "external edit"
        );
        assert!(active.join("old-0").is_file());
    }

    // Simulate a human resolving the conflict after saving the edit.
    fs::write(&original_first, FIRST_NEW).unwrap();
    for _ in 0..2 {
        ProjectStore::open(tmp.path().into()).unwrap();
        assert_eq!(identity(tmp.path()), before);
    }
    assert!(!active.exists());
}

#[test]
fn committed_cleanup_and_unconfirmed_marker_converge_to_new_state() {
    for fault in [Phase::SyncCommit, Phase::BeforeCleanup] {
        let tmp = tempfile::tempdir().unwrap();
        let store = seeded(tmp.path());
        let result = with_fault(move |point| fail_at(point, &[fault]), || update(&store));
        match fault {
            Phase::SyncCommit => assert!(matches!(
                result,
                Err(StoreError::LibraryCommitUnconfirmed(_))
            )),
            _ => result.unwrap(),
        }
        assert!(tmp.path().join(ACTIVE).join(COMMITTED).is_file());
        for _ in 0..2 {
            assert_new(tmp.path());
        }
        assert!(!tmp.path().join(ACTIVE).exists());
    }
}

#[test]
fn selected_update_preserves_permissions_and_other_manifest_fields() {
    let tmp = tempfile::tempdir().unwrap();
    let store = seeded(tmp.path());
    let file = tmp.path().join("pous/lib/controls/first.st");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&file, Permissions::from_mode(0o640)).unwrap();
    }
    let mode_before = current(&file).unwrap().unwrap().mode;
    store
        .import_library("controls", "2.0", &[("first.st".into(), FIRST_NEW.into())])
        .unwrap();
    assert_eq!(current(&file).unwrap().unwrap().mode, mode_before);
    assert_eq!(fs::read_to_string(&file).unwrap(), FIRST_NEW);
    assert_eq!(
        store.read_pou_source("lib/controls/second").unwrap(),
        SECOND_OLD
    );
    assert_eq!(
        store.read_pou_source("lib/controls/spare").unwrap(),
        "FUNCTION_BLOCK FB_Spare\nEND_FUNCTION_BLOCK\n"
    );
    let manifest: toml::Table =
        toml::from_str(&fs::read_to_string(tmp.path().join(MANIFEST)).unwrap()).unwrap();
    assert_eq!(manifest["extra"].as_str(), Some("keep me"));
    assert_eq!(manifest["governance"]["write_mode"].as_str(), Some("open"));
    assert_eq!(store.read_libraries().unwrap()["other"], "9.0");
}

#[test]
fn rejects_bad_journal_and_library_symlink_without_touching_sentinel() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("project");
    let store = seeded(&root);
    let sentinel = tmp.path().join("outside.st");
    fs::write(&sentinel, "sentinel").unwrap();
    #[cfg(unix)]
    {
        let block = root.join("pous/lib/controls/second.st");
        fs::remove_file(&block).unwrap();
        std::os::unix::fs::symlink(&sentinel, &block).unwrap();
        assert!(update(&store).is_err());
        assert_eq!(fs::read_to_string(&sentinel).unwrap(), "sentinel");
        fs::remove_file(&block).unwrap();
        fs::write(block, SECOND_OLD).unwrap();
    }
    with_fault(
        |point| fail_at(point, &[Phase::BeforeInstall(1), Phase::Rollback(0)]),
        || update(&store),
    )
    .unwrap_err();
    let record = root.join(ACTIVE).join(PREPARED);
    let mut journal: Journal = toml::from_str(&fs::read_to_string(&record).unwrap()).unwrap();
    journal.blocks[0] = "../../../../outside.st".into();
    fs::write(&record, toml::to_string_pretty(&journal).unwrap()).unwrap();
    assert!(matches!(
        store.read_pous(),
        Err(StoreError::LibraryRecoveryRequired(_))
    ));
    assert!(record.is_file());
    assert_eq!(fs::read_to_string(sentinel).unwrap(), "sentinel");
}

#[test]
fn writer_serializes_same_project_and_leaves_other_project_available() {
    let tmp = tempfile::tempdir().unwrap();
    let store = seeded(&tmp.path().join("one"));
    let other = seeded(&tmp.path().join("two"));
    let (paused, arrived) = mpsc::channel();
    let (resume, hold) = mpsc::channel();
    let writer = store.clone();
    let import = std::thread::spawn(move || {
        with_fault(
            move |phase| {
                if phase == Phase::AfterInstall(0) {
                    paused.send(()).unwrap();
                    hold.recv_timeout(Duration::from_secs(10)).unwrap();
                }
                Ok(())
            },
            || update(&writer),
        )
    });
    arrived.recv_timeout(Duration::from_secs(10)).unwrap();
    assert!(matches!(store.read_pous(), Err(StoreError::LibraryBusy)));
    assert!(matches!(
        store.library_state(),
        Err(StoreError::LibraryBusy)
    ));
    update(&other).unwrap();
    assert_new(other.root());

    let (started, ready) = mpsc::channel();
    let delete_store = store.clone();
    let delete = std::thread::spawn(move || {
        started.send(()).unwrap();
        delete_store.remove_library("controls")
    });
    ready.recv_timeout(Duration::from_secs(10)).unwrap();
    resume.send(()).unwrap();
    import.join().unwrap().unwrap();
    delete.join().unwrap().unwrap();
    assert!(!store.root().join("pous/lib/controls").exists());
    assert!(!store.read_libraries().unwrap().contains_key("controls"));
    assert_new(other.root());
}

#[test]
fn writer_waits_until_a_same_project_copy_finishes() {
    let tmp = tempfile::tempdir().unwrap();
    let store = seeded(tmp.path());
    let (reading, entered) = mpsc::channel();
    let (resume, hold) = mpsc::channel();
    let root = store.root().to_path_buf();
    let reader = std::thread::spawn(move || {
        ProjectStore::with_snapshot_files(&root, || {
            let first = fs::read_to_string(root.join("pous/lib/controls/first.st"))?;
            reading.send(()).unwrap();
            hold.recv_timeout(Duration::from_secs(10)).unwrap();
            let second = fs::read_to_string(root.join("pous/lib/controls/second.st"))?;
            Ok((first, second))
        })
    });
    entered.recv_timeout(Duration::from_secs(10)).unwrap();
    let writer = store.clone();
    let (starting, started) = mpsc::channel();
    let import = std::thread::spawn(move || {
        // A real exclusive attempt proves contention before signalling.
        let probe = lock_file(writer.root()).unwrap();
        assert!(matches!(
            probe.try_lock(),
            Err(fs::TryLockError::WouldBlock)
        ));
        starting.send(()).unwrap();
        update(&writer)
    });
    started.recv_timeout(Duration::from_secs(10)).unwrap();
    resume.send(()).unwrap();
    let (first, second) = reader.join().unwrap().unwrap();
    assert_eq!(first, FIRST_OLD);
    assert_eq!(second, SECOND_OLD);
    import.join().unwrap().unwrap();
    assert_new(store.root());
}

#[test]
fn child_process() {
    let Some(action) = std::env::var_os("IA2_LIBRARY_TEST_ACTION") else {
        return;
    };
    let root = PathBuf::from(std::env::var_os("IA2_LIBRARY_TEST_ROOT").unwrap());
    let store = ProjectStore::open(root.clone()).unwrap();
    if action == "reopen" {
        eprintln!("reopen hashes {:?}", identity(&root));
        return;
    }
    let ready = PathBuf::from(std::env::var_os("IA2_LIBRARY_TEST_READY").unwrap());
    let checkpoint = match action.to_str().unwrap() {
        "prepared" => Phase::Prepared,
        "first" => Phase::AfterInstall(0),
        "manifest" => Phase::AfterInstall(2),
        "committed" => Phase::BeforeCleanup,
        _ => panic!("unknown child action"),
    };
    with_fault(
        move |phase| {
            if phase == checkpoint {
                fs::write(&ready, format!("{phase:?}\n")).unwrap();
                loop {
                    std::thread::park();
                }
            }
            Ok(())
        },
        || update(&store),
    )
    .unwrap();
    panic!("crash fixture never reached {checkpoint:?}");
}

struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn subprocess(root: &Path, action: &str) -> Command {
    let mut cmd = Command::new(std::env::current_exe().unwrap());
    cmd.args([
        "--exact",
        "library_transaction::tests::child_process",
        "--nocapture",
    ])
    .env("IA2_LIBRARY_TEST_ACTION", action)
    .env("IA2_LIBRARY_TEST_ROOT", root);
    cmd
}

#[test]
fn killed_import_is_recovered_twice_by_fresh_processes() {
    for action in ["prepared", "first", "manifest", "committed"] {
        let tmp = tempfile::tempdir().unwrap();
        let root = std::env::var_os("IA2_LIBRARY_CRASH_CASES")
            .map(|directory| PathBuf::from(directory).join(action))
            .unwrap_or_else(|| tmp.path().join("project"));
        assert!(!root.exists(), "fixture output already exists: {root:?}");
        seeded(&root);
        let before = identity(&root);
        let ready = tmp.path().join("ready");
        let mut child = ChildGuard(
            subprocess(&root, action)
                .env("IA2_LIBRARY_TEST_READY", &ready)
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        while !ready.is_file() {
            assert!(
                child.0.try_wait().unwrap().is_none(),
                "{action} exited before handshake"
            );
            assert!(Instant::now() < deadline, "{action} timed out");
            std::thread::sleep(Duration::from_millis(10));
        }
        child.0.kill().unwrap();
        assert!(!child.0.wait().unwrap().success());
        eprintln!(
            "killed at {action}; before {before:?}; interrupted {:?}",
            identity(&root)
        );
        for _ in 0..2 {
            assert!(subprocess(&root, "reopen").status().unwrap().success());
            if action == "committed" {
                assert_new(&root);
            } else {
                assert_eq!(identity(&root), before);
            }
            assert!(!root.join(ACTIVE).exists());
        }
    }
}
