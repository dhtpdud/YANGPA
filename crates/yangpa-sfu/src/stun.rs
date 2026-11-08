//! 서버 반사 후보를 얻는 최소 STUN 바인딩(RFC 8489). 공개 STUN 서버의 응답에는 MESSAGE-INTEGRITY가 없어
//! str0m의 ICE용 파서가 받지 않으므로 바인딩 요청·응답만 따로 다룹니다.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

const MAGIC: [u8; 4] = [0x21, 0x12, 0xA4, 0x42];
const BINDING_REQUEST: u16 = 0x0001;
const BINDING_SUCCESS: u16 = 0x0101;
const ATTR_MAPPED_ADDRESS: u16 = 0x0001;
const ATTR_XOR_MAPPED_ADDRESS: u16 = 0x0020;

/// 트랜잭션 ID로 바인딩 요청을 만듭니다(속성 없음).
pub fn binding_request(trans_id: [u8; 12]) -> Vec<u8> {
    let mut out = Vec::with_capacity(20);
    out.extend_from_slice(&BINDING_REQUEST.to_be_bytes());
    out.extend_from_slice(&0u16.to_be_bytes());
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&trans_id);
    out
}

/// `buf`가 `trans_id`에 대한 바인딩 성공 응답이면 그 매핑 주소를 돌려줍니다.
pub fn parse_binding_response(buf: &[u8], trans_id: &[u8; 12]) -> Option<SocketAddr> {
    if buf.len() < 20 || buf[4..8] != MAGIC || &buf[8..20] != trans_id {
        return None;
    }
    if u16::from_be_bytes([buf[0], buf[1]]) != BINDING_SUCCESS {
        return None;
    }
    let len = u16::from_be_bytes([buf[2], buf[3]]) as usize;
    let body = buf.get(20..20 + len)?;
    let mut plain = None;
    let mut pos = 0;
    while pos + 4 <= body.len() {
        let typ = u16::from_be_bytes([body[pos], body[pos + 1]]);
        let alen = u16::from_be_bytes([body[pos + 2], body[pos + 3]]) as usize;
        let value = body.get(pos + 4..pos + 4 + alen)?;
        match typ {
            ATTR_XOR_MAPPED_ADDRESS => return decode_address(value, Some(&buf[4..20])),
            ATTR_MAPPED_ADDRESS => plain = decode_address(value, None),
            _ => {}
        }
        // 속성은 4바이트 경계로 채워짐.
        pos += 4 + alen.div_ceil(4) * 4;
    }
    plain
}

/// 바인딩 응답(성공·오류)이면 그 트랜잭션 ID. 같은 소켓으로 들어온 패킷 중 내가 보낸 STUN 서버 요청의 응답만
/// 가르는 데 씁니다(참가자가 ICE 연결 확인에 답한 응답도 같은 바인딩 응답이라 트랜잭션으로 구별해야 함).
pub fn binding_response_id(buf: &[u8]) -> Option<[u8; 12]> {
    let is_response = buf.len() >= 20
        && buf[4..8] == MAGIC
        && matches!(
            u16::from_be_bytes([buf[0], buf[1]]),
            BINDING_SUCCESS | 0x0111
        );
    if !is_response {
        return None;
    }
    buf[8..20].try_into().ok()
}

fn decode_address(value: &[u8], xor: Option<&[u8]>) -> Option<SocketAddr> {
    if value.len() < 4 {
        return None;
    }
    let family = value[1];
    let mut port = u16::from_be_bytes([value[2], value[3]]);
    if let Some(key) = xor {
        port ^= u16::from_be_bytes([key[0], key[1]]);
    }
    let ip = match family {
        0x01 => {
            let raw: [u8; 4] = value.get(4..8)?.try_into().ok()?;
            let mut b = raw;
            if let Some(key) = xor {
                for (i, v) in b.iter_mut().enumerate() {
                    *v ^= key[i];
                }
            }
            IpAddr::V4(Ipv4Addr::from(b))
        }
        0x02 => {
            let raw: [u8; 16] = value.get(4..20)?.try_into().ok()?;
            let mut b = raw;
            if let Some(key) = xor {
                for (i, v) in b.iter_mut().enumerate() {
                    *v ^= key[i];
                }
            }
            IpAddr::V6(Ipv6Addr::from(b))
        }
        _ => return None,
    };
    Some(SocketAddr::new(ip, port))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 5769 2.2의 IPv4 응답 예(192.0.2.1:32853), 소프트웨어·무결성 속성 포함.
    const RFC5769_V4: &str = "0101003c2112a442b7e7a701bc34d686fa87dfae8022000b\
                               7465737420766563746f7220\
                               002000080001a147e112a643\
                               000800142b91f599fd9e90c38c7489f92af9ba53f06be7d7\
                               80280004c07d4c96";

    fn hex(s: &str) -> Vec<u8> {
        let s: String = s.chars().filter(|c| !c.is_whitespace()).collect();
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn parses_rfc5769_response() {
        let buf = hex(RFC5769_V4);
        let tid: [u8; 12] = buf[8..20].try_into().unwrap();
        assert_eq!(binding_response_id(&buf), Some(tid));
        assert_eq!(
            parse_binding_response(&buf, &tid),
            Some("192.0.2.1:32853".parse().unwrap())
        );
        let mut other = tid;
        other[0] ^= 1;
        assert_eq!(parse_binding_response(&buf, &other), None);
    }

    #[test]
    fn request_shape() {
        let req = binding_request([7; 12]);
        assert_eq!(req.len(), 20);
        assert_eq!(&req[..4], &[0, 1, 0, 0]);
        assert_eq!(binding_response_id(&req), None);
    }
}
