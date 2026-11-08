// SDP 손질: Opus DTX(말하지 않을 때 전송 중단)와 인밴드 FEC를 켭니다.

/** Opus 페이로드 번호들. */
export function opusPayloadTypes(sdp: string): string[] {
  const out: string[] = [];
  for (const m of sdp.matchAll(/^a=rtpmap:(\d+) opus\/48000(?:\/\d+)?\r?$/gim)) out.push(m[1]);
  return out;
}

/** fmtp 매개변수 문자열에 값을 넣거나 바꿉니다(`a=1;b=2`). */
export function setFmtpParams(params: string, set: Record<string, string>): string {
  const entries: Array<[string, string]> = [];
  for (const part of params.split(';')) {
    const p = part.trim();
    if (!p) continue;
    const eq = p.indexOf('=');
    entries.push(eq < 0 ? [p, ''] : [p.slice(0, eq).trim(), p.slice(eq + 1).trim()]);
  }
  for (const [k, v] of Object.entries(set)) {
    const i = entries.findIndex(([ek]) => ek.toLowerCase() === k.toLowerCase());
    if (i >= 0) entries[i] = [entries[i][0], v];
    else entries.push([k, v]);
  }
  return entries.map(([k, v]) => (v === '' ? k : `${k}=${v}`)).join(';');
}

/** 모든 Opus 페이로드에 usedtx=1, useinbandfec=1을 넣습니다. fmtp 줄이 없으면 rtpmap 뒤에 만듭니다. */
export function enableOpusDtx(sdp: string): string {
  const pts = opusPayloadTypes(sdp);
  if (pts.length === 0) return sdp;
  const eol = sdp.includes('\r\n') ? '\r\n' : '\n';
  const lines = sdp.split(/\r?\n/);
  const want = { usedtx: '1', useinbandfec: '1' };
  const seen = new Set<string>();
  for (let i = 0; i < lines.length; i++) {
    const m = /^a=fmtp:(\d+) (.*)$/.exec(lines[i]);
    if (m && pts.includes(m[1])) {
      lines[i] = `a=fmtp:${m[1]} ${setFmtpParams(m[2], want)}`;
      seen.add(m[1]);
    }
  }
  for (const pt of pts) {
    if (seen.has(pt)) continue;
    const idx = lines.findIndex((l) => new RegExp(`^a=rtpmap:${pt} opus/48000`, 'i').test(l));
    if (idx >= 0) lines.splice(idx + 1, 0, `a=fmtp:${pt} ${setFmtpParams('', want)}`);
  }
  return lines.join(eol);
}

/**
 * 영상 코덱을 VP8(+재전송·오류 정정)만 남깁니다. 평문 접두 규칙(frame.ts)이 VP8 기준이고,
 * H.264처럼 NAL 헤더를 읽는 코덱은 암호화된 본문 때문에 패킷화가 깨질 수 있기 때문입니다.
 * VP8은 WebRTC 필수 코덱이라 모든 참가자가 지원합니다. VP8이 없으면 원래 목록을 그대로 둡니다.
 */
export function onlyVp8(codecs: RTCRtpCodec[]): RTCRtpCodec[] {
  const isAux = (m: string) => m === 'video/rtx' || m === 'video/red' || m === 'video/ulpfec' || m === 'video/flexfec-03';
  const vp8 = codecs.filter((c) => c.mimeType.toLowerCase() === 'video/vp8');
  if (vp8.length === 0) return codecs;
  return [...vp8, ...codecs.filter((c) => isAux(c.mimeType.toLowerCase()))];
}
