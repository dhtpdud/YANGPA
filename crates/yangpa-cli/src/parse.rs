//! 명령줄 값 해석: 크기(`2MB`), 속도(`2MB/s`), 기간(`24h`, `30s`), 채팅 수명, 색, 목록.

use std::collections::BTreeMap;
use std::time::Duration;

use yangpa_core::model::Permissions;
use yangpa_rpc::api::{DataType, Scope};

/// clap 파생이 `Vec`을 '여러 번 받는 인자'로 해석하지 않게 하는 별칭입니다.
/// 쉼표 목록 하나를 값 해석기가 통째로 검사합니다(`--perms manage_channels,kick`).
pub type NameList = Vec<String>;
/// 데이터 종류 목록(별칭인 이유는 [`NameList`] 참고).
pub type TypeList = Vec<DataType>;
/// 토큰 범위 목록(별칭인 이유는 [`NameList`] 참고).
pub type ScopeList = Vec<Scope>;

/// 크기. 단위는 1024 배수입니다(`KB`=`KiB`=1024). 소수 허용(`1.5MB`).
/// 비트 단위 속도 표기(`kbps`, `mbps`, `gbps`)는 1000 배수 비트로 보고 바이트로 바꿉니다.
pub fn parse_size(s: &str) -> Result<u64, String> {
    let t = s.trim();
    if t.is_empty() {
        return Err("크기가 비었습니다".into());
    }
    let split = t
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(t.len());
    let (num, unit) = t.split_at(split);
    let num: f64 = num
        .parse()
        .map_err(|_| format!("크기 숫자를 읽을 수 없습니다: {s}"))?;
    if !num.is_finite() || num < 0.0 {
        return Err(format!("잘못된 크기: {s}"));
    }
    let unit = unit.trim().to_ascii_lowercase();
    let factor: f64 = match unit.as_str() {
        "" | "b" => 1.0,
        "k" | "kb" | "kib" => 1024.0,
        "m" | "mb" | "mib" => 1024.0 * 1024.0,
        "g" | "gb" | "gib" => 1024.0 * 1024.0 * 1024.0,
        "t" | "tb" | "tib" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        "bps" => 1.0 / 8.0,
        "kbps" => 1000.0 / 8.0,
        "mbps" => 1_000_000.0 / 8.0,
        "gbps" => 1_000_000_000.0 / 8.0,
        _ => {
            return Err(format!(
                "알 수 없는 크기 단위: {unit} (B, KB, MB, GB, TB, kbps, mbps, gbps)"
            ));
        }
    };
    let bytes = (num * factor).round();
    if bytes > u64::MAX as f64 {
        return Err(format!("크기가 너무 큽니다: {s}"));
    }
    Ok(bytes as u64)
}

/// 대역폭 상한(바이트/초). `/s`는 붙여도 되고 빼도 됩니다. `unlimited`·`none`·`off`·`0`은 무제한(`None`).
pub fn parse_rate(s: &str) -> Result<Option<u64>, String> {
    let t = s.trim();
    let lower = t.to_ascii_lowercase();
    if matches!(
        lower.as_str(),
        "unlimited" | "none" | "off" | "무제한" | "0"
    ) {
        return Ok(None);
    }
    let body = lower
        .strip_suffix("/s")
        .or_else(|| lower.strip_suffix("/sec"))
        .unwrap_or(&lower);
    let v = parse_size(body)?;
    if v == 0 {
        return Ok(None);
    }
    Ok(Some(v))
}

/// clap용: 속도.
pub fn rate_arg(s: &str) -> Result<RateArg, String> {
    parse_rate(s).map(RateArg)
}

/// clap 값 타입(`Option<u64>`를 clap이 선택 인자로 오해하지 않게 감쌈).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RateArg(pub Option<u64>);

/// 기간(`30s`, `24h`, `1h 30m`, `2d`). humantime 형식.
pub fn parse_duration(s: &str) -> Result<Duration, String> {
    let t = s.trim();
    // 단위 없는 숫자는 초로 봅니다.
    if !t.is_empty() && t.bytes().all(|b| b.is_ascii_digit()) {
        return t
            .parse::<u64>()
            .map(Duration::from_secs)
            .map_err(|e| e.to_string());
    }
    humantime::parse_duration(t).map_err(|e| format!("기간을 읽을 수 없습니다({t}): {e}"))
}

