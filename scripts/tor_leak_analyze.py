#!/usr/bin/env python3
"""Tor 모드 패킷 캡처 분석기.

캡처(pcap)에서 시험 노드(--local)가 내보낸 패킷의 목적지가 모두 Tor 릴레이(합의 문서의 OR/디렉터리 주소,
Arti에 내장된 폴백 디렉터리·디렉터리 권한 주소)인지 봅니다. 하나라도 아니면(직접 연결, DNS 조회, mDNS, UDP,
IPv6 등) 실패합니다. 외부 패키지 없이 표준 라이브러리만 씁니다.

`--phases`(줄마다 `유닉스 시각<TAB>단계 이름`, tor-leak-check.sh가 씀)를 주면 패킷을 캡처 시각으로 단계에 나눠
단계별 나간 패킷·UDP·누출을 함께 보여 줍니다(어느 기능에서 샜는지 바로 보이게). 판정 기준은 같습니다.

`--direct-phase`·`--direct-allow`(줄마다 `IP:포트`)를 주면 그 단계가 시작된 뒤의 UDP만 목록의 목적지와, 목록의 STUN
서버가 알려 준 내 공인 IP(새 기기가 링크의 공인 주소로 겁니다)를 누출이 아닌 '직접 연결'로 따로 셉니다. 내 기기 직접
연결은 설계상 Tor 밖으로 나가므로(decisions.md "첫 동기화와 내 기기 직접 연결") 그 길이 쓰는 곳만 넘기고, 그 단계 앞의
패킷과 그 밖의 목적지는 그대로 누출입니다. 내 공인 IP는 결과에 적지 않습니다. 단계를 지났는데 직접 연결이 하나도 없으면
그 길을 밟지 못한 것이니 실패합니다.

    tor_leak_analyze.py analyze --pcap cap.pcap --local 10.231.0.2 \
        --consensus-dir A/arti/cache/dir_blobs --consensus-dir B/arti/cache/dir_blobs \
        --dircommon ~/.cargo/registry/src/.../tor-dircommon-0.47.0 --phases phases.tsv \
        --direct-phase own_device --direct-allow direct-allow.txt
    tor_leak_analyze.py selftest
"""

from __future__ import annotations

import argparse
import ipaddress
import json
import os
import re
import struct
import sys
import tempfile
from collections import Counter
from dataclasses import dataclass, field

# pcap 링크 형식.
LINKTYPE_NULL = 0
LINKTYPE_ETHERNET = 1
LINKTYPE_RAW = 101
LINKTYPE_RAW_ALT = 12
LINKTYPE_LINUX_SLL = 113
LINKTYPE_LINUX_SLL2 = 276

ETH_IPV4 = 0x0800
ETH_IPV6 = 0x86DD
ETH_VLAN = (0x8100, 0x88A8)

PROTO_TCP = 6
PROTO_UDP = 17

# STUN(RFC 5389): 매직 쿠키, Binding Success, XOR-MAPPED-ADDRESS.
STUN_COOKIE = b"\x21\x12\xa4\x42"
STUN_BINDING_SUCCESS = b"\x01\x01"
STUN_XOR_MAPPED = 0x0020
# 결과에 내 공인 IP 대신 적는 이름(사람들이 결과를 붙여 넣어도 IP가 나가지 않게).
MY_PUBLIC = "(내 공인 주소)"

# 따옴표 안의 `IP:포트`·`[IPv6]:포트`(폴백 디렉터리·권한 목록 소스에서 꺼냄).
QUOTED_ADDR = re.compile(r'"(\[[0-9A-Fa-f:.]+\]:\d+|\d{1,3}(?:\.\d{1,3}){3}:\d+)"')


@dataclass(frozen=True)
class Packet:
    src: str
    dst: str
    proto: str  # "tcp" | "udp" | "ip<n>" | "ipv6-<n>"
    sport: int | None
    dport: int | None
    ts: float = 0.0  # 캡처 시각(유닉스 초)
    payload: bytes = b""  # UDP 내용(들어온 STUN 답에서 내 공인 IP를 읽음)


