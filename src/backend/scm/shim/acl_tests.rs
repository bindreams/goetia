//! `verdict` over stock and doctored tables: plain data, so these run on every host. The
//! Windows-only tests read a temp directory this test created and nothing else.

use super::*;

// Access masks, spelled out as `icacls` prints them: (F), (M), (RX).
const FULL: u32 = FILE_ALL_ACCESS;
const MODIFY: u32 = 0x13_01BF;
const READ_EXECUTE: u32 = 0x12_00A0;
const GENERIC_READ_EXECUTE: u32 = GENERIC_READ | GENERIC_EXECUTE;
const FILE_ADD_FILE: u32 = 0x2;
const FILE_WRITE_EA: u32 = 0x10;
const FILE_WRITE_ATTRIBUTES: u32 = 0x100;

// ACE flags.
const OBJECT_INHERIT: u8 = 0x01;
const CONTAINER_INHERIT: u8 = 0x02;
const INHERIT_ONLY: u8 = INHERIT_ONLY_ACE;
const OI_CI: u8 = OBJECT_INHERIT | CONTAINER_INHERIT;
const OI_CI_IO: u8 = OI_CI | INHERIT_ONLY;

pub(in crate::backend::scm::shim) fn users() -> Sid {
    sid_from_parts(5, &[32, 545])
}
fn creator_owner() -> Sid {
    sid_from_parts(3, &[0])
}
fn authenticated_users() -> Sid {
    sid_from_parts(5, &[11])
}
fn application_packages() -> Sid {
    sid_from_parts(15, &[2, 1])
}

pub(in crate::backend::scm::shim) fn allow(trustee: Sid, mask: u32, flags: u8) -> Ace {
    Ace {
        trustee,
        mask,
        flags,
        allow: true,
    }
}
fn deny(trustee: Sid, mask: u32, flags: u8) -> Ace {
    Ace {
        trustee,
        mask,
        flags,
        allow: false,
    }
}
fn object(path: &str, owner: Sid, dacl: Vec<Ace>) -> ObjectSecurity {
    ObjectSecurity {
        path: PathBuf::from(path),
        owner,
        protected: false,
        directory: true,
        dacl: Some(dacl),
    }
}

/// `object`, as a file.
fn file(path: &str, owner: Sid, dacl: Vec<Ace>) -> ObjectSecurity {
    ObjectSecurity {
        directory: false,
        ..object(path, owner, dacl)
    }
}

/// The stock shim file, as a provider creates it.
fn stock_file() -> ObjectSecurity {
    file(
        r"C:\Program Files\Goetia\goetia-shim.exe",
        administrators(),
        vec![
            allow(system(), FULL, 0),
            allow(administrators(), FULL, 0),
            allow(everyone(), READ_EXECUTE, 0),
        ],
    )
}

/// A host file in the stock folder: the file first, then the folder and its ancestors.
fn stock_host_chain() -> Vec<ObjectSecurity> {
    let mut chain = vec![stock_file()];
    chain.extend(stock_chain());
    chain
}

/// `C:\` as a stock Windows install has it.
fn stock_root() -> ObjectSecurity {
    object(
        r"C:\",
        system(),
        vec![
            allow(administrators(), FULL, OI_CI),
            allow(system(), FULL, OI_CI),
            allow(users(), READ_EXECUTE, OI_CI),
            allow(authenticated_users(), MODIFY, OI_CI_IO),
            allow(authenticated_users(), FILE_APPEND_DATA, 0),
        ],
    )
}

