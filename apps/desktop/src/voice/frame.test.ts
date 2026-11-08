import { describe, expect, it } from 'vitest';
import {
  FRAME_VERSION,
  HEADER_LEN,
  IV_LEN,
  IV_SALT_LEN,
  IvSequence,
  KeyRing,
  TAG_LEN,
  clearPrefixLength,
  decodeHeader,
  decryptFrame,
  encodeHeader,
  encryptFrame,
  senderId,
} from './frame';

const key32 = (b: number) => new Uint8Array(32).fill(b);
const alice = senderId('device-alice');
const bob = senderId('device-bob');

describe('E2EE 헤더', () => {
  it('IV·세대·버전을 프레임 끝 14바이트에 싣는다', () => {
    const iv = Uint8Array.from({ length: IV_LEN }, (_, i) => i + 1);
    const h = encodeHeader({ version: FRAME_VERSION, generation: 300, iv });
    expect(h).toHaveLength(HEADER_LEN);
    expect(h[IV_LEN]).toBe(300 & 0xff);
    expect(h[IV_LEN + 1]).toBe(FRAME_VERSION);
    const frame = new Uint8Array(40 + HEADER_LEN);
    frame.set(h, 40);
    const d = decodeHeader(frame);
    expect(d?.generation).toBe(44);
    expect(Array.from(d!.iv)).toEqual(Array.from(iv));
  });

  it('짧거나 버전이 다른 프레임은 버린다', () => {
    expect(decodeHeader(new Uint8Array(HEADER_LEN + TAG_LEN - 1))).toBeNull();
    const f = new Uint8Array(64);
    f[63] = FRAME_VERSION + 1;
    expect(decodeHeader(f)).toBeNull();
    expect(() => encodeHeader({ version: 1, generation: 0, iv: new Uint8Array(8) })).toThrow();
  });

  it('코덱별 평문 접두 길이', () => {
    expect(clearPrefixLength('audio', undefined)).toBe(1);
    expect(clearPrefixLength('video', 'key')).toBe(10);
    expect(clearPrefixLength('video', 'delta')).toBe(3);
  });

  it('IV는 같은 송신자 안에서 겹치지 않는다', () => {
    const seq = new IvSequence(new Uint8Array([9, 9, 9, 9, 9, 9, 9, 9]));
    const a = seq.next();
    const b = seq.next();
    expect(Array.from(a.slice(0, IV_SALT_LEN))).toEqual([9, 9, 9, 9, 9, 9, 9, 9]);
    expect(Array.from(a)).not.toEqual(Array.from(b));
    expect(b[11]).toBe(1);
  });

  it('송신자마다 접두가 8바이트 무작위라 서로 겹치지 않는다', () => {
    // 같은 세대 키를 모든 참가자가 쓰므로 접두가 짧으면 첫 프레임부터 nonce가 겹칠 수 있습니다.
    expect(() => new IvSequence(new Uint8Array(4))).toThrow();
    const salts = new Set<string>();
    for (let i = 0; i < 2000; i++) salts.add(Array.from(new IvSequence().next().slice(0, IV_SALT_LEN)).join(','));
    expect(salts.size).toBe(2000);
  });

  it('카운터가 다 차면 접두를 새로 뽑고, 고정 접두는 같은 IV를 다시 만들지 않는다', () => {
    const seq = new IvSequence();
    seq.seek(0xffff_ffff);
    const last = seq.next();
    const wrapped = seq.next();
    expect(Array.from(last.slice(IV_SALT_LEN))).toEqual([0xff, 0xff, 0xff, 0xff]);
    expect(Array.from(wrapped.slice(IV_SALT_LEN))).toEqual([0, 0, 0, 0]);
    expect(Array.from(wrapped.slice(0, IV_SALT_LEN))).not.toEqual(Array.from(last.slice(0, IV_SALT_LEN)));

    const fixed = new IvSequence(new Uint8Array(IV_SALT_LEN));
    fixed.seek(0xffff_ffff);
    fixed.next();
    expect(() => fixed.next()).toThrow();
  });
});