@dataclass
class Allowlist:
    pairs: set[tuple[str, int]] = field(default_factory=set)
    consensus_files: int = 0
    relays: int = 0
    builtin: int = 0

    def allows(self, ip: str, port: int | None) -> bool:
        return port is not None and (norm_ip(ip), port) in self.pairs


def norm_ip(ip: str) -> str:
    return str(ipaddress.ip_address(ip))


def split_hostport(s: str) -> tuple[str, int] | None:
    s = s.strip()
    if s.startswith("["):
        host, _, rest = s[1:].partition("]")
        port = rest.lstrip(":")
    else:
        host, _, port = s.rpartition(":")
    try:
        return norm_ip(host), int(port)
    except ValueError:
        return None


# ───────────── 허용 목록 ─────────────


def parse_consensus(text: str, allow: Allowlist) -> int:
    """마이크로설명 합의 문서의 `r`(IPv4·OR·디렉터리 포트)·`a`(추가 주소) 줄."""
    n = 0
    for line in text.splitlines():
        if line.startswith("r "):
            parts = line.split()
            # r 별명 신원 날짜 시각 IP OR포트 디렉터리포트
            if len(parts) >= 8:
                try:
                    ip = norm_ip(parts[5])
                    allow.pairs.add((ip, int(parts[6])))
                    if int(parts[7]) != 0:
                        allow.pairs.add((ip, int(parts[7])))
                    n += 1
                except ValueError:
                    continue
        elif line.startswith("a "):
            hp = split_hostport(line[2:])
            if hp:
                allow.pairs.add(hp)
    return n


def load_direct_allow(path: str | None) -> set[tuple[str, int]]:
    """`IP:포트` 줄들."""
    if not path:
        return set()
    with open(path, encoding="utf-8") as f:
        return {hp for hp in map(split_hostport, f) if hp}


def load_consensus_dirs(dirs: list[str], allow: Allowlist) -> None:
    for d in dirs:
        if not os.path.isdir(d):
            continue
        for name in sorted(os.listdir(d)):
            path = os.path.join(d, name)
            if not os.path.isfile(path):
                continue
            with open(path, "rb") as f:
                head = f.read(64)
                if not head.startswith(b"network-status-version 3"):
                    continue
                f.seek(0)
                text = f.read().decode("utf-8", "replace")
            allow.relays += parse_consensus(text, allow)
            allow.consensus_files += 1


def load_builtin(dircommon: str | None, allow: Allowlist) -> None:
    if not dircommon:
        return
    for rel in ("data/fallback_dirs.rs", "src/authority.rs"):
        path = os.path.join(dircommon, rel)
        if not os.path.isfile(path):
            continue
        with open(path, encoding="utf-8") as f:
            for m in QUOTED_ADDR.finditer(f.read()):
                hp = split_hostport(m.group(1))
                if hp:
                    allow.pairs.add(hp)
                    allow.builtin += 1


# ───────────── pcap ─────────────


def read_pcap(path: str) -> tuple[int, list[tuple[float, bytes]]]:
    """(링크 형식, [(캡처 시각, 프레임)])."""
    with open(path, "rb") as f:
        data = f.read()
    if len(data) < 24:
        raise ValueError("pcap 머리가 없음")
    magic = data[:4]
    # 마이크로초 또는 나노초 단위 시각(tcpdump --time-stamp-precision=nano).
    if magic in (b"\xd4\xc3\xb2\xa1", b"\x4d\x3c\xb2\xa1"):
        endian = "<"
    elif magic in (b"\xa1\xb2\xc3\xd4", b"\xa1\xb2\x3c\x4d"):
        endian = ">"
    elif magic == b"\x0a\x0d\x0d\x0a":
        raise ValueError("pcapng는 읽지 않습니다(tcpdump -w는 pcap을 씀)")
    else:
        raise ValueError("pcap 형식이 아님")
    frac = 1e-9 if magic in (b"\x4d\x3c\xb2\xa1", b"\xa1\xb2\x3c\x4d") else 1e-6
    linktype = struct.unpack(endian + "I", data[20:24])[0] & 0x0FFFFFFF
    frames = []
    off = 24
    while off + 16 <= len(data):
        sec, sub, incl = struct.unpack(endian + "III", data[off : off + 12])
        off += 16
        frames.append((sec + sub * frac, data[off : off + incl]))
        off += incl
    return linktype, frames


