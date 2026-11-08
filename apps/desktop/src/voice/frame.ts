// 음성·화면 프레임 종단간 암호화(WebRTC Encoded Transform용 순수 함수).
//
// 프레임 배치: [평문 접두(코덱 헤더) | AES-GCM 암호문+태그(16) | E2EE 헤더(14)]
//   E2EE 헤더 = IV(12) | 세대(1) | 버전(1)
// 헤더를 프레임 끝에 두는 이유: VP8 등은 수신 쪽 패킷 해체기가 프레임 앞 몇 바이트(키 프레임 여부)를 읽으므로
// 앞부분을 바꾸면 영상이 깨집니다. 같은 이유로 코덱 헤더 몇 바이트는 암호화하지 않고 AAD로 묶어 변조를 막습니다.
//
// 키: 통화의 관리 기기가 참가자가 바뀔 때마다 새로 뽑아 참가자 상자 키로 봉인해 나눈 32바이트 미디어 키(데몬
// `voice.rs`의 `MediaKey`)를 HKDF로 한 번 더 파생해 AES-256-GCM 키로 씁니다. 세대는 헤더에 1바이트(세대 mod 256)로 실어 수신 쪽이 맞는 키를 고릅니다.
//
// 보낸 기기: 한 세대 키를 참가자 모두가 함께 쓰므로, 키 없이 프레임을 나르는 SFU 호스트가 A의 프레임을 B의 트랙으로
// 바꿔 붙여도 키만으로는 알 수 없습니다. 그래서 보낸 기기 ID를 AAD에 넣고(버전 2), 받는 쪽은 그 트랙이 왔다고
// 알려진 기기(메시: 상대 기기, SFU: 호스트가 준 트랙 표의 출처)로 검사합니다. 키를 가진 참가자가 호스트면 이
// 검사로도 사칭을 막지 못합니다(같은 키로 다른 기기 이름을 넣어 암호화할 수 있음).

export const FRAME_VERSION = 2;
export const IV_LEN = 12;
export const TAG_LEN = 16;
export const HEADER_LEN = IV_LEN + 2;

/** 남겨 둘 이전 세대 키 수(참가자 변경 직후 늦게 도착한 프레임용). */
export const KEEP_GENERATIONS = 3;

export type MediaKind = 'audio' | 'video';
export type VideoFrameType = 'key' | 'delta' | 'empty' | undefined;

export interface FrameHeader {
  version: number;
  /** 세대 mod 256 */
  generation: number;
  iv: Uint8Array;
}

/** 평문으로 둘 코덱 헤더 길이: Opus TOC 1바이트, VP8 키 프레임 10바이트·나머지 3바이트(WebRTC 예제와 같은 값). */
export function clearPrefixLength(kind: MediaKind, type: VideoFrameType): number {
  if (kind === 'audio') return 1;
  if (type === 'key') return 10;
  if (type === 'delta') return 3;
  return 1;
}

export function encodeHeader(h: FrameHeader): Uint8Array {
  if (h.iv.length !== IV_LEN) throw new Error('IV 길이가 잘못됐습니다'); // i18n-ignore: 프레임 암호 내부 불변식 오류(화면에 안 뜸)
  const out = new Uint8Array(HEADER_LEN);
  out.set(h.iv, 0);
  out[IV_LEN] = h.generation & 0xff;
  out[IV_LEN + 1] = h.version & 0xff;
  return out;
}

/** 프레임 끝의 E2EE 헤더를 읽습니다. 너무 짧거나 버전이 다르면 null. */
export function decodeHeader(frame: Uint8Array): FrameHeader | null {
  if (frame.length < HEADER_LEN + TAG_LEN) return null;
  const off = frame.length - HEADER_LEN;
  const version = frame[off + IV_LEN + 1];
  if (version !== FRAME_VERSION) return null;
  return { version, generation: frame[off + IV_LEN], iv: frame.slice(off, off + IV_LEN) };
}

const enc = new TextEncoder();

