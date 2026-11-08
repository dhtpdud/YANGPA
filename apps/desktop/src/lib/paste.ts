// 클립보드 첨부: 붙여넣은 이미지·파일은 웹뷰에서 경로 없이 바이트로만 오므로, 데몬 RAM에 조각씩 올리고(blob.paste)
// 보낼 때 경로 대신 그 ID를 넘깁니다. 휘발성 채널에서도 디스크·임시 파일을 거치지 않습니다.

import type { ApiClient, PickedFile } from '../api/client';
import { t } from '../i18n';
import { bytesToBase64 } from './misc';

/** 한 번에 올리는 조각(데몬 한도 4MiB 안). */
export const PASTE_PIECE = 3 * 1024 * 1024;
/** 붙여넣기 하나의 최대 크기. 더 큰 파일은 첨부 버튼이나 끌어 놓기로 올립니다(경로로 읽어 메모리를 덜 씀). */
export const PASTE_MAX_BYTES = 256 * 1024 * 1024;

const EXT: Record<string, string> = { 'image/png': 'png', 'image/jpeg': 'jpg', 'image/gif': 'gif', 'image/webp': 'webp', 'image/bmp': 'bmp' };

/** 붙여넣은 파일의 이름. 스크린샷처럼 이름이 없거나 'image.png'뿐이면 시각을 붙여 구분합니다. */
export function pastedName(file: { name: string; type: string }, now: Date): string {
  const name = file.name.trim();
  if (name && !/^image\.\w+$/i.test(name)) return name;
  const p = (n: number) => String(n).padStart(2, '0');
  const stamp = `${now.getFullYear()}${p(now.getMonth() + 1)}${p(now.getDate())}-${p(now.getHours())}${p(now.getMinutes())}${p(now.getSeconds())}`;
  const ext = EXT[file.type] ?? (name.split('.').pop() || 'bin');
  return t('붙여넣기-{stamp}.{ext}', { stamp, ext });
}

/**
 * 붙여넣기의 파일들. 글자도 함께 있으면(문서에서 복사해 이미지가 딸려 온 경우) 글자만 붙이도록 빈 목록을 돌려줍니다.
 */
export function clipboardFiles(data: Pick<DataTransfer, 'files' | 'getData'> | null): File[] {
  if (!data || data.getData('text/plain')) return [];
  return Array.from(data.files ?? []);
}

/** 파일 하나를 데몬 RAM에 올립니다. 다 올린 뒤 웹뷰에 남은 바이트는 0으로 덮습니다. */
export async function uploadPaste(client: ApiClient, file: Blob, name: string): Promise<PickedFile> {
  if (file.size === 0) throw new Error(t('빈 파일은 붙여넣을 수 없습니다'));
  if (file.size > PASTE_MAX_BYTES) throw new Error(t('붙여넣기는 {size}MiB까지입니다. 큰 파일은 첨부 버튼이나 끌어 놓기로 올려 주세요', { size: PASTE_MAX_BYTES / (1024 * 1024) }));
  const bytes = new Uint8Array(await file.arrayBuffer());
  try {
    let id: string | undefined;
    for (let off = 0; off < bytes.length; off += PASTE_PIECE) {
      const data_base64 = bytesToBase64(bytes.subarray(off, off + PASTE_PIECE));
      const r = await client.call('blob.paste', id ? { paste: id, data_base64 } : { size: bytes.length, data_base64 });
      id = r.paste;
    }
    return { path: `paste:${id}`, name, size: bytes.length, paste: id };
  } finally {
    bytes.fill(0);
  }
}