def l3_of(linktype: int, frame: bytes) -> tuple[int, bytes] | None:
    """(이더 형식, L3 바이트)."""
    if linktype == LINKTYPE_ETHERNET:
        if len(frame) < 14:
            return None
        et = struct.unpack("!H", frame[12:14])[0]
        off = 14
        while et in ETH_VLAN and len(frame) >= off + 4:
            et = struct.unpack("!H", frame[off + 2 : off + 4])[0]
            off += 4
        return et, frame[off:]
    if linktype == LINKTYPE_LINUX_SLL:
        if len(frame) < 16:
            return None
        return struct.unpack("!H", frame[14:16])[0], frame[16:]
    if linktype == LINKTYPE_LINUX_SLL2:
        if len(frame) < 20:
            return None
        return struct.unpack("!H", frame[0:2])[0], frame[20:]
    if linktype in (LINKTYPE_RAW, LINKTYPE_RAW_ALT):
        if not frame:
            return None
        v = frame[0] >> 4
        return (ETH_IPV4 if v == 4 else ETH_IPV6), frame
    if linktype == LINKTYPE_NULL:
        if len(frame) < 4:
            return None
        fam = struct.unpack("<I", frame[:4])[0]
        return (ETH_IPV4 if fam == 2 else ETH_IPV6), frame[4:]
    raise ValueError(f"지원하지 않는 링크 형식 {linktype}")


def parse_ip(et: int, b: bytes, ts: float = 0.0) -> Packet | None:
    if et == ETH_IPV4 and len(b) >= 20:
        ihl = (b[0] & 0x0F) * 4
        proto = b[9]
        src = str(ipaddress.IPv4Address(b[12:16]))
        dst = str(ipaddress.IPv4Address(b[16:20]))
        frag_off = struct.unpack("!H", b[6:8])[0] & 0x1FFF
        l4 = b[ihl:]
    elif et == ETH_IPV6 and len(b) >= 40:
        proto = b[6]
        src = str(ipaddress.IPv6Address(b[8:24]))
        dst = str(ipaddress.IPv6Address(b[24:40]))
        frag_off = 0
        l4 = b[40:]
    else:
        return None
    if proto in (PROTO_TCP, PROTO_UDP) and frag_off == 0 and len(l4) >= 4:
        sport, dport = struct.unpack("!HH", l4[:4])
        if proto == PROTO_TCP:
            return Packet(src, dst, "tcp", sport, dport, ts)
        return Packet(src, dst, "udp", sport, dport, ts, l4[8:])
    tag = f"ip{proto}" if et == ETH_IPV4 else f"ipv6-{proto}"
    return Packet(src, dst, tag, None, None, ts)


def packets(path: str) -> list[Packet]:
    linktype, frames = read_pcap(path)
    out = []
    for ts, fr in frames:
        l3 = l3_of(linktype, fr)
        if not l3:
            continue
        p = parse_ip(*l3, ts)
        if p:
            out.append(p)
    return out


# ───────────── 단계 ─────────────

# 첫 단계 표시보다 앞선 패킷.
BEFORE_PHASES = "(시작 전)"


def load_phases(path: str | None) -> list[tuple[float, str]]:
    """`시각<TAB>이름` 줄들(시각 순으로 정렬). 빈 줄·형식이 틀린 줄은 건너뜁니다."""
    if not path:
        return []
    out = []
    with open(path, encoding="utf-8") as f:
        for line in f:
            ts, sep, name = line.rstrip("\n").partition("\t")
            if not sep or not name:
                continue
            try:
                out.append((float(ts), name))
            except ValueError:
                continue
    out.sort(key=lambda x: x[0])
    return out