/// `C:\Program Files` as a stock Windows install has it.
fn stock_program_files() -> ObjectSecurity {
    object(
        r"C:\Program Files",
        trusted_installer(),
        vec![
            allow(trusted_installer(), FULL, 0),
            allow(trusted_installer(), FULL, CONTAINER_INHERIT | INHERIT_ONLY),
            allow(system(), MODIFY, 0),
            allow(system(), FULL, OI_CI_IO),
            allow(administrators(), MODIFY, 0),
            allow(administrators(), FULL, OI_CI_IO),
            allow(users(), READ_EXECUTE, 0),
            allow(users(), GENERIC_READ_EXECUTE, OI_CI_IO),
            allow(creator_owner(), FULL, OI_CI_IO),
            allow(application_packages(), READ_EXECUTE, 0),
            allow(application_packages(), GENERIC_READ_EXECUTE, OI_CI_IO),
        ],
    )
}

/// The folder a provider creates: SYSTEM and Administrators full control, Everyone read and execute.
fn protected_target() -> ObjectSecurity {
    object(
        r"C:\Program Files\Goetia",
        administrators(),
        vec![
            allow(system(), FULL, OI_CI),
            allow(administrators(), FULL, OI_CI),
            allow(everyone(), READ_EXECUTE, OI_CI),
        ],
    )
}

fn stock_chain() -> Vec<ObjectSecurity> {
    vec![protected_target(), stock_program_files(), stock_root()]
}

fn refused(chain: &[ObjectSecurity]) -> Unsafe {
    verdict(chain).expect_err("the chain must be refused")
}

#[skuld::test]
fn the_stock_program_files_chain_is_accepted() {
    assert!(verdict(&stock_chain()).is_ok());
}

#[skuld::test]
fn a_target_that_is_an_existing_stock_folder_is_judged_as_one() {
    assert!(verdict(&[stock_program_files(), stock_root()]).is_ok());
}

#[skuld::test]
fn an_account_that_may_only_add_files_is_accepted() {
    let mut chain = stock_chain();
    chain[0]
        .dacl
        .as_mut()
        .unwrap()
        .push(allow(users(), FILE_ADD_FILE | FILE_APPEND_DATA, OI_CI));
    assert!(verdict(&chain).is_ok());
}

#[skuld::test]
fn an_untrusted_account_with_only_generic_write_is_accepted() {
    // Generic write maps to rights that change neither permissions, owner nor the entries.
    let mut chain = stock_chain();
    chain[0]
        .dacl
        .as_mut()
        .unwrap()
        .push(allow(users(), GENERIC_WRITE, OI_CI));
    assert!(verdict(&chain).is_ok());
}

#[skuld::test]
fn the_stock_host_file_and_its_folders_are_accepted() {
    assert!(verdict(&stock_host_chain()).is_ok());
}

#[skuld::test]
fn each_right_that_changes_a_file_is_refused_with_its_own_wording() {
    let cases = [
        (FILE_WRITE_DATA, "write to"),
        (FILE_APPEND_DATA, "write to"),
        (GENERIC_WRITE, "write to"),
        (MODIFY, "write to"),
        (WRITE_DAC, "change permissions of"),
        (WRITE_OWNER, "take ownership of"),
        (DELETE, "delete or rename"),
    ];
    for (mask, right) in cases {
        let mut chain = stock_host_chain();
        chain[0].dacl.as_mut().unwrap().push(allow(users(), mask, 0));
        let refusal = refused(&chain);
        assert_eq!(refusal.right, right, "mask {mask:#x}");
        assert_eq!(refusal.path, PathBuf::from(r"C:\Program Files\Goetia\goetia-shim.exe"));
        assert_eq!(refusal.account, users(), "mask {mask:#x}");
    }
}

/// Extended attributes and timestamps are not what a process is made of, and neither changes the
/// file's contents or who may change them.
#[skuld::test]
fn a_file_whose_extended_attributes_and_attributes_alone_can_be_written_is_accepted() {
    let mut chain = stock_host_chain();
    chain[0]
        .dacl
        .as_mut()
        .unwrap()
        .push(allow(users(), FILE_WRITE_EA | FILE_WRITE_ATTRIBUTES, 0));
    assert!(verdict(&chain).is_ok());
}

