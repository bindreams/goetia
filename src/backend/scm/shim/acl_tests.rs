//! `verdict` over stock and doctored tables: plain data, so these run on every host. The
//! Windows-only tests read a temp directory this test created and nothing else.

use super::*;

// Access masks, spelled out as `icacls` prints them: (F), (M), (RX).
const FULL: u32 = 0x1F_01FF;
const MODIFY: u32 = 0x13_01BF;
const READ_EXECUTE: u32 = 0x12_00A0;
const GENERIC_READ_EXECUTE: u32 = 0xA000_0000;
const GENERIC_ALL: u32 = 0x1000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;
const FILE_ADD_FILE: u32 = 0x2;
const FILE_APPEND_DATA: u32 = 0x4;
const FILE_DELETE_CHILD: u32 = 0x40;
const DELETE: u32 = 0x1_0000;
const WRITE_DAC: u32 = 0x4_0000;
const WRITE_OWNER: u32 = 0x8_0000;

// ACE flags.
const OBJECT_INHERIT: u8 = 0x01;
const CONTAINER_INHERIT: u8 = 0x02;
const INHERIT_ONLY: u8 = 0x08;
const OI_CI: u8 = OBJECT_INHERIT | CONTAINER_INHERIT;
const OI_CI_IO: u8 = OI_CI | INHERIT_ONLY;

fn system() -> Sid {
    sid_from_parts(5, &[18])
}
fn administrators() -> Sid {
    sid_from_parts(5, &[32, 544])
}
fn users() -> Sid {
    sid_from_parts(5, &[32, 545])
}
fn everyone() -> Sid {
    sid_from_parts(1, &[0])
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
fn trusted_installer() -> Sid {
    sid_from_parts(5, &[80, 956008885, 3418522649, 1831038044, 1853292631, 2271478464])
}

fn allow(trustee: Sid, mask: u32, flags: u8) -> Ace {
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
        dacl: Some(dacl),
    }
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

#[skuld::test]
fn a_sid_is_written_in_the_standard_string_form() {
    assert_eq!(sid_string(&administrators()), "S-1-5-32-544");
    assert_eq!(sid_string(&everyone()), "S-1-1-0");
    assert_eq!(
        sid_string(&trusted_installer()),
        "S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464"
    );
}

#[skuld::test]
fn bytes_that_are_not_a_sid_are_written_as_such_not_panicked_on() {
    assert_eq!(sid_string(&[]), "an invalid SID");
    assert_eq!(sid_string(&[1, 3, 0, 0, 0, 0, 0, 5, 1, 0]), "an invalid SID");
}

#[cfg(windows)]
mod windows {
    use super::system::{display_path, read_chain};
    use super::*;
    use crate::error::Error;

    fn disabled() -> FolderCheck {
        FolderCheck {
            mode: CheckPermissions::Disable,
        }
    }
    fn enabled() -> FolderCheck {
        FolderCheck {
            mode: CheckPermissions::Enable,
        }
    }

    #[skuld::test]
    fn a_disabled_check_reads_nothing() {
        let dir = tempfile::tempdir().unwrap();
        assert!(disabled().check(&dir.path().join("missing").join("deeper")).is_ok());
        assert!(disabled().check(Path::new(r"Z:\no\such\volume")).is_ok());
    }

    #[skuld::test]
    fn the_chain_of_a_directory_runs_from_it_to_the_volume_root() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().canonicalize().unwrap();
        let chain = read_chain(&target).unwrap();
        let paths: Vec<&Path> = chain.iter().map(|o| o.path.as_path()).collect();
        let want: Vec<PathBuf> = target.ancestors().map(display_path).collect();
        assert_eq!(paths, want.iter().map(PathBuf::as_path).collect::<Vec<_>>());
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
    fn junction(link: &Path, target: &Path) {
        let made = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .output()
            .unwrap();
        assert!(made.status.success(), "{}", String::from_utf8_lossy(&made.stdout));
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
        // Unlinked first: a junction loop must not be left for the directory's cleanup to walk.
        std::fs::remove_dir(&a).unwrap();
        std::fs::remove_dir(&b).unwrap();
        assert!(outcome.is_err(), "{:?}", outcome.map(|c| labels(&c)));
    }

    #[skuld::test]
    fn a_final_path_is_opened_as_it_is_not_as_win32_would_rewrite_it() {
        // `x.` exists only through a verbatim path; opened as `x` it is another object or none.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let odd = root.join("x.");
        std::fs::create_dir(&odd).unwrap();
        // `mklink /J` stores a normalised target (`x`), so this needs a symlink, which takes
        // Developer Mode or elevation.
        let link = root.join("link");
        std::os::windows::fs::symlink_dir(&odd, &link)
            .expect("creating a symlink needs Windows Developer Mode or an elevated token");

        let outcome = read_chain(&link);
        std::fs::remove_dir(&link).unwrap();
        std::fs::remove_dir(&odd).unwrap();
        let paths = labels(&outcome.unwrap());
        assert!(paths.contains(&display_path(&odd)), "{paths:?}");
    }

    #[skuld::test]
    fn a_path_is_shown_without_its_verbatim_prefix_only_when_that_leaves_a_path() {
        let guid = Path::new(r"\\?\Volume{01234567-89ab-cdef-0123-456789abcdef}\");
        assert_eq!(display_path(guid), guid);
        assert_eq!(display_path(Path::new(r"\\?\C:\a")), Path::new(r"C:\a"));
        assert_eq!(display_path(Path::new(r"\\?\UNC\h\s\a")), Path::new(r"\\h\s\a"));
    }

    #[skuld::test]
    fn a_folder_its_creator_owns_is_refused() {
        // A temp directory belongs to the account running this test, which is not a trusted one.
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().canonicalize().unwrap();
        let Err(Error::ShimFolderUnsafe { path, .. }) = enabled().check(&target) else {
            panic!("a temp directory must be refused");
        };
        assert!(display_path(&target).starts_with(&path), "{path:?}");
    }
}