def phase_of(phases: list[tuple[float, str]], ts: float) -> str | None:
    """시각이 속한 단계(시작 시각이 그 시각 이하인 마지막 단계)."""
    if not phases:
        return None
    name = BEFORE_PHASES
    for start, n in phases:
        if start > ts:
            break
        name = n
    return name


# ───────────── STUN ─────────────


def stun_mapped_ip(udp: bytes) -> str | None:
    """Binding Success의 XOR-MAPPED-ADDRESS(IPv4)."""
    if len(udp) < 20 or udp[:2] != STUN_BINDING_SUCCESS or udp[4:8] != STUN_COOKIE:
        return None
    off = 20
    while off + 4 <= len(udp):
        kind, n = struct.unpack("!HH", udp[off : off + 4])
        value = udp[off + 4 : off + 4 + n]
        if kind == STUN_XOR_MAPPED and len(value) == 8 and value[1] == 1:
            return str(ipaddress.IPv4Address(bytes(a ^ b for a, b in zip(value[4:8], STUN_COOKIE))))
        off += 4 + (n + 3) // 4 * 4  # 속성은 4바이트로 맞춰 붙음
    return None


def my_public_ips(
    pkts: list[Packet], local: set[str], direct_from: float, direct_allow: set[tuple[str, int]]
) -> set[str]:
    """그 단계부터 허용한 STUN 서버가 답한 내 공인 IP."""
    found = (
        stun_mapped_ip(p.payload)
        for p in pkts
        if p.proto == "udp"
        and p.dst in local
        and p.ts >= direct_from
        and (norm_ip(p.src), p.sport) in direct_allow
    )
    return {ip for ip in found if ip}


# ───────────── 판정 ─────────────


@dataclass
class PhaseStats:
    outbound: int = 0
    to_relays: int = 0
    udp: int = 0
    direct: int = 0
    violations: Counter = field(default_factory=Counter)


@dataclass
class Report:
    outbound: int = 0
    to_relays: int = 0
    udp: int = 0
    relay_endpoints: set[tuple[str, int]] = field(default_factory=set)
    # 내 기기 단계부터 허용한 목적지(`--direct-allow`, 내 공인 IP)로 간 패킷.
    direct: Counter = field(default_factory=Counter)
    violations: Counter = field(default_factory=Counter)
    # 단계 이름 → 통계(`--phases`가 있을 때만).
    phases: dict[str, PhaseStats] = field(default_factory=dict)

    def ok(self, min_relay_packets: int) -> bool:
        return not self.violations and self.to_relays >= min_relay_packets


def judge(
    pkts: list[Packet],
    local: set[str],
    allow: Allowlist,
    phases: list[tuple[float, str]] | None = None,
    direct_from: float | None = None,
    direct_allow: set[tuple[str, int]] | None = None,
) -> Report:
    r = Report()
    phases = phases or []
    direct_allow = direct_allow or set()
    mine = my_public_ips(pkts, local, direct_from, direct_allow) if direct_from is not None else set()
    # 시각 순서대로 보이게 미리 만들어 둡니다(패킷이 없던 단계도 0으로 남음).
    for n in ([BEFORE_PHASES] if phases else []) + [n for _, n in phases]:
        r.phases.setdefault(n, PhaseStats())
    for p in pkts:
        if p.src not in local:
            continue
        name = phase_of(phases, p.ts)
        ph = r.phases.setdefault(name, PhaseStats()) if name else PhaseStats()
        r.outbound += 1
        ph.outbound += 1
        if p.proto == "udp":
            r.udp += 1
            ph.udp += 1
        direct_on = direct_from is not None and p.ts >= direct_from and p.proto == "udp"
        if p.proto == "tcp" and allow.allows(p.dst, p.dport):
            r.to_relays += 1
            ph.to_relays += 1
            r.relay_endpoints.add((p.dst, p.dport))
        elif direct_on and p.dst in mine:
            r.direct[(p.proto, MY_PUBLIC, None)] += 1
            ph.direct += 1
        elif direct_on and (norm_ip(p.dst), p.dport) in direct_allow:
            r.direct[(p.proto, p.dst, p.dport)] += 1
            ph.direct += 1
        else:
            r.violations[(p.proto, p.dst, p.dport)] += 1
            ph.violations[(p.proto, p.dst, p.dport)] += 1
    return r


