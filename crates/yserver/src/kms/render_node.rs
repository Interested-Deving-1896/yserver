//! Locate and open a DRM render node sibling of the scanout card fd.
//!
//! Phase 4.2 design §3.2: open at backend init via sysfs walk
//! (`/sys/dev/char/<major>:<minor>/device/drm/renderD*`); fall back to
//! a userspace enumeration of `/dev/dri/renderD*` whose parent device
//! matches the card's parent device. We deliberately do **not**
//! hardcode `/dev/dri/renderD128` — on multi-GPU hosts that selects
//! the wrong device.

use std::{
    fs,
    io::{self, ErrorKind},
    os::{
        fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd},
        unix::fs::MetadataExt,
    },
    path::{Path, PathBuf},
};

/// Open the render node that pairs with `card_fd`. The returned fd is
/// O_CLOEXEC and owned by the caller.
pub fn open_for_card<F: AsFd>(card_fd: F) -> io::Result<OwnedFd> {
    let fd = card_fd.as_fd();
    let stat = fstat_rdev(fd)?;
    let major = libc_major(stat);
    let minor = libc_minor(stat);

    if let Some(path) = render_node_path_via_sysfs((major, minor))? {
        return open_cloexec(&path);
    }

    if let Some(path) = render_node_path_via_dev_walk((major, minor))? {
        return open_cloexec(&path);
    }

    Err(io::Error::other(format!(
        "no DRM render node found for card with rdev {major}:{minor} \
         (sysfs walk and /dev/dri scan both yielded nothing)"
    )))
}

/// Resolve `(major, minor)` of a card device to the sibling render
/// node path, by reading `/sys/dev/char/<major>:<minor>/device/drm/`.
pub fn render_node_path_via_sysfs(card_dev: (u32, u32)) -> io::Result<Option<PathBuf>> {
    let dir = PathBuf::from(format!(
        "/sys/dev/char/{}:{}/device/drm",
        card_dev.0, card_dev.1
    ));
    let entries = match fs::read_dir(&dir) {
        Ok(e) => e,
        Err(err) if err.kind() == ErrorKind::NotFound => {
            return Err(io::Error::new(
                ErrorKind::NotFound,
                format!("sysfs path missing: {}", dir.display()),
            ));
        }
        Err(err) => return Err(err),
    };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.starts_with("renderD") {
            let dev_path = PathBuf::from("/dev/dri").join(&*name_str);
            return Ok(Some(dev_path));
        }
    }
    Ok(None)
}

fn render_node_path_via_dev_walk(card_dev: (u32, u32)) -> io::Result<Option<PathBuf>> {
    let entries = match fs::read_dir("/dev/dri") {
        Ok(e) => e,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    let mut candidates: Vec<PathBuf> = Vec::new();
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy().into_owned();
        if name_str.starts_with("renderD") {
            candidates.push(entry.path());
        }
    }
    candidates.sort();
    let card_parent = sysfs_parent_for(card_dev).ok();
    for cand in &candidates {
        if let Ok(meta) = fs::metadata(cand) {
            let cand_dev = (libc_major(meta.rdev()), libc_minor(meta.rdev()));
            if let (Some(card_p), Ok(cand_p)) = (card_parent.as_deref(), sysfs_parent_for(cand_dev))
                && card_p == cand_p
            {
                return Ok(Some(cand.clone()));
            }
        }
    }
    Ok(candidates.into_iter().next())
}

fn sysfs_parent_for(dev: (u32, u32)) -> io::Result<PathBuf> {
    let link = PathBuf::from(format!("/sys/dev/char/{}:{}/device", dev.0, dev.1));
    fs::canonicalize(&link)
}

fn fstat_rdev(fd: BorrowedFd<'_>) -> io::Result<u64> {
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::fstat(fd.as_raw_fd(), &mut stat) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    #[allow(clippy::useless_conversion)]
    Ok(u64::from(stat.st_rdev))
}

#[allow(clippy::cast_possible_truncation)]
fn libc_major(rdev: u64) -> u32 {
    libc::major(rdev) as u32
}

#[allow(clippy::cast_possible_truncation)]
fn libc_minor(rdev: u64) -> u32 {
    libc::minor(rdev) as u32
}

fn open_cloexec(path: &Path) -> io::Result<OwnedFd> {
    use std::os::fd::FromRawFd;
    let cstr = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())
        .map_err(|_| io::Error::other(format!("path contains nul byte: {}", path.display())))?;
    let raw = unsafe { libc::open(cstr.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_node_path_via_sysfs_returns_not_found_for_absurd_dev() {
        let res = render_node_path_via_sysfs((9999, 9999));
        assert!(matches!(res, Err(e) if e.kind() == ErrorKind::NotFound));
    }

    #[test]
    fn render_node_path_via_sysfs_smoke() {
        let Ok(entries) = fs::read_dir("/dev/dri") else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with("card")
                && let Ok(meta) = entry.metadata()
            {
                let rdev = meta.rdev();
                let dev = (libc_major(rdev), libc_minor(rdev));
                if let Ok(Some(path)) = render_node_path_via_sysfs(dev) {
                    let s = path.to_string_lossy();
                    assert!(
                        s.starts_with("/dev/dri/renderD"),
                        "expected renderD* path, got {s:?}"
                    );
                    return;
                }
            }
        }
    }

    #[test]
    fn open_cloexec_fails_for_missing_path() {
        let path = std::env::temp_dir().join("yserver-render-node-test-nonexistent");
        let _ = fs::remove_file(&path);
        let res = open_cloexec(&path);
        assert!(res.is_err());
    }

    #[test]
    fn libc_major_minor_round_trip() {
        let dev = libc::makedev(226, 128);
        assert_eq!(libc_major(dev), 226);
        assert_eq!(libc_minor(dev), 128);
    }
}
