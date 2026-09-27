use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::Serialize;

static NEXT_TEMP_FILE: AtomicU64 = AtomicU64::new(0);

pub(crate) fn write_json_atomic<T: Serialize + ?Sized>(path: &Path, value: &T) -> io::Result<()> {
    let bytes = serde_json::to_vec_pretty(value)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("state.json");
    let sequence = NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed);
    let temp = parent.join(format!(
        ".{file_name}.{}.{}.tmp",
        std::process::id(),
        sequence
    ));

    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::ser::Error as _;
    use serde::{Serialize, Serializer};
    use std::sync::atomic::{AtomicU64, Ordering};

    fn temp_dir() -> std::path::PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "zradio-storage-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[test]
    fn atomic_json_write_replaces_value_without_temp_files() {
        let dir = temp_dir();
        let path = dir.join("state.json");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, r#"{"value":"old"}"#).unwrap();

        write_json_atomic(&path, &serde_json::json!({"value": "new"})).unwrap();

        let stored: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(stored, serde_json::json!({"value": "new"}));
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    struct FailsToSerialize;

    impl Serialize for FailsToSerialize {
        fn serialize<S>(&self, _serializer: S) -> Result<S::Ok, S::Error>
        where
            S: Serializer,
        {
            Err(S::Error::custom("intentional failure"))
        }
    }

    #[test]
    fn serialization_failure_keeps_previous_file() {
        let dir = temp_dir();
        let path = dir.join("state.json");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, "previous").unwrap();

        assert!(write_json_atomic(&path, &FailsToSerialize).is_err());

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "previous");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