/// 만료 기간. `never`·`none`이면 만료 없음.
pub fn parse_expiry(s: &str) -> Result<ExpiryArg, String> {
    let t = s.trim().to_ascii_lowercase();
    if matches!(t.as_str(), "never" | "none" | "off" | "무기한") {
        return Ok(ExpiryArg(None));
    }
    let d = parse_duration(&t)?;
    if d.as_secs() == 0 {
        return Err("만료 기간은 1초 이상이어야 합니다".into());
    }
    Ok(ExpiryArg(Some(d.as_secs())))
}

/// 만료까지 초(`None`이면 만료 없음).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExpiryArg(pub Option<u64>);

/// 채팅 수명.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TtlArg {
    /// 수명 없음.
    None,
    /// 퇴장 시 삭제.
    OnLeave,
    /// 수신 후 N초.
    Seconds(u32),
}

/// `30s`, `5m`, `on-leave`(퇴장 시 삭제), `none`.
pub fn parse_ttl(s: &str) -> Result<TtlArg, String> {
    let t = s.trim().to_ascii_lowercase();
    match t.as_str() {
        "none" | "off" | "0" => return Ok(TtlArg::None),
        "on-leave" | "onleave" | "leave" | "퇴장" => return Ok(TtlArg::OnLeave),
        _ => {}
    }
    let d = parse_duration(&t)?;
    if d.subsec_nanos() != 0 {
        return Err("채팅 수명은 초 단위입니다".into());
    }
    let secs = u32::try_from(d.as_secs()).map_err(|_| "채팅 수명이 너무 깁니다".to_string())?;
    if secs == 0 {
        return Ok(TtlArg::None);
    }
    Ok(TtlArg::Seconds(secs))
}

/// 색: `#RRGGBB`, `RRGGBB`, `0xRRGGBB`.
pub fn parse_color(s: &str) -> Result<u32, String> {
    let t = s.trim();
    let hex = t
        .strip_prefix('#')
        .or_else(|| t.strip_prefix("0x"))
        .or_else(|| t.strip_prefix("0X"))
        .unwrap_or(t);
    if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("색은 #RRGGBB 형식이어야 합니다: {s}"));
    }
    u32::from_str_radix(hex, 16).map_err(|e| e.to_string())
}

/// `on`/`off` 같은 켜기·끄기.
pub fn parse_switch(s: &str) -> Result<bool, String> {
    match s.trim().to_ascii_lowercase().as_str() {
        "on" | "true" | "yes" | "1" | "enable" | "켬" => Ok(true),
        "off" | "false" | "no" | "0" | "disable" | "끔" => Ok(false),
        _ => Err(format!("on 또는 off: {s}")),
    }
}

/// 권한 이름 목록(`manage_channels,kick`). 이름을 미리 검사합니다.
pub fn parse_permissions(s: &str) -> Result<Vec<String>, String> {
    Permissions::parse_list(s)?;
    Ok(split_list(s))
}

/// 데이터 종류 목록(`text,emoji`).
pub fn parse_types(s: &str) -> Result<Vec<DataType>, String> {
    split_list(s).iter().map(|t| t.parse()).collect()
}

/// 토큰 범위 목록(`read,write`).
pub fn parse_scopes(s: &str) -> Result<Vec<Scope>, String> {
    let v: Vec<Scope> = split_list(s)
        .iter()
        .map(|t| t.parse())
        .collect::<Result<_, _>>()?;
    if v.is_empty() {
        return Err("범위를 하나 이상 지정하세요(read, write, admin, settings, owner)".into());
    }
    Ok(v)
}

/// 종류별 우선순위(`text=250,image=100`).
pub fn parse_priorities(s: &str) -> Result<BTreeMap<DataType, u8>, String> {
    let mut out = BTreeMap::new();
    for item in split_list(s) {
        let (k, v) = item
            .split_once('=')
            .ok_or_else(|| format!("종류=우선순위 형식이어야 합니다: {item}"))?;
        let t: DataType = k.trim().parse()?;
        let p: u8 = v
            .trim()
            .parse()
            .map_err(|_| format!("우선순위는 0~255: {v}"))?;
        out.insert(t, p);
    }
    Ok(out)
}

/// 쉼표 목록. 빈 항목은 버립니다.
pub fn split_list(s: &str) -> Vec<String> {
    s.split(',')
        .map(str::trim)
        .filter(|x| !x.is_empty())
        .map(str::to_owned)
        .collect()
}