def violation_list(c: Counter) -> list[dict]:
    return [
        {"proto": k[0], "dst": k[1], "port": k[2], "packets": v}
        for k, v in sorted(c.items(), key=lambda kv: (-kv[1], str(kv[0])))
    ]


def cmd_analyze(a: argparse.Namespace) -> int:
    allow = Allowlist()
    load_consensus_dirs(a.consensus_dir, allow)
    load_builtin(a.dircommon, allow)
    if allow.consensus_files == 0:
        print("오류: 합의 문서를 찾지 못해 릴레이 목록을 만들 수 없습니다", file=sys.stderr)
        return 2
    local = {norm_ip(x) for x in a.local}
    phases = load_phases(a.phases)
    direct_from = next((t for t, n in phases if n == a.direct_phase), None)
    r = judge(packets(a.pcap), local, allow, phases, direct_from, load_direct_allow(a.direct_allow))
    summary = {
        "consensus_files": allow.consensus_files,
        "relays": allow.relays,
        "builtin_addrs": allow.builtin,
        "outbound_packets": r.outbound,
        "packets_to_relays": r.to_relays,
        "udp_packets": r.udp,
        "relay_endpoints": len(r.relay_endpoints),
        "direct_packets": sum(r.direct.values()),
        "direct": violation_list(r.direct),
        "violations": violation_list(r.violations),
        "phases": [
            {
                "phase": n,
                "outbound_packets": s.outbound,
                "packets_to_relays": s.to_relays,
                "udp_packets": s.udp,
                "direct_packets": s.direct,
                "violations": violation_list(s.violations),
            }
            for n, s in r.phases.items()
        ],
    }
    if a.json:
        print(json.dumps(summary, ensure_ascii=False, indent=2))
    else:
        print(
            f"합의 문서 {allow.consensus_files}개(릴레이 {allow.relays}), 내장 주소 {allow.builtin}개"
        )
        print(
            f"나간 패킷 {r.outbound}개 중 릴레이로 {r.to_relays}개(릴레이 {len(r.relay_endpoints)}곳), UDP {r.udp}개, "
            f"내 기기 직접 연결 {summary['direct_packets']}개"
        )
        for ph in summary["phases"]:
            bad = sum(v["packets"] for v in ph["violations"])
            print(
                f"  {ph['phase']}: 나감 {ph['outbound_packets']}, 릴레이 {ph['packets_to_relays']}, "
                f"UDP {ph['udp_packets']}, 직접 연결 {ph['direct_packets']}, 누출 {bad}"
            )
        for v in summary["violations"]:
            print(f"누출: {v['proto']} → {v['dst']}:{v['port']} ({v['packets']}개)")
    if r.violations:
        print("실패: Tor 릴레이가 아닌 곳으로 나간 패킷이 있습니다", file=sys.stderr)
        return 1
    if r.to_relays < a.min_relay_packets:
        print("실패: 릴레이로 나간 패킷이 너무 적어 캡처가 잘못된 것 같습니다", file=sys.stderr)
        return 1
    if direct_from is not None and not r.direct:
        print("실패: 내 기기 단계를 지났는데 직접 연결이 없어 그 길을 시험하지 못했습니다", file=sys.stderr)
        return 1
    # 표준 출력은 `--json`의 결과 파일이 되므로 판정 문구는 표준 오류로.
    print("통과: 나간 패킷은 모두 Tor 릴레이로 갔습니다(내 기기 직접 연결은 허용 목록 안)", file=sys.stderr)
    return 0


