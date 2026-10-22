//! Real filesystem checks for the small OS-specific source-transfer primitives.
use super::*;
use std::os::unix::fs::symlink;

#[test]
fn no_replace_rename_preserves_existing_directory_and_symlink() {
    let temp = tempfile::tempdir().unwrap();
    let root = open_root(temp.path()).unwrap();
    let fd = root.as_raw_fd();
    std::fs::create_dir(temp.path().join("source")).unwrap();
    std::fs::write(temp.path().join("source/payload"), b"new").unwrap();
    std::fs::create_dir(temp.path().join("existing")).unwrap();
    std::fs::write(temp.path().join("existing/payload"), b"original").unwrap();
    symlink("missing", temp.path().join("link")).unwrap();

    for target in ["existing", "link"] {
        let error =
            rename_noreplace(fd, &cstr("source").unwrap(), &cstr(target).unwrap()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(
            std::fs::read(temp.path().join("source/payload")).unwrap(),
            b"new"
        );
    }
    assert_eq!(
        std::fs::read(temp.path().join("existing/payload")).unwrap(),
        b"original"
    );
    assert_eq!(
        std::fs::read_link(temp.path().join("link")).unwrap(),
        Path::new("missing")
    );

    rename_noreplace(fd, &cstr("source").unwrap(), &cstr("published").unwrap()).unwrap();
    assert!(!temp.path().join("source").exists());
    assert_eq!(
        std::fs::read(temp.path().join("published/payload")).unwrap(),
        b"new"
    );
}

#[test]
fn directory_enumeration_clears_errno_and_rewinds() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("one"), b"1").unwrap();
    std::fs::create_dir(temp.path().join("two")).unwrap();
    let root = open_root(temp.path()).unwrap();
    for _ in 0..2 {
        nix::errno::Errno::ENOENT.set();
        let mut names = read_dir_names(root.as_raw_fd()).unwrap();
        names.sort();
        assert_eq!(names, vec![cstr("one").unwrap(), cstr("two").unwrap()]);
    }
}
