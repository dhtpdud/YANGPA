// MOCK 백엔드용 가짜 원격 화면(캔버스 애니메이션). 실제 통화에서는 쓰지 않습니다.

// 샘플 데이터(api/mockData)는 배포 번들에 싣지 않으므로 여기서 언어만 보고 고릅니다(session.ts가 이 파일을 늘 불러옴).
import { currentLang } from '../i18n';

const streams = new Map<string, { stream: MediaStream; timer: ReturnType<typeof setInterval> }>();

export function createFakeScreenStream(device: string): MediaStream {
  const existing = streams.get(device);
  if (existing) return existing.stream;
  const canvas = document.createElement('canvas');
  canvas.width = 1280;
  canvas.height = 720;
  const ctx = canvas.getContext('2d');
  let frame = 0;
  const ko = currentLang() === 'ko';
  const lines = [
    'fn reconcile(local: &Range, remote: &Fingerprint) -> Plan {',
    ko ? '    // 구간 지문이 같으면 건너뜀' : '    // skip when the range fingerprints match', // i18n-ignore: 개발용 가짜 화면에 그리는 예시 글
    '    if local.fingerprint() == *remote { return Plan::Skip; }',
    '    if local.len() <= 64 { return Plan::ListIds; }',
    '    Plan::Split(local.split(16))',
    '}',
    '',
    'async fn catch_up(space: SpaceId) -> Result<()> {',
    '    let heads = peer.get_heads(space).await?;',
    '    for head in heads.unknown() {',
    '        dag.follow_parents(head, 512).await?;',
    '    }',
    '    Ok(())',
    '}',
  ];
  const draw = () => {
    if (!ctx) return;
    frame++;
    ctx.fillStyle = '#1e1e2e';
    ctx.fillRect(0, 0, canvas.width, canvas.height);
    ctx.fillStyle = '#2a2a3c';
    ctx.fillRect(0, 0, canvas.width, 44);
    ctx.fillStyle = '#89b4fa';
    ctx.font = '600 18px sans-serif';
    ctx.fillText(ko ? 'sync/reconcile.rs — 양파 메신저' : 'sync/reconcile.rs — YANGPA messenger', 20, 28); // i18n-ignore: 개발용 가짜 화면에 그리는 예시 글
    ctx.font = '20px monospace';
    lines.forEach((l, i) => {
      ctx.fillStyle = l.trim().startsWith('//') ? '#7f849c' : i % 3 === 0 ? '#cba6f7' : '#cdd6f4';
      ctx.fillText(l, 40, 100 + i * 34);
    });
    const caretLine = Math.floor(frame / 30) % lines.length;
    if (frame % 30 < 18) {
      ctx.fillStyle = '#f5e0dc';
      ctx.fillRect(40 + (lines[caretLine]?.length ?? 0) * 12, 82 + caretLine * 34, 3, 24);
    }
  };
  draw();
  const timer = setInterval(draw, 1000 / 15);
  const stream = canvas.captureStream(15);
  streams.set(device, { stream, timer });
  return stream;
}

export function stopFakeScreens() {
  for (const { stream, timer } of streams.values()) {
    clearInterval(timer);
    stream.getTracks().forEach((t) => t.stop());
  }
  streams.clear();
}
