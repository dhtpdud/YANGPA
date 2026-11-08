//! 공개 릴리스(GitHub `dhtpdud/YANGPA`)의 판 번호와 서명 확인. 데몬(확인·내려받기)과 앱(설치 직전 다시 확인)이 함께 씁니다.
//!
//! 릴리스마다 `SHA256SUMS.txt`를 배포자 PC에만 있는 Ed25519 키로 서명한 `SHA256SUMS.txt.sig`를 함께 올립니다
//! (`scripts/sign-release.sh`). 키가 GitHub에 없으므로 GitHub 계정이나 CI가 털려도 서명 없는 파일은 설치하지 않습니다.

use ed25519_dalek::{Signature, VerifyingKey};

/// 원본 저장소. 고친 판은 자기 저장소와 서명 키로 바꿉니다.
pub const REPO: &str = "dhtpdud/YANGPA";
/// 릴리스 서명 공개 키(Ed25519, hex). 비밀 키는 배포자 PC에만 둡니다(docs/release.md).
const RELEASE_KEY_HEX: &str = "ad5b553c58d8304cdec9cb5e35016aac621a8b548392ee68d9aa0c4163a7d037";
/// 릴리스 파일 전부의 SHA-256 목록과 그 서명.
pub const SUMS: &str = "SHA256SUMS.txt";
pub const SUMS_SIG: &str = "SHA256SUMS.txt.sig";

/// `x.y.z`(앞의 `v`는 허용). 시험판(`1.0.0-rc.1`)이나 다른 꼴은 None.
pub fn parse_version(s: &str) -> Option<(u64, u64, u64)> {
    let s = s.strip_prefix('v').unwrap_or(s);
    let mut parts = s.split('.').map(|p| {
        (!p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
            .then(|| p.parse::<u64>().ok())
            .flatten()
    });
    let v = (parts.next()??, parts.next()??, parts.next()??);
    parts.next().is_none().then_some(v)
}

/// `latest`가 `current`보다 새 판인지. 어느 쪽이든 읽지 못하면 아니라고 봅니다.
pub fn is_newer(latest: &str, current: &str) -> bool {
    matches!((parse_version(latest), parse_version(current)), (Some(l), Some(c)) if l > c)
}

/// 그 판의 데스크톱 묶음 파일 이름인지(`YANGPA_<판>_…`, 경로 문자 없음).
pub fn valid_asset(asset: &str, version: &str) -> bool {
    parse_version(version).is_some()
        && !version.starts_with('v')
        && asset
            .strip_prefix(&format!("YANGPA_{version}_"))
            .is_some_and(|rest| {
                !rest.is_empty()
                    && !rest.contains("..")
                    && rest
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
            })
}

/// 서명을 확인한 `SHA256SUMS.txt`에서 그 파일의 SHA-256(소문자 hex). 서명이 틀리거나 목록에 없으면 None.
pub fn signed_sum(sums: &[u8], sig: &[u8], asset: &str) -> Option<String> {
    let key: [u8; 32] = hex::decode(RELEASE_KEY_HEX).ok()?.try_into().ok()?;
    signed_sum_with(&key, sums, sig, asset)
}

fn signed_sum_with(key: &[u8; 32], sums: &[u8], sig: &[u8], asset: &str) -> Option<String> {
    let key = VerifyingKey::from_bytes(key).ok()?;
    key.verify_strict(sums, &Signature::from_slice(sig).ok()?)
        .ok()?;
    std::str::from_utf8(sums).ok()?.lines().find_map(|line| {
        // sha256sum 꼴: `<hex>  <이름>`, 이진 표시는 `<hex> *<이름>`.
        let (hash, name) = line.split_once(' ')?;
        let name = name.trim_start_matches(' ').trim_start_matches('*');
        (name == asset && hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()))
            .then(|| hash.to_ascii_lowercase())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    #[test]
    fn versions() {
        assert_eq!(parse_version("v1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_version("10.0.11"), Some((10, 0, 11)));
        for bad in [
            "1.2",
            "1.2.3.4",
            "1.2.3-rc.1",
            "v",
            "",
            "1..3",
            "+1.2.3",
            "1.2.x",
        ] {
            assert_eq!(parse_version(bad), None, "{bad}");
        }
        assert!(is_newer("v1.0.10", "1.0.9"));
        assert!(is_newer("2.0.0", "1.99.99"));
        assert!(!is_newer("1.0.0", "1.0.0"));
        assert!(!is_newer("0.9.0", "1.0.0"));
        assert!(!is_newer("1.1.0", "1.0.0-rc.1"));
    }

    #[test]
    fn assets() {
        assert!(valid_asset("YANGPA_1.2.3_x64-setup.exe", "1.2.3"));
        assert!(valid_asset("YANGPA_1.2.3_x64_portable.zip", "1.2.3"));
        assert!(!valid_asset("YANGPA_1.2.3_x64-setup.exe", "1.2.4"));
        assert!(!valid_asset("YANGPA_1.2.3_", "1.2.3"));
        assert!(!valid_asset("YANGPA_1.2.3_../../evil.exe", "1.2.3"));
        assert!(!valid_asset("YANGPA_1.2.3_a/b.exe", "1.2.3"));
        assert!(!valid_asset("YANGPA_1.2.3_a\\b.exe", "1.2.3"));
        assert!(!valid_asset("YANGPA_v1.2.3_x.exe", "v1.2.3"));
    }

    /// 실제 배포 키로 `openssl pkeyutl -sign -rawin`이 만든 서명을 받아들이는지(키 쌍·서명 형식이 맞는지 고정).
    #[test]
    fn real_key_accepts_openssl_signature() {
        let sums = b"9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08  YANGPA_9.9.9_x64-setup.exe\n";
        let sig = hex::decode(
            "9815e2061db4824795292f26f62115453330396ad5d47844a6d55622b661b6fb\
             5d97dcbb25d931df828fae86ad9d1cee08ee5f225aa116de32c8006b5be3fc06",
        )
        .unwrap();
        assert_eq!(
            signed_sum(sums, &sig, "YANGPA_9.9.9_x64-setup.exe").as_deref(),
            Some("9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08")
        );
        let mut tampered = sums.to_vec();
        tampered[0] = b'a';
        assert_eq!(
            signed_sum(&tampered, &sig, "YANGPA_9.9.9_x64-setup.exe"),
            None
        );
        assert_eq!(
            signed_sum(sums, &sig[..63], "YANGPA_9.9.9_x64-setup.exe"),
            None
        );
    }

    #[test]
    fn sums_lines() {
        let sk = SigningKey::from_bytes(&[7; 32]);
        let key = sk.verifying_key().to_bytes();
        let a = "a".repeat(64);
        let b = "B".repeat(64);
        let sums = format!(
            "{a}  YANGPA_1.0.0_x.zip\r\n{b} *YANGPA_1.0.0_y.exe\nshort  YANGPA_1.0.0_z.deb\n"
        );
        let sig = sk.sign(sums.as_bytes()).to_bytes();
        let get = |n| signed_sum_with(&key, sums.as_bytes(), &sig, n);
        assert_eq!(get("YANGPA_1.0.0_x.zip"), Some(a));
        assert_eq!(get("YANGPA_1.0.0_y.exe"), Some("b".repeat(64)));
        assert_eq!(get("YANGPA_1.0.0_z.deb"), None);
        assert_eq!(get("YANGPA_1.0.0_w.dmg"), None);
        // 다른 키로 서명했으면 목록을 믿지 않음.
        assert_eq!(
            signed_sum(sums.as_bytes(), &sig, "YANGPA_1.0.0_x.zip"),
            None
        );
    }
}