/// 바이트 수를 사람이 읽기 쉽게(1024 배수).
pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{bytes}B")
    } else {
        format!("{v:.1}{}", UNITS[i])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(parse_size("100").unwrap(), 100);
        assert_eq!(parse_size("2MB").unwrap(), 2 * 1024 * 1024);
        assert_eq!(parse_size("2mb").unwrap(), 2 * 1024 * 1024);
        assert_eq!(parse_size("512KiB").unwrap(), 512 * 1024);
        assert_eq!(parse_size("1.5M").unwrap(), 1_572_864);
        assert_eq!(parse_size("1G").unwrap(), 1 << 30);
        assert_eq!(parse_size("8mbps").unwrap(), 1_000_000);
        assert_eq!(parse_size(" 20 MB ").unwrap(), 20 << 20);
        assert!(parse_size("").is_err());
        assert!(parse_size("MB").is_err());
        assert!(parse_size("-1MB").is_err());
        assert!(parse_size("3 parsecs").is_err());
        assert!(parse_size("99999999999TB").is_err());
    }

    #[test]
    fn rates() {
        assert_eq!(parse_rate("2MB/s").unwrap(), Some(2 * 1024 * 1024));
        assert_eq!(parse_rate("512KB").unwrap(), Some(512 * 1024));
        assert_eq!(parse_rate("100mbps").unwrap(), Some(12_500_000));
        assert_eq!(parse_rate("unlimited").unwrap(), None);
        assert_eq!(parse_rate("0").unwrap(), None);
        assert!(parse_rate("fast").is_err());
    }

    #[test]
    fn durations_and_ttl() {
        assert_eq!(parse_duration("24h").unwrap(), Duration::from_secs(86_400));
        assert_eq!(parse_duration("30s").unwrap(), Duration::from_secs(30));
        assert_eq!(
            parse_duration("1h 30m").unwrap(),
            Duration::from_secs(5_400)
        );
        assert_eq!(parse_duration("45").unwrap(), Duration::from_secs(45));
        assert!(parse_duration("soon").is_err());
        assert_eq!(parse_expiry("24h").unwrap(), ExpiryArg(Some(86_400)));
        assert_eq!(parse_expiry("never").unwrap(), ExpiryArg(None));
        assert!(parse_expiry("0s").is_err());
        assert_eq!(parse_ttl("30s").unwrap(), TtlArg::Seconds(30));
        assert_eq!(parse_ttl("5m").unwrap(), TtlArg::Seconds(300));
        assert_eq!(parse_ttl("on-leave").unwrap(), TtlArg::OnLeave);
        assert_eq!(parse_ttl("none").unwrap(), TtlArg::None);
        assert!(parse_ttl("1500ms").is_err());
        assert!(parse_ttl("200years").is_err());
    }

    #[test]
    fn colors_lists_and_switches() {
        assert_eq!(parse_color("#ff8800").unwrap(), 0xff8800);
        assert_eq!(parse_color("0x00FF00").unwrap(), 0x00ff00);
        assert!(parse_color("#fff").is_err());
        assert!(parse_switch("on").unwrap());
        assert!(!parse_switch("OFF").unwrap());
        assert!(parse_switch("maybe").is_err());
        assert_eq!(
            parse_permissions("manage_channels, kick").unwrap(),
            vec!["manage_channels", "kick"]
        );
        assert!(parse_permissions("manage_channels,fly").is_err());
        assert_eq!(
            parse_types("text,emoji").unwrap(),
            vec![DataType::Text, DataType::Emoji]
        );
        assert!(parse_types("text,sound").is_err());
        assert_eq!(
            parse_scopes("read,write").unwrap(),
            vec![Scope::Read, Scope::Write]
        );
        assert!(parse_scopes("").is_err());
        let p = parse_priorities("text=250,image=100").unwrap();
        assert_eq!(p[&DataType::Image], 100);
        assert!(parse_priorities("text").is_err());
        assert!(parse_priorities("text=300").is_err());
        assert_eq!(split_list("a,,b ,"), vec!["a", "b"]);
    }

    #[test]
    fn size_format() {
        assert_eq!(format_size(512), "512B");
        assert_eq!(format_size(2 * 1024 * 1024), "2.0MiB");
    }
}