# ───────────── 자체 시험 ─────────────


def _eth_ipv4(src: str, dst: str, proto: int, sport: int, dport: int, payload: bytes = b"\x00" * 12) -> bytes:
    l4 = struct.pack("!HHI", sport, dport, 0) + payload
    ip = bytearray(20)
    ip[0] = 0x45
    struct.pack_into("!H", ip, 2, 20 + len(l4))
    ip[8] = 64
    ip[9] = proto
    ip[12:16] = ipaddress.IPv4Address(src).packed
    ip[16:20] = ipaddress.IPv4Address(dst).packed
    return b"\x00" * 12 + struct.pack("!H", ETH_IPV4) + bytes(ip) + l4


def _eth_ipv6(src: str, dst: str, proto: int, sport: int, dport: int) -> bytes:
    l4 = struct.pack("!HH", sport, dport) + b"\x00" * 16
    ip = bytearray(40)
    ip[0] = 0x60
    struct.pack_into("!H", ip, 4, len(l4))
    ip[6] = proto
    ip[7] = 64
    ip[8:24] = ipaddress.IPv6Address(src).packed
    ip[24:40] = ipaddress.IPv6Address(dst).packed
    return b"\x00" * 12 + struct.pack("!H", ETH_IPV6) + bytes(ip) + l4


def _write_pcap(path: str, frames: list[bytes], times: list[float] | None = None, nano: bool = False) -> None:
    times = times or [0.0] * len(frames)
    unit = 1_000_000_000 if nano else 1_000_000
    with open(path, "wb") as f:
        magic = 0xA1B23C4D if nano else 0xA1B2C3D4
        f.write(struct.pack("<IHHiIII", magic, 2, 4, 0, 0, 65535, LINKTYPE_ETHERNET))
        for t, fr in zip(times, frames):
            sec = int(t)
            f.write(struct.pack("<IIII", sec, round((t - sec) * unit), len(fr), len(fr)))
            f.write(fr)


