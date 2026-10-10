use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    os::{
        fd::AsRawFd,
        unix::fs::{OpenOptionsExt, PermissionsExt},
    },
    path::{Path, PathBuf},
};

pub fn data_dir() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("below-the-crown")
}

pub fn private_dir(path: &Path) -> Result<PathBuf> {
    if let Ok(meta) = fs::symlink_metadata(path)
        && (meta.file_type().is_symlink() || !meta.is_dir())
    {
        bail!(
            "Private directory is not a real directory: {}",
            path.display()
        );
    }
    fs::create_dir_all(path).with_context(|| format!("Cannot create {}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(path.to_path_buf())
}

pub fn write_private(path: &Path, bytes: &[u8], executable: bool) -> Result<()> {
    let parent = path.parent().context("File has no parent")?;
    private_dir(parent)?;
    if fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
        bail!("Refusing a symlink at {}", path.display());
    }
    let temporary = parent.join(format!(".write-{}", uuid::Uuid::new_v4()));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(if executable { 0o700 } else { 0o600 })
            .open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

pub fn manager_socket(directory: &Path) -> Result<PathBuf> {
    let hash = format!(
        "{:x}",
        Sha256::digest(directory.as_os_str().as_encoded_bytes())
    );
    let base = match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(path) => PathBuf::from(path).join("below-the-crown"),
        None => {
            std::env::temp_dir().join(format!("below-the-crown-{}", unsafe { libc::geteuid() }))
        }
    };
    Ok(private_dir(&base)?.join(format!("{}.sock", &hash[..24])))
}

pub struct ServerLock(File);

impl Drop for ServerLock {
    fn drop(&mut self) {
        // Closing alone leaves a flock held by a forked child until it execs.
        // Ownership belongs to this guard, so release it explicitly first.
        unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

pub fn lock_server(directory: &Path) -> Result<ServerLock> {
    private_dir(directory)?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(directory.join("server.lock"))?;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        bail!("A server already owns {}", directory.display());
    }
    Ok(ServerLock(lock))
}

pub fn find_executable(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|p| p.join(name))
        .find_map(|p| {
            let real = fs::canonicalize(p).ok()?;
            let metadata = fs::metadata(&real).ok()?;
            (metadata.is_file() && metadata.permissions().mode() & 0o111 != 0).then_some(real)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn private_files_are_atomic_and_not_world_readable() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("worker/auth.json");
        write_private(&file, b"test-owned-fixture", false).unwrap();
        assert_eq!(fs::read(&file).unwrap(), b"test-owned-fixture");
        assert_eq!(
            fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
        write_private(&file, b"new", false).unwrap();
        assert_eq!(fs::read(file).unwrap(), b"new");
    }
    #[test]
    fn refuse_symlink_private_destinations() {
        let dir = tempfile::tempdir().unwrap();
        let other = dir.path().join("real");
        fs::write(&other, b"original").unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&other, &link).unwrap();
        assert!(write_private(&link, b"replacement", false).is_err());
        assert_eq!(fs::read(other).unwrap(), b"original");
    }
    #[test]
    fn server_lock_rejects_a_second_owner() {
        let dir = tempfile::tempdir().unwrap();
        let first = lock_server(dir.path()).unwrap();
        assert!(lock_server(dir.path()).is_err());
        drop(first);
        assert!(lock_server(dir.path()).is_ok());
    }
    #[test]
    fn dropping_server_owner_releases_lock_even_when_child_inherited_descriptor() {
        use std::{io::Read, os::unix::process::CommandExt, process::Stdio};

        let dir = tempfile::tempdir().unwrap();
        let first = lock_server(dir.path()).unwrap();
        let fd = first.0.as_raw_fd();
        let mut command = std::process::Command::new("sh");
        command
            .args(["-c", "printf ready; read -r release"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped());
        unsafe {
            command.pre_exec(move || {
                if libc::fcntl(fd, libc::F_SETFD, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn().unwrap();
        let mut ready = [0; 5];
        child.stdout.take().unwrap().read_exact(&mut ready).unwrap();
        assert_eq!(&ready, b"ready");
        drop(first);
        let replacement = lock_server(dir.path());
        child.stdin.take().unwrap().write_all(b"release\n").unwrap();
        assert!(child.wait().unwrap().success());
        assert!(
            replacement.is_ok(),
            "The child must not extend host ownership"
        );
        assert!(lock_server(dir.path()).is_err());
        drop(replacement);
        assert!(lock_server(dir.path()).is_ok());
    }
}
