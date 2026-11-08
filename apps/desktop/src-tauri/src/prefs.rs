//! UI 설정(테마·패널 여닫이·마지막 일반 채널 등) 파일. 웹뷰가 incognito라 localStorage가 남지 않아서 여기 둡니다.
//! 휘발성 채널 데이터는 들어오지 않습니다(프런트엔드 `sanitizePrefs`가 키를 고르고, 여기서는 크기·모양만 검사).

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::error::CmdError;

pub const FILE_NAME: &str = "ui-prefs.json";
/// 파일 최대 크기.
pub const MAX_BYTES: usize = 64 * 1024;
/// 최상위 키 최대 개수.
pub const MAX_KEYS: usize = 64;

pub fn path_in(dir: &Path) -> PathBuf {
    dir.join(FILE_NAME)
}

/// 읽기. 없거나 깨졌거나 너무 크면 빈 객체.
pub fn load(dir: &Path) -> Map<String, Value> {
    let path = path_in(dir);
    let Ok(meta) = fs::metadata(&path) else {
        return Map::new();
    };
    if meta.len() > MAX_BYTES as u64 {
        return Map::new();
    }
    fs::read(&path)
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|v| match v {
            Value::Object(m) if m.len() <= MAX_KEYS => Some(m),
            _ => None,
        })
        .unwrap_or_default()
}

/// 검사 후 직렬화.
pub fn validate(prefs: &Value) -> Result<Vec<u8>, CmdError> {
    let Value::Object(m) = prefs else {
        return Err(CmdError::invalid_params("설정은 객체여야 합니다"));
    };
    if m.len() > MAX_KEYS {
        return Err(CmdError::invalid_params("설정 항목이 너무 많습니다"));
    }
    let bytes = serde_json::to_vec_pretty(prefs).map_err(|e| CmdError::internal(e.to_string()))?;
    if bytes.len() > MAX_BYTES {
        return Err(CmdError::invalid_params("설정이 너무 큽니다"));
    }
    Ok(bytes)
}

/// 임시 파일에 쓰고 바꿔치기(중간에 꺼져도 반쪽 파일이 남지 않게).
pub fn save(dir: &Path, prefs: &Value) -> Result<(), CmdError> {
    let bytes = validate(prefs)?;
    write_atomic(dir, &bytes)
        .map_err(|e| CmdError::internal(format!("설정을 저장하지 못했습니다: {e}")))
}

fn write_atomic(dir: &Path, bytes: &[u8]) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    let tmp = dir.join(format!("{FILE_NAME}.tmp"));
    {
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path_in(dir))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load(dir.path()).is_empty());
        save(dir.path(), &json!({"theme": "light", "memberList": false})).unwrap();
        let m = load(dir.path());
        assert_eq!(m.get("theme"), Some(&json!("light")));
        assert_eq!(m.get("memberList"), Some(&json!(false)));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(path_in(dir.path()))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn rejects_bad_shapes() {
        assert!(validate(&json!([1, 2])).is_err());
        assert!(validate(&json!("x")).is_err());
        let big = json!({"x": "a".repeat(MAX_BYTES)});
        assert!(validate(&big).is_err());
        let many: Map<String, Value> = (0..=MAX_KEYS)
            .map(|i| (format!("k{i}"), json!(i)))
            .collect();
        assert!(validate(&Value::Object(many)).is_err());
    }

    #[test]
    fn corrupt_file_reads_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(path_in(dir.path()), b"{not json").unwrap();
        assert!(load(dir.path()).is_empty());
        fs::write(path_in(dir.path()), b"[1,2,3]").unwrap();
        assert!(load(dir.path()).is_empty());
        fs::write(path_in(dir.path()), vec![b' '; MAX_BYTES + 1]).unwrap();
        assert!(load(dir.path()).is_empty());
    }
}