#[skuld::test]
fn delete_child_means_nothing_on_a_file() {
    let mut chain = stock_host_chain();
    chain[0]
        .dacl
        .as_mut()
        .unwrap()
        .push(allow(users(), FILE_DELETE_CHILD, 0));
    assert!(verdict(&chain).is_ok());
}

#[skuld::test]
fn an_inherit_only_ace_on_a_file_grants_nothing() {
    let mut chain = stock_host_chain();
    chain[0].dacl.as_mut().unwrap().push(allow(users(), FULL, OI_CI_IO));
    assert!(verdict(&chain).is_ok());
}

#[skuld::test]
fn a_file_an_untrusted_account_owns_is_refused_as_owned() {
    let mut chain = stock_host_chain();
    chain[0].owner = users();
    let refusal = refused(&chain);
    assert_eq!(refusal.right, "own");
    assert_eq!(refusal.path, PathBuf::from(r"C:\Program Files\Goetia\goetia-shim.exe"));
}

#[skuld::test]
fn a_file_with_a_null_dacl_is_refused() {
    let mut chain = stock_host_chain();
    chain[0].dacl = None;
    assert_eq!(refused(&chain).right, "do anything to");
}

/// A file inherits what a doctored folder grants, so both are unsafe for one cause: the folder is
/// the one to fix, and is what is named.
#[skuld::test]
fn an_unsafe_folder_is_reported_before_the_host_file_in_it() {
    let mut chain = stock_host_chain();
    chain[0].dacl.as_mut().unwrap().push(allow(users(), FILE_WRITE_DATA, 0));
    chain[1].dacl.as_mut().unwrap().push(allow(users(), WRITE_DAC, 0));
    let refusal = refused(&chain);
    assert_eq!(refusal.path, PathBuf::from(r"C:\Program Files\Goetia"));
    assert_eq!(refusal.right, "change permissions of");
}

#[skuld::test]
fn a_host_file_is_reported_when_every_folder_is_safe() {
    let mut chain = stock_host_chain();
    chain[0].dacl.as_mut().unwrap().push(allow(users(), FILE_WRITE_DATA, 0));
    assert_eq!(
        refused(&chain).path,
        PathBuf::from(r"C:\Program Files\Goetia\goetia-shim.exe")
    );
}

/// A link to a file is judged as a file, and so is the file it leads to; the folders on the way are
/// judged as folders.
#[skuld::test]
fn a_file_the_chain_reaches_through_a_link_is_judged_as_a_file() {
    let link = file(r"C:\Goetia\host.exe", administrators(), vec![allow(system(), FULL, 0)]);
    let mut target = stock_file();
    target.dacl.as_mut().unwrap().push(allow(users(), FILE_WRITE_DATA, 0));
    let mut chain = vec![link, target];
    chain.extend(stock_chain());
    let refusal = refused(&chain);
    assert_eq!(refusal.right, "write to");
    assert_eq!(refusal.path, PathBuf::from(r"C:\Program Files\Goetia\goetia-shim.exe"));
}

#[skuld::test]
fn an_inherit_only_ace_grants_nothing_to_the_object_it_is_on() {
    let mut chain = stock_chain();
    chain[0].dacl.as_mut().unwrap().push(allow(users(), FULL, OI_CI_IO));
    assert!(verdict(&chain).is_ok());
}

#[skuld::test]
fn trusted_accounts_may_do_anything() {
    let chain = vec![object(
        r"C:\X",
        system(),
        vec![
            allow(system(), FULL, 0),
            allow(administrators(), FULL, 0),
            allow(trusted_installer(), FULL, 0),
        ],
    )];
    assert!(verdict(&chain).is_ok());
}