def cmd_selftest(_: argparse.Namespace) -> int:
    consensus = "\n".join(
        [
            "network-status-version 3 microdesc",
            "r relay1 AAAA 2026-01-01 00:00:00 198.51.100.7 9001 0",
            "a [2001:db8::7]:9001",
            "r relay2 BBBB 2026-01-01 00:00:00 203.0.113.9 443 80",
            "",
        ]
    )
    fallbacks = 'fallback("X", "Y", &["192.0.2.33:443", "[2001:db8::33]:443"]),\n'
    local = "10.231.0.2"
    with tempfile.TemporaryDirectory() as tmp:
        cdir = os.path.join(tmp, "dir_blobs")
        os.makedirs(cdir)
        with open(os.path.join(cdir, "con_microdesc_sha3-256-00"), "w") as f:
            f.write(consensus)
        with open(os.path.join(cdir, "mds_sha3-256-11"), "w") as f:
            f.write("onion-key\nr not a consensus 1.2.3.4 1 1\n")
        dc = os.path.join(tmp, "dircommon")
        os.makedirs(os.path.join(dc, "data"))
        with open(os.path.join(dc, "data", "fallback_dirs.rs"), "w") as f:
            f.write(fallbacks)

        allow = Allowlist()
        load_consensus_dirs([cdir], allow)
        load_builtin(dc, allow)
        assert allow.consensus_files == 1, allow
        assert allow.relays == 2, allow
        for ok in [("198.51.100.7", 9001), ("2001:db8::7", 9001), ("203.0.113.9", 80), ("192.0.2.33", 443)]:
            assert allow.allows(*ok), ok
        assert not allow.allows("1.2.3.4", 1), "합의 문서가 아닌 파일을 읽음"

        good = [
            _eth_ipv4(local, "198.51.100.7", PROTO_TCP, 40000, 9001),
            _eth_ipv4("198.51.100.7", local, PROTO_TCP, 9001, 40000),  # 들어온 패킷은 보지 않음
            _eth_ipv4(local, "192.0.2.33", PROTO_TCP, 40001, 443),
        ]
        bad = [
            _eth_ipv4(local, "198.51.100.7", PROTO_TCP, 40002, 22),  # 릴레이 IP지만 다른 포트
            _eth_ipv4(local, "10.231.0.1", PROTO_UDP, 5353, 53),  # DNS
            _eth_ipv4(local, "224.0.0.251", PROTO_UDP, 5353, 5353),  # mDNS
            _eth_ipv4(local, "93.184.216.34", PROTO_TCP, 40003, 443),  # 직접 연결
            _eth_ipv4(local, "198.51.100.7", PROTO_UDP, 40004, 9001),  # QUIC 등 UDP
            _eth_ipv6("2001:db8::99", "2001:db8::1", PROTO_TCP, 40005, 443),
        ]
        pcap = os.path.join(tmp, "cap.pcap")
        _write_pcap(pcap, good)
        r = judge(packets(pcap), {local}, allow)
        assert r.ok(2) and r.to_relays == 2 and r.outbound == 2, r
        _write_pcap(pcap, good + bad)
        r = judge(packets(pcap), {local, "2001:db8::99"}, allow)
        assert len(r.violations) == len(bad), r.violations
        assert not r.ok(1)
        # 릴레이로 간 것이 없으면(캡처 실패) 통과시키지 않음.
        _write_pcap(pcap, [])
        assert not judge(packets(pcap), {local}, allow).ok(1)

        # 단계 나누기: 캡처 시각으로 단계에 넣고, UDP는 단계별로도 셈(나노초 캡처 포함).
        phases_path = os.path.join(tmp, "phases.tsv")
        with open(phases_path, "w", encoding="utf-8") as f:
            f.write("2000.25\tvoice\n\nnot a line\n1000.5\tcrew\n")
        phases = load_phases(phases_path)
        assert [n for _, n in phases] == ["crew", "voice"], phases
        for nano in (False, True):
            _write_pcap(
                pcap,
                [good[0], good[2], bad[4], good[0], bad[1]],
                [999.0, 1000.5, 1500.0, 2000.25, 2100.000001],
                nano=nano,
            )
            pk = packets(pcap)
            assert abs(pk[-1].ts - 2100.000001) < 1e-6, pk[-1].ts
            r = judge(pk, {local}, allow, phases)
            assert r.udp == 2 and r.outbound == 5, r
            ph = r.phases
            assert list(ph) == [BEFORE_PHASES, "crew", "voice"], list(ph)
            assert ph[BEFORE_PHASES].outbound == 1 and ph[BEFORE_PHASES].to_relays == 1, ph
            assert ph["crew"].outbound == 2 and ph["crew"].udp == 1, ph["crew"]
            assert ph["crew"].violations == Counter({("udp", "198.51.100.7", 9001): 1}), ph["crew"]
            assert ph["voice"].udp == 1 and ph["voice"].to_relays == 1, ph["voice"]
            assert ph["voice"].violations == Counter({("udp", "10.231.0.1", 53): 1}), ph["voice"]
        # 단계가 없으면 단계 통계도 없음.
        assert judge(pk, {local}, allow).phases == {}

        # 내 기기 단계: 그 뒤의 UDP만, 목록의 목적지만 넘김. 앞선 같은 패킷과 목록 밖 UDP·TCP는 그대로 누출.
        stun = _eth_ipv4(local, "192.0.2.77", PROTO_UDP, 40006, 3478)
        other = _eth_ipv4(local, "192.0.2.78", PROTO_UDP, 40006, 3478)
        stun_tcp = _eth_ipv4(local, "192.0.2.77", PROTO_TCP, 40007, 3478)
        _write_pcap(pcap, [stun, stun, other, stun_tcp], [999.0, 2000.25, 2000.5, 2001.0])
        direct_path = os.path.join(tmp, "direct-allow.txt")
        with open(direct_path, "w", encoding="utf-8") as f:
            f.write("192.0.2.77:3478\n\n")
        r = judge(packets(pcap), {local}, allow, phases, 2000.25, load_direct_allow(direct_path))
        assert r.direct == Counter({("udp", "192.0.2.77", 3478): 1}), r.direct
        assert r.phases["voice"].direct == 1, r.phases
        assert sum(r.violations.values()) == 3 and r.phases[BEFORE_PHASES].violations, r.violations
        # 목록을 줘도 단계를 지나지 않았으면 넘기지 않음.
        assert not judge(packets(pcap), {local}, allow, phases, None, {("192.0.2.77", 3478)}).direct

        # 내 공인 IP: 허용한 STUN 서버가 그 단계 뒤에 답한 것만, UDP만 넘기고 결과에 IP를 적지 않음.
        xored = bytes(a ^ b for a, b in zip(ipaddress.IPv4Address("198.18.0.50").packed, STUN_COOKIE))
        reply = (
            STUN_BINDING_SUCCESS
            + struct.pack("!H", 20)
            + STUN_COOKIE
            + b"\x00" * 12
            + struct.pack("!HH", 0x8022, 5)  # 앞에 다른 속성이 있어도(길이 맞춤 포함) 찾음
            + b"abcde\x00\x00\x00"
            + struct.pack("!HHBBH", STUN_XOR_MAPPED, 8, 0, 1, 0)
            + xored
        )
        assert stun_mapped_ip(reply) == "198.18.0.50", stun_mapped_ip(reply)
        assert stun_mapped_ip(reply[:30]) is None and stun_mapped_ip(b"\x00" * 40) is None
        answer = _eth_ipv4("192.0.2.77", local, PROTO_UDP, 3478, 40006, reply)
        forged = _eth_ipv4("192.0.2.78", local, PROTO_UDP, 3478, 40006, reply)
        to_me = _eth_ipv4(local, "198.18.0.50", PROTO_UDP, 40006, 5555)
        to_me_tcp = _eth_ipv4(local, "198.18.0.50", PROTO_TCP, 40006, 5555)
        allowed = {("192.0.2.77", 3478)}
        _write_pcap(pcap, [to_me, answer, to_me, to_me_tcp], [2000.0, 2000.3, 2000.4, 2000.5])
        r = judge(packets(pcap), {local}, allow, phases, 2000.25, allowed)
        assert r.direct == Counter({("udp", MY_PUBLIC, None): 1}), r.direct
        assert r.violations == Counter({("udp", "198.18.0.50", 5555): 1, ("tcp", "198.18.0.50", 5555): 1}), r.violations
        # 허용하지 않은 곳이 보낸 답은 믿지 않음.
        _write_pcap(pcap, [forged, to_me], [2000.3, 2000.4])
        assert not judge(packets(pcap), {local}, allow, phases, 2000.25, allowed).direct
    print("자체 시험 통과")
    return 0


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="cmd", required=True)
    an = sub.add_parser("analyze", help="캡처를 판정합니다")
    an.add_argument("--pcap", required=True)
    an.add_argument("--local", action="append", required=True, help="시험 노드의 주소(여러 번)")
    an.add_argument("--consensus-dir", action="append", default=[], help="Arti의 dir_blobs 경로(여러 번)")
    an.add_argument("--dircommon", help="tor-dircommon 소스 경로(폴백·권한 주소)")
    an.add_argument("--min-relay-packets", type=int, default=10)
    an.add_argument("--phases", help="단계 표시 파일(`유닉스 시각<TAB>이름` 줄)")
    an.add_argument("--direct-phase", help="이 단계부터 --direct-allow의 목적지를 직접 연결로 셉니다")
    an.add_argument("--direct-allow", help="직접 연결로 허용할 `IP:포트` 줄 파일")
    an.add_argument("--json", action="store_true")
    an.set_defaults(func=cmd_analyze)
    st = sub.add_parser("selftest", help="분석기 자체 시험")
    st.set_defaults(func=cmd_selftest)
    a = ap.parse_args()
    return a.func(a)


if __name__ == "__main__":
    sys.exit(main())