describe('프레임 암호화', () => {
  it('왕복하고 코덱 접두는 평문으로 남는다', async () => {
    const ring = new KeyRing();
    await ring.setKey(3, key32(1));
    const cur = ring.current()!;
    const data = Uint8Array.from({ length: 200 }, (_, i) => i & 0xff);
    const ct = await encryptFrame(data, 10, cur.key, 3, new IvSequence().next(), alice);
    expect(ct.length).toBe(200 + TAG_LEN + HEADER_LEN);
    expect(Array.from(ct.slice(0, 10))).toEqual(Array.from(data.slice(0, 10)));
    expect(Array.from(ct.slice(10, 30))).not.toEqual(Array.from(data.slice(10, 30)));
    const pt = await decryptFrame(ct, 10, ring, alice);
    expect(pt && Array.from(pt)).toEqual(Array.from(data));
  });

  it('평문 접두를 바꾸면(AAD) 복호화가 실패한다', async () => {
    const ring = new KeyRing();
    await ring.setKey(1, key32(2));
    const ct = await encryptFrame(new Uint8Array(50).fill(7), 3, ring.current()!.key, 1, new IvSequence().next(), alice);
    const tampered = ct.slice();
    tampered[0] ^= 1;
    expect(await decryptFrame(tampered, 3, ring, alice)).toBeNull();
    const body = ct.slice();
    body[20] ^= 0x80;
    expect(await decryptFrame(body, 3, ring, alice)).toBeNull();
  });

  it('세대 키가 없거나 다른 키면 버린다', async () => {
    const a = new KeyRing();
    await a.setKey(5, key32(3));
    const ct = await encryptFrame(new Uint8Array(30), 1, a.current()!.key, 5, new IvSequence().next(), alice);
    const b = new KeyRing();
    expect(await decryptFrame(ct, 1, b, alice)).toBeNull();
    await b.setKey(5, key32(4));
    expect(await decryptFrame(ct, 1, b, alice)).toBeNull();
  });

  it('최근 세대만 남겨 늦게 온 프레임도 푼다', async () => {
    const ring = new KeyRing();
    await ring.setKey(1, key32(1));
    const old = await encryptFrame(new Uint8Array(20).fill(1), 1, ring.current()!.key, 1, new IvSequence().next(), alice);
    await ring.setKey(2, key32(2));
    await ring.setKey(3, key32(3));
    expect(ring.current()?.generation).toBe(3);
    expect(await decryptFrame(old, 1, ring, alice)).not.toBeNull();
    await ring.setKey(4, key32(4));
    expect(ring.size).toBe(3);
    expect(await decryptFrame(old, 1, ring, alice)).toBeNull();
    ring.clear();
    expect(ring.current()).toBeNull();
  });

  it('마지막에 받은 키로 보낸다: 세대 번호가 더 작거나 같아도(관리 기기가 바뀌어 새로 시작)', async () => {
    const ring = new KeyRing();
    await ring.setKey(5, key32(1));
    const old = await encryptFrame(new Uint8Array(20).fill(1), 1, ring.current()!.key, 5, new IvSequence().next(), alice);
    await ring.setKey(2, key32(2));
    expect(ring.current()?.generation).toBe(2);
    // 이전 세대 키도 남아 늦게 온 프레임은 풉니다.
    expect(await decryptFrame(old, 1, ring, alice)).not.toBeNull();
    // 같은 번호의 다른 키는 앞의 것을 갈아 끼웁니다.
    const before = await encryptFrame(new Uint8Array(20).fill(2), 1, ring.current()!.key, 2, new IvSequence().next(), alice);
    await ring.setKey(2, key32(3));
    expect(ring.size).toBe(2);
    expect(await decryptFrame(before, 1, ring, alice)).toBeNull();
    const after = await encryptFrame(new Uint8Array(20).fill(3), 1, ring.current()!.key, 2, new IvSequence().next(), alice);
    expect(await decryptFrame(after, 1, ring, alice)).not.toBeNull();
    // 정리는 받은 순서로: 지금 키(세대 번호가 가장 작아도)는 남습니다.
    await ring.setKey(9, key32(4));
    await ring.setKey(7, key32(5));
    await ring.setKey(1, key32(6));
    expect(ring.size).toBe(3);
    expect(ring.current()?.generation).toBe(1);
    expect(ring.lookup(5)).toBeNull();
  });

  it('보낸 기기가 다르면(AAD) 버린다: 키 없는 호스트가 트랙 출처를 바꿔 붙여도 들키게', async () => {
    const ring = new KeyRing();
    await ring.setKey(2, key32(5));
    const data = new Uint8Array(40).fill(3);
    const fromAlice = await encryptFrame(data, 1, ring.current()!.key, 2, new IvSequence().next(), alice);
    expect(await decryptFrame(fromAlice, 1, ring, bob)).toBeNull();
    expect(await decryptFrame(fromAlice, 1, ring, senderId(''))).toBeNull();
    const pt = await decryptFrame(fromAlice, 1, ring, alice);
    expect(pt && Array.from(pt)).toEqual(Array.from(data));
  });

  it('보낸 기기를 묶지 않은 옛 버전(1) 프레임은 받지 않는다', async () => {
    const ring = new KeyRing();
    await ring.setKey(1, key32(6));
    const ct = await encryptFrame(new Uint8Array(30).fill(1), 1, ring.current()!.key, 1, new IvSequence().next(), alice);
    expect(FRAME_VERSION).toBe(2);
    ct[ct.length - 1] = 1;
    expect(decodeHeader(ct)).toBeNull();
    expect(await decryptFrame(ct, 1, ring, alice)).toBeNull();
  });

  it('접두보다 짧은 프레임도 처리한다', async () => {
    const ring = new KeyRing();
    await ring.setKey(0, key32(9));
    const tiny = Uint8Array.of(0xaa, 0xbb);
    const ct = await encryptFrame(tiny, 10, ring.current()!.key, 0, new IvSequence().next(), alice);
    const pt = await decryptFrame(ct, 10, ring, alice);
    expect(pt && Array.from(pt)).toEqual([0xaa, 0xbb]);
  });
});