#[skuld::test]
fn each_dangerous_right_is_refused_with_its_own_wording() {
    let cases = [
        (WRITE_DAC, "change permissions of"),
        (WRITE_OWNER, "take ownership of"),
        (DELETE, "delete or rename"),
        (FILE_DELETE_CHILD, "delete or rename entries in"),
    ];
    for (mask, right) in cases {
        let mut chain = stock_chain();
        chain[0].dacl.as_mut().unwrap().push(allow(users(), mask, 0));
        let refusal = refused(&chain);
        assert_eq!(refusal.right, right, "mask {mask:#x}");
        assert_eq!(refusal.path, PathBuf::from(r"C:\Program Files\Goetia"));
        assert_eq!(refusal.account, users(), "mask {mask:#x}");
    }
}

#[skuld::test]
fn an_untrusted_owner_is_refused_as_owning() {
    let mut chain = stock_chain();
    chain[0].owner = users();
    let refusal = refused(&chain);
    assert_eq!(refusal.right, "own");
    assert_eq!(refusal.account, users());
    assert_eq!(refusal.path, PathBuf::from(r"C:\Program Files\Goetia"));
}

#[skuld::test]
fn generic_all_is_mapped_before_it_is_judged() {
    let mut chain = stock_chain();
    chain[0]
        .dacl
        .as_mut()
        .unwrap()
        .push(allow(everyone(), GENERIC_ALL, OI_CI));
    let refusal = refused(&chain);
    assert_eq!(refusal.right, "change permissions of");
    assert_eq!(refusal.account, everyone());
}

#[skuld::test]
fn delete_child_on_the_parent_names_the_parent() {
    let mut chain = stock_chain();
    chain[1]
        .dacl
        .as_mut()
        .unwrap()
        .push(allow(users(), FILE_DELETE_CHILD, 0));
    let refusal = refused(&chain);
    assert_eq!(refusal.right, "delete or rename entries in");
    assert_eq!(refusal.path, PathBuf::from(r"C:\Program Files"));
}

#[skuld::test]
fn delete_child_on_the_target_names_the_target() {
    let mut chain = stock_chain();
    chain[0]
        .dacl
        .as_mut()
        .unwrap()
        .push(allow(users(), FILE_DELETE_CHILD, 0));
    let refusal = refused(&chain);
    assert_eq!(refusal.right, "delete or rename entries in");
    assert_eq!(refusal.path, PathBuf::from(r"C:\Program Files\Goetia"));
}

#[skuld::test]
fn a_null_dacl_on_an_ancestor_is_refused_naming_the_ancestor() {
    let mut chain = stock_chain();
    chain[1].dacl = None;
    let refusal = refused(&chain);
    assert_eq!(refusal.right, "do anything to");
    assert_eq!(refusal.path, PathBuf::from(r"C:\Program Files"));
    assert_eq!(refusal.account, everyone());
}

#[skuld::test]
fn a_deny_ace_does_not_cancel_an_allow_ace() {
    let mut chain = stock_chain();
    let dacl = chain[0].dacl.as_mut().unwrap();
    dacl.insert(0, deny(users(), WRITE_DAC, 0));
    dacl.push(allow(users(), WRITE_DAC, 0));
    assert_eq!(refused(&chain).right, "change permissions of");
}

#[skuld::test]
fn a_deny_ace_alone_refuses_nothing() {
    let mut chain = stock_chain();
    chain[0].dacl.as_mut().unwrap().insert(0, deny(users(), FULL, 0));
    assert!(verdict(&chain).is_ok());
}

#[skuld::test]
fn the_nearest_object_to_the_target_is_reported_first() {
    let mut chain = stock_chain();
    chain[2].owner = users();
    chain[1].owner = users();
    assert_eq!(refused(&chain).path, PathBuf::from(r"C:\Program Files"));
}

#[cfg(windows)]
pub(in crate::backend::scm::shim) mod windows {
    use super::system::{ObjectId, display_path, identity_of, is_unreadable, parse, read_chain, resolve_target};
    use super::*;
    use crate::error::Error;
    use std::io;

