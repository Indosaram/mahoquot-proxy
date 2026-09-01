use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static TEMP_FILE_NONCE: AtomicU64 = AtomicU64::new(0x9e37_79b9_7f4a_7c15);

fn random_suffix() -> u64 {
    let counter = TEMP_FILE_NONCE.fetch_add(0x9e37_79b9_7f4a_7c15, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos() as u64)
        .unwrap_or_default();
    let mut value = counter ^ nanos ^ u64::from(std::process::id()).rotate_left(32);
    value ^= value << 13;
    value ^= value >> 7;
    value ^= value << 17;
    value
}

pub fn write_credential_atomically(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let file_name = path
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new("credential"))
        .to_string_lossy();

    for _ in 0..128 {
        let temp_path = parent.join(format!(
            ".{file_name}.tmp.{}.{:016x}",
            std::process::id(),
            random_suffix()
        ));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }

        let mut file = match options.open(&temp_path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        };

        let result = (|| {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                file.set_permissions(fs::Permissions::from_mode(0o600))?;
            }
            file.write_all(bytes)?;
            file.sync_all()?;
            drop(file);
            fs::rename(&temp_path, path)
        })();

        if result.is_err() {
            let _ = fs::remove_file(&temp_path);
        }
        return result;
    }

    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not create a unique credential temp file",
    ))
}

#[cfg(test)]
mod tests {
    use super::write_credential_atomically;
    use std::sync::{Arc, Barrier};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn test_dir(label: &str) -> std::path::PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "mahoquot-credential-file-{label}-{}-{nonce}",
            std::process::id()
        ))
    }

    #[cfg(unix)]
    #[test]
    fn resulting_file_mode_is_owner_read_write_only() {
        use std::os::unix::fs::PermissionsExt;

        let dir = test_dir("mode");
        let path = dir.join("credential.json");

        write_credential_atomically(&path, b"secret").expect("credential write succeeds");

        let mode = std::fs::metadata(&path)
            .expect("credential metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
        std::fs::remove_dir_all(dir).expect("remove test directory");
    }

    #[test]
    fn concurrent_writes_to_same_path_are_complete() {
        let dir = test_dir("concurrent");
        std::fs::create_dir_all(&dir).expect("create test directory");
        let path = dir.join("credential.json");
        let payloads = Arc::new(
            (0..100)
                .map(|index| {
                    format!(
                        "payload-{index:03}-{}",
                        char::from(b'a' + (index % 26) as u8)
                    )
                    .into_bytes()
                })
                .collect::<Vec<_>>(),
        );
        let barrier = Arc::new(Barrier::new(payloads.len() + 1));

        let threads = (0..payloads.len())
            .map(|index| {
                let payload = payloads[index].clone();
                let path = path.clone();
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    write_credential_atomically(&path, &payload)
                })
            })
            .collect::<Vec<_>>();

        barrier.wait();
        for thread in threads {
            thread
                .join()
                .expect("writer thread does not panic")
                .expect("credential write succeeds");
        }

        let final_content = std::fs::read(&path).expect("read final credential");
        assert!(payloads.iter().any(|payload| payload == &final_content));
        std::fs::remove_dir_all(dir).expect("remove test directory");
    }

    #[cfg(unix)]
    #[test]
    fn failed_write_leaves_original_file_intact() {
        use std::os::unix::fs::PermissionsExt;

        let dir = test_dir("failure");
        std::fs::create_dir_all(&dir).expect("create test directory");
        let path = dir.join("credential.json");
        std::fs::write(&path, b"original").expect("write original credential");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500))
            .expect("make directory unwritable");

        let result = write_credential_atomically(&path, b"replacement");

        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
            .expect("restore directory permissions");
        assert!(result.is_err(), "write unexpectedly succeeded");
        assert_eq!(
            std::fs::read(&path).expect("read original credential"),
            b"original"
        );
        std::fs::remove_dir_all(dir).expect("remove test directory");
    }
}