/** 보낸 기기 ID를 AAD에 넣을 바이트로(데몬이 주는 문자열 그대로, UTF-8). */
export function senderId(device: string): Uint8Array {
  return enc.encode(device);
}

/** AAD = 평문 접두 || 세대 || 버전 || 보낸 기기 ID. */
function aad(prefix: Uint8Array, generation: number, version: number, sender: Uint8Array): Uint8Array {
  const out = new Uint8Array(prefix.length + 2 + sender.length);
  out.set(prefix, 0);
  out[prefix.length] = generation & 0xff;
  out[prefix.length + 1] = version & 0xff;
  out.set(sender, prefix.length + 2);
  return out;
}

export const IV_SALT_LEN = 8;
const IV_COUNTER_MAX = 0xffff_ffff;

/**
 * 송신 쪽 IV: 무작위 8바이트 접두 + 4바이트 증가 카운터.
 * 한 세대의 키를 모든 참가자가 함께 쓰므로 GCM nonce가 겹치지 않으려면 송신자(워커)끼리 접두가 달라야 합니다.
 * 4바이트 접두는 참가자 n명일 때 n²/2³³ 확률로 겹쳐 첫 프레임부터 nonce가 재사용되므로 8바이트로 늘렸고,
 * 카운터가 한 바퀴 돌면 접두를 새로 뽑아 같은 IV를 다시 쓰지 않습니다.
 */
export class IvSequence {
  private salt: Uint8Array;
  private counter = 0;
  private readonly fixedSalt: boolean;

  constructor(salt?: Uint8Array) {
    if (salt && salt.length !== IV_SALT_LEN) throw new Error('IV 접두는 8바이트여야 합니다'); // i18n-ignore: 프레임 암호 내부 불변식 오류(화면에 안 뜸)
    this.fixedSalt = !!salt;
    this.salt = salt ? salt.slice() : crypto.getRandomValues(new Uint8Array(IV_SALT_LEN));
  }

  next(): Uint8Array {
    if (this.counter > IV_COUNTER_MAX) {
      // 고정 접두(테스트용)로는 겹치지 않는 IV를 더 만들 수 없습니다.
      if (this.fixedSalt) throw new Error('IV 카운터가 다 찼습니다'); // i18n-ignore: 프레임 암호 내부 불변식 오류(화면에 안 뜸)
      this.salt = crypto.getRandomValues(new Uint8Array(IV_SALT_LEN));
      this.counter = 0;
    }
    const iv = new Uint8Array(IV_LEN);
    iv.set(this.salt, 0);
    new DataView(iv.buffer).setUint32(IV_SALT_LEN, this.counter);
    this.counter++;
    return iv;
  }

  /** 테스트용: 카운터 위치를 옮깁니다. */
  seek(counter: number) {
    if (!Number.isInteger(counter) || counter < 0 || counter > IV_COUNTER_MAX + 1) throw new Error('카운터 범위 밖'); // i18n-ignore: 프레임 암호 내부 불변식 오류(화면에 안 뜸)
    this.counter = counter;
  }
}

const HKDF_SALT = enc.encode('yangpa/v1/frame-salt');
const HKDF_INFO = enc.encode('yangpa/v1/frame-aes-gcm');

/** 32바이트 미디어 키 → AES-256-GCM 키(내보낼 수 없음). 호출한 쪽은 원본 바이트를 지우세요. */
export async function deriveFrameKey(raw: Uint8Array): Promise<CryptoKey> {
  if (raw.length !== 32) throw new Error('미디어 키는 32바이트여야 합니다'); // i18n-ignore: 프레임 암호 내부 불변식 오류(화면에 안 뜸)
  const base = await crypto.subtle.importKey('raw', raw as BufferSource, 'HKDF', false, ['deriveKey']);
  return crypto.subtle.deriveKey(
    { name: 'HKDF', hash: 'SHA-256', salt: HKDF_SALT, info: HKDF_INFO },
    base,
    { name: 'AES-GCM', length: 256 },
    false,
    ['encrypt', 'decrypt'],
  );
}