    fn disabled() -> FolderCheck {
        FolderCheck::new(CheckPermissions::Disable)
    }
    fn enabled() -> FolderCheck {
        FolderCheck::new(CheckPermissions::Enable)
    }

    /// A new folder in `parent`, which everyone has full control of (inherited by what is made in
    /// it). Returns its final path.
    fn world_writable_folder(parent: &Path) -> PathBuf {
        let folder = parent.join("open");
        Descriptor::from_sddl("D:P(A;OICI;FA;;;WD)")
            .unwrap()
            .create_dir(&folder)
            .unwrap();
        folder.canonicalize().unwrap()
    }

    /// A volume that keeps no ACLs is refused: no owner or permissions can be read back from it.
    /// The named-pipe file system is one that does not keep them.
    #[skuld::test]
    fn a_volume_that_keeps_no_acls_is_refused() {
        let refusal = require_acls_under(Path::new(r"\\.\pipe\")).expect_err("the pipe file system keeps no ACLs");

        assert!(
            matches!(
                refusal,
                Error::ShimFolderUnsafe {
                    right: "bypass access control on",
                    ..
                }
            ),
            "{refusal:?}"
        );
    }

    #[skuld::test]
    fn a_disabled_check_reads_nothing() {
        let dir = tempfile::tempdir().unwrap();
        assert!(disabled().check(&dir.path().join("missing").join("deeper")).is_ok());
        assert!(disabled().check(Path::new(r"Z:\no\such\volume")).is_ok());
    }

    #[skuld::test]
    fn a_disabled_file_check_reads_nothing() {
        assert!(disabled().check(Path::new(r"Z:\no\such\volume\host.exe")).is_ok());
    }

    #[skuld::test]
    fn the_chain_of_a_file_starts_with_the_file_and_only_it_is_not_a_directory() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().canonicalize().unwrap().join("host.exe");
        std::fs::write(&exe, b"x").unwrap();
        let chain = read_chain(&exe).unwrap();
        assert_eq!(chain[0].path, display_path(&exe));
        assert!(!chain[0].directory);
        assert!(chain[1..].iter().all(|object| object.directory), "{chain:?}");
    }

    /// The folder is judged before the file in it, so a file's check refuses for the folder.
    #[skuld::test]
    fn a_host_file_in_a_folder_everyone_can_write_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let folder = world_writable_folder(dir.path());
        let exe = folder.join("host.exe");
        std::fs::write(&exe, b"x").unwrap();

        let Err(Error::ShimFolderUnsafe { path, .. }) = enabled().check(&exe) else {
            panic!("a file in a folder everyone can write must be refused");
        };
        assert_eq!(path, display_path(&folder));
    }

