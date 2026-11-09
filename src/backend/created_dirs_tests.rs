use super::*;

/// `root/a/b/c`, created outermost first and recorded in that order. Returns the guard.
fn create_chain(root: &Path) -> CreatedDirs {
    let mut created = CreatedDirs::new();
    let mut dir = root.to_path_buf();
    for name in ["a", "b", "c"] {
        dir.push(name);
        fs::create_dir(&dir).unwrap();
        created.record(&dir);
    }
    created
}

#[skuld::test]
fn drop_removes_every_recorded_directory_deepest_first() {
    let tmp = tempfile::tempdir().unwrap();

    drop(create_chain(tmp.path()));

    assert!(!tmp.path().join("a").exists());
}

#[skuld::test]
fn drop_leaves_a_directory_that_existed_before() {
    let tmp = tempfile::tempdir().unwrap();
    let before = tmp.path().join("a");
    fs::create_dir(&before).unwrap();
    let mut created = CreatedDirs::new();
    let new = before.join("b");
    fs::create_dir(&new).unwrap();
    created.record(&new);

    drop(created);

    assert!(before.is_dir());
    assert!(!new.exists());
}

#[skuld::test]
fn drop_leaves_a_directory_that_is_no_longer_empty_and_its_ancestors() {
    let tmp = tempfile::tempdir().unwrap();
    let created = create_chain(tmp.path());
    let file = tmp.path().join("a/b/c/data");
    fs::write(&file, b"x").unwrap();

    drop(created);

    assert_eq!(fs::read(&file).unwrap(), b"x");
}

/// A removal that fails is reported, never a panic: the guard runs while an error unwinds.
#[skuld::test]
fn drop_does_not_panic_when_a_removal_fails() {
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("not-a-dir");
    fs::write(&file, b"x").unwrap();
    let mut created = CreatedDirs::new();
    created.record(&file);

    drop(created);

    assert!(file.is_file());
}

#[skuld::test]
fn keep_keeps_every_recorded_directory() {
    let tmp = tempfile::tempdir().unwrap();
    let mut created = create_chain(tmp.path());

    created.keep();
    drop(created);

    assert!(tmp.path().join("a/b/c").is_dir());
}

/// A panic may come after the definition was written: the directories of an installed service stay.
#[skuld::test]
fn drop_keeps_the_directories_while_a_panic_unwinds() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();

    let unwound = std::panic::catch_unwind(move || {
        let _created = create_chain(&root);
        panic!("a step after the write");
    });

    assert!(unwound.is_err());
    assert!(tmp.path().join("a/b/c").is_dir());
}