/**
 * 세대별 키 보관(최근에 받은 몇 세대만). 마지막에 받은 키가 보낼 때 쓰는 키입니다: 데몬이 지금 키로 알려 준 것이 세대
 * 번호가 더 작거나 같아도(관리 기기가 바뀌며 이전 세대를 못 듣고 새로 시작) 그것을 따라야 상대와 같은 키로 보냅니다.
 * 같은 세대 번호의 다른 키는 앞의 것을 갈아 끼웁니다.
 */
export class KeyRing {
  private readonly keys = new Map<number, { generation: number; key: CryptoKey; order: number }>();
  private currentGen: number | null = null;
  private order = 0;

  async setKey(generation: number, raw: Uint8Array): Promise<void> {
    const key = await deriveFrameKey(raw);
    this.keys.set(generation & 0xff, { generation, key, order: ++this.order });
    this.currentGen = generation;
    // 오래된 키 정리(받은 순서로, 지금 키는 남김)
    const sorted = [...this.keys.values()].sort((a, b) => b.order - a.order);
    for (const old of sorted.slice(KEEP_GENERATIONS)) this.keys.delete(old.generation & 0xff);
  }

  current(): { generation: number; key: CryptoKey } | null {
    if (this.currentGen === null) return null;
    return this.keys.get(this.currentGen & 0xff) ?? null;
  }

  lookup(gen8: number): CryptoKey | null {
    return this.keys.get(gen8 & 0xff)?.key ?? null;
  }

  clear() {
    this.keys.clear();
    this.currentGen = null;
  }

  get size() {
    return this.keys.size;
  }
}

/** 프레임 하나를 암호화합니다. `sender`는 내 기기 ID(`senderId`). */
export async function encryptFrame(
  data: Uint8Array,
  clearLen: number,
  key: CryptoKey,
  generation: number,
  iv: Uint8Array,
  sender: Uint8Array,
): Promise<Uint8Array> {
  const n = Math.min(clearLen, data.length);
  const prefix = data.subarray(0, n);
  const body = data.subarray(n);
  const ct = new Uint8Array(
    await crypto.subtle.encrypt(
      { name: 'AES-GCM', iv: iv as BufferSource, additionalData: aad(prefix, generation, FRAME_VERSION, sender) as BufferSource, tagLength: TAG_LEN * 8 },
      key,
      body as BufferSource,
    ),
  );
  const out = new Uint8Array(n + ct.length + HEADER_LEN);
  out.set(prefix, 0);
  out.set(ct, n);
  out.set(encodeHeader({ version: FRAME_VERSION, generation, iv }), n + ct.length);
  return out;
}

/**
 * 프레임 하나를 복호화합니다. 평문 접두 길이는 송신 쪽과 같은 규칙(clearPrefixLength)으로 정합니다.
 * `sender`는 이 트랙을 보냈다고 알려진 기기 ID(`senderId`). 헤더가 없거나 키가 없거나 인증에 실패하면(다른 기기가
 * 보낸 프레임 포함) null(프레임을 버림).
 */
export async function decryptFrame(
  data: Uint8Array,
  clearLen: number,
  ring: KeyRing,
  sender: Uint8Array,
): Promise<Uint8Array | null> {
  const h = decodeHeader(data);
  if (!h) return null;
  const key = ring.lookup(h.generation);
  if (!key) return null;
  const end = data.length - HEADER_LEN;
  const n = Math.min(clearLen, Math.max(0, end - TAG_LEN));
  const prefix = data.subarray(0, n);
  const ct = data.subarray(n, end);
  if (ct.length < TAG_LEN) return null;
  try {
    const pt = new Uint8Array(
      await crypto.subtle.decrypt(
        { name: 'AES-GCM', iv: h.iv as BufferSource, additionalData: aad(prefix, h.generation, h.version, sender) as BufferSource, tagLength: TAG_LEN * 8 },
        key,
        ct as BufferSource,
      ),
    );
    const out = new Uint8Array(n + pt.length);
    out.set(prefix, 0);
    out.set(pt, n);
    return out;
  } catch {
    return null;
  }
}