    #[skuld::test]
    fn the_chain_of_a_directory_runs_from_it_to_the_volume_root() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().canonicalize().unwrap();
        let chain = read_chain(&target).unwrap();
        let paths: Vec<&Path> = chain.iter().map(|o| o.path.as_path()).collect();
        let want: Vec<PathBuf> = target.ancestors().map(display_path).collect();
        assert_eq!(paths, want.iter().map(PathBuf::as_path).collect::<Vec<_>>());
        assert!(chain.iter().all(|object| object.directory));
        for object in &chain {
            assert!(!object.owner.is_empty(), "{}", object.path.display());
            assert_ne!(sid_string(&object.owner), "an invalid SID", "{}", object.path.display());
        }
    }

    #[skuld::test]
    fn a_missing_target_is_represented_by_its_first_existing_ancestor() {
        let dir = tempfile::tempdir().unwrap();
        let existing = dir.path().canonicalize().unwrap();
        let chain = read_chain(&existing.join("a").join("b")).unwrap();
        assert_eq!(chain[0].path, display_path(&existing));
    }

    /// A junction (no privilege needed, unlike a symlink) at `link` to the existing `target`.
    pub(in crate::backend::scm::shim) fn junction(link: &Path, target: &Path) {
        let made = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .output()
            .unwrap();
        assert!(made.status.success(), "{}", String::from_utf8_lossy(&made.stdout));
    }

    /// A directory symlink at `link`, which takes Developer Mode or an elevated token.
    fn symlink(target: &Path, link: &Path) {
        std::os::windows::fs::symlink_dir(target, link)
            .expect("creating a symlink needs Windows Developer Mode or an elevated token");
    }

    fn labels(chain: &[ObjectSecurity]) -> Vec<PathBuf> {
        chain.iter().map(|o| o.path.clone()).collect()
    }

    #[skuld::test]
    fn the_chain_holds_the_links_and_the_final_path() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let real = root.join("real");
        std::fs::create_dir(&real).unwrap();
        let link = root.join("link");
        junction(&link, &real);

        let paths = labels(&read_chain(&link).unwrap());
        assert!(paths.contains(&display_path(&link)), "{paths:?}");
        assert!(paths.contains(&display_path(&real)), "{paths:?}");
        let mut unique = paths.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), paths.len(), "no object twice: {paths:?}");
    }

    #[skuld::test]
    fn a_link_reached_through_another_is_judged_with_its_parent() {
        // `outer` -> `inner\middle` -> `real`: whoever can retarget `middle` redirects the folder,
        // though neither the given path nor the final path names it.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let real = root.join("real");
        let inner = root.join("inner");
        std::fs::create_dir(&real).unwrap();
        std::fs::create_dir(&inner).unwrap();
        let middle = inner.join("middle");
        junction(&middle, &real);
        let outer = root.join("outer");
        junction(&outer, &middle);

        let paths = labels(&read_chain(&outer).unwrap());
        for held in [&outer, &middle, &inner, &real, &root] {
            assert!(
                paths.contains(&display_path(held)),
                "{} missing from {paths:?}",
                held.display()
            );
        }
    }

    #[skuld::test]
    fn a_loop_of_links_ends() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let (a, b) = (root.join("a"), root.join("b"));
        // `b` -> `a`, then `a` is replaced by a junction to `b`: a -> b -> a.
        std::fs::create_dir(&a).unwrap();
        junction(&b, &a);
        std::fs::remove_dir(&a).unwrap();
        junction(&a, &b);

        // A loop has no final path, so the read fails; what matters is that the walk ended.
        let outcome = read_chain(&a);
        assert!(outcome.is_err(), "{:?}", outcome.map(|c| labels(&c)));
    }

    #[skuld::test]
    fn a_final_path_is_opened_as_it_is_not_as_win32_would_rewrite_it() {
        // `x.` exists only through a verbatim path; opened as `x` it is another object or none.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let odd = root.join("x.");
        std::fs::create_dir(&odd).unwrap();
        // `mklink /J` stores a normalised target (`x`), so this needs a symlink.
        let link = root.join("link");
        symlink(&odd, &link);

        let paths = labels(&read_chain(&link).unwrap());
        assert!(paths.contains(&display_path(&odd)), "{paths:?}");
    }

    #[skuld::test]
    fn a_relative_link_target_is_resolved_where_the_system_resolves_it() {
        // `outer\sublink\link` -> `..\foo`. `sublink` leads to `elsewhere`, so the system resolves
        // the `..` there, to `root\foo`; spelled out it would be `outer\foo`, which is not there.
        // `foo` is itself a link, to `final`.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let (outer, elsewhere, last) = (root.join("outer"), root.join("elsewhere"), root.join("final"));
        for made in [&outer, &elsewhere, &last] {
            std::fs::create_dir(made).unwrap();
        }
        let foo = root.join("foo");
        let sublink = outer.join("sublink");
        let link = elsewhere.join("link");
        symlink(&last, &foo);
        symlink(&elsewhere, &sublink);
        symlink(Path::new(r"..\foo"), &link);

        // An object is labelled by the first spelling it was reached by.
        let given = sublink.join("link");
        let paths = labels(&read_chain(&given).unwrap());
        for held in [&given, &sublink, &foo, &last] {
            assert!(
                paths.contains(&display_path(held)),
                "{} missing from {paths:?}",
                held.display()
            );
        }
    }

    #[skuld::test]
    fn a_drive_relative_link_target_is_refused() {
        // `CreateSymbolicLink` resolves `C:foo` itself and stores an absolute target, so no link
        // made through the API holds one; the target is read as the system would hand it over.
        let link = PathBuf::from(r"C:\d\link");
        let outcome = resolve_target(&link, PathBuf::from("C:foo"), || panic!("not needed"));
        let Err(Error::Os { what, path, .. }) = outcome else {
            panic!("a drive-relative target must be refused: {outcome:?}");
        };
        assert_eq!(what, "follow the link");
        assert_eq!(path, Some(link));
    }

    #[skuld::test]
    fn link_targets_are_resolved_lexically() {
        let link = PathBuf::from(r"C:\d\link");
        let parent = || Ok(PathBuf::from(r"C:\real\parent"));
        let resolve = |target: &str| resolve_target(&link, PathBuf::from(target), parent).unwrap();
        assert_eq!(resolve(r"..\foo"), PathBuf::from(r"C:\real\foo"));
        assert_eq!(resolve(r"sub\.\foo"), PathBuf::from(r"C:\real\parent\sub\foo"));
        assert_eq!(resolve(r"\foo"), PathBuf::from(r"C:\foo"));
        assert_eq!(resolve(r"D:\a\..\b"), PathBuf::from(r"D:\b"));
        // The system does not interpret `..` in a verbatim path.
        assert_eq!(resolve(r"\\?\D:\a\..\b"), PathBuf::from(r"\\?\D:\a\..\b"));
    }

    #[skuld::test]
    fn the_file_index_stands_in_when_the_file_id_is_unsupported() {
        let unsupported = || Err(io::Error::from_raw_os_error(87));
        let index = || Ok(ObjectId::Index { volume: 7, index: 1 });
        let id = ObjectId::Id { volume: 1, id: [2; 16] };
        assert_eq!(identity_of(Ok(id), || panic!("not needed")).unwrap(), id);
        assert_eq!(
            identity_of(unsupported(), index).unwrap(),
            ObjectId::Index { volume: 7, index: 1 }
        );
        assert!(identity_of(unsupported(), unsupported).is_err());
    }

    /// A file index and a file ID that spell the same bytes are two identities: an object that the
    /// walk knows by one form is not taken for another object known by the other.
    #[skuld::test]
    fn an_identity_by_file_index_never_equals_one_by_file_id() {
        let mut id = [0; 16];
        id[..8].copy_from_slice(&5u64.to_le_bytes());
        assert_ne!(ObjectId::Index { volume: 3, index: 5 }, ObjectId::Id { volume: 3, id });
    }

    #[skuld::test]
    fn a_path_spelled_in_another_case_names_each_object_once() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let lower = PathBuf::from(display_path(&root).to_string_lossy().to_lowercase());

        let paths = labels(&read_chain(&lower).unwrap());
        let mut folded: Vec<String> = paths.iter().map(|p| p.to_string_lossy().to_lowercase()).collect();
        folded.sort();
        let before = folded.len();
        folded.dedup();
        assert_eq!(folded.len(), before, "an object twice: {paths:?}");
    }

    #[skuld::test]
    fn a_path_is_shown_without_its_verbatim_prefix_only_when_that_leaves_a_path() {
        let guid = Path::new(r"\\?\Volume{01234567-89ab-cdef-0123-456789abcdef}\");
        assert_eq!(display_path(guid), guid);
        assert_eq!(display_path(Path::new(r"\\?\C:\a")), Path::new(r"C:\a"));
        assert_eq!(display_path(Path::new(r"\\?\UNC\h\s\a")), Path::new(r"\\h\s\a"));
    }

    #[skuld::test]
    fn a_folder_everyone_can_write_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let target = world_writable_folder(dir.path());
        // Whoever owns it, it is refused: an untrusted owner as the owner, a trusted one for the
        // entry that gives Everyone control of its permissions.
        let owner = read_chain(&target).unwrap()[0].owner.clone();
        let (account, right) = if is_trusted(&owner) {
            (everyone(), "change permissions of")
        } else {
            (owner, "own")
        };

        let Err(Error::ShimFolderUnsafe {
            path,
            account: named,
            right: said,
        }) = enabled().check(&target)
        else {
            panic!("a folder everyone can write must be refused");
        };
        assert_eq!(path, display_path(&target));
        assert_eq!(named, account_name(&account));
        assert_eq!(said, right);
    }

    /// An in-memory ACL holding one full-control entry of `ace_type` for `trustee`.
    fn acl_with_entry(ace_type: u8, trustee: &Sid) -> Vec<u32> {
        use windows_sys::Win32::Security::{ACL, ACL_REVISION, AddAce, InitializeAcl};
        let mut ace = vec![0, 0];
        ace.extend_from_slice(&((8 + trustee.len()) as u16).to_le_bytes());
        ace.extend_from_slice(&0x1F_01FFu32.to_le_bytes());
        ace.extend_from_slice(trustee);
        let mut acl = vec![0u32; 64];
        let pointer = acl.as_mut_ptr().cast::<ACL>();
        // SAFETY: `acl` is a zeroed, 4-aligned buffer of the length passed; `ace` is as long as its
        // header says.
        unsafe {
            assert_ne!(InitializeAcl(pointer, 256, ACL_REVISION), 0);
            assert_ne!(
                AddAce(pointer, ACL_REVISION, u32::MAX, ace.as_ptr().cast(), ace.len() as u32),
                0
            );
        }
        // `AddAce` refuses a type it does not know, so the type is written over an allowed entry's:
        // the first entry follows the 8-byte ACL header.
        acl[2] = (acl[2] & !0xFF) | u32::from(ace_type);
        acl
    }

    #[skuld::test]
    fn an_entry_goetia_cannot_read_is_told_from_a_read_that_failed() {
        let owner = sid_from_parts(5, &[18]);
        // A compound entry (type 4), which goetia does not read.
        let mut acl = acl_with_entry(4, &owner);
        let acl = acl.as_mut_ptr().cast();

        // SAFETY: `owner` is a valid SID and `acl` a valid ACL, both live for the call.
        let unreadable = unsafe { parse(owner.as_ptr() as _, acl, Path::new("f")) }.unwrap_err();

        assert!(is_unreadable(&unreadable), "{unreadable:?}");
        assert!(!is_unreadable(&Error::os_at(
            "read the permissions of",
            "f",
            io::Error::other("an entry of type 4, which goetia cannot read")
        )));
    }

    #[skuld::test]
    fn an_entry_goetia_can_read_is_read() {
        let owner = sid_from_parts(5, &[18]);
        let mut acl = acl_with_entry(0, &owner);
        let acl = acl.as_mut_ptr().cast();

        // SAFETY: as above.
        let (found, dacl) = unsafe { parse(owner.as_ptr() as _, acl, Path::new("f")) }.unwrap();

        assert_eq!(found, owner);
        assert_eq!(dacl.unwrap().len(), 1);
    }

    #[skuld::test]
    fn an_owner_that_is_not_a_sid_is_unreadable_not_a_failed_read() {
        let junk = [0xFFu8; 16];

        // SAFETY: `junk` is readable for the call, which only validates it; a null DACL is allowed.
        let unreadable = unsafe { parse(junk.as_ptr() as _, std::ptr::null_mut(), Path::new("f")) }.unwrap_err();

        assert!(is_unreadable(&unreadable), "{unreadable:?}");
    }
}
