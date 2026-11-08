// 보내기 전 첨부 목록(입력창·포럼 새 글 공용). 동영상·음성을 붙이면 웹뷰에서 썸네일·크기·길이를 미리 읽어(lib/media.ts)
// AttachmentInput에 같이 싣습니다. 미리보기는 한 번에 하나씩 열고(큰 파일 여러 개를 한꺼번에 풀지 않게), 못 읽으면 힌트 없이 보냅니다.
// 휘발성 채널에서는 미리보기를 열지 않습니다(웹뷰 미디어 경로를 거치지 않게).

import { useCallback, useEffect, useRef, useState } from 'react';
import type { ApiClient, PickedFile } from '../api/client';
import type { AttachmentInput } from '../api/types';
import { t } from '../i18n';
import { formatBytes } from '../lib/format';
import { captureMediaHints, mediaKindOf, type MediaHints } from '../lib/media';
import { pastedName, uploadPaste } from '../lib/paste';
import { useStore } from '../store/store';

export const MAX_FILES = 10;

/** 경로별 힌트. 읽는 중이면 'pending', 못 읽었으면 null. */
export type HintMap = Record<string, MediaHints | null | 'pending'>;

export interface PendingFiles {
  files: PickedFile[];
  hints: HintMap;
  add(picked: PickedFile[]): void;
  remove(index: number): void;
  clear(): void;
  /** 클립보드에서 붙여넣은 파일을 데몬 RAM에 올려 넣습니다. 실패하면 오류 문구(일부만 실패해도). */
  paste(list: File[]): Promise<string | null>;
  /** 올리는 중인 붙여넣기 수. */
  pasting: number;
  /** 보낼 첨부. 아직 읽는 중인 힌트는 기다립니다(각각 시간 제한이 있음). `hinted`: 힌트를 하나라도 실었는지. */
  collect(): Promise<{ inputs: AttachmentInput[]; hinted: boolean }>;
  /** 보내지 못했을 때: 힌트(썸네일·길이)는 있으면 좋은 정보일 뿐이라 다음에는 힌트 없이 보냅니다(같은 값으로 계속 실패하지 않게). */
  dropHints(): void;
}

/** 채널 첨부 한도를 넘는 붙여넣기 오류 문구(데몬도 보낼 때 거절함). */
export function overLimitText(limit: number): string {
  return limit === 0 ? NO_ATTACH_TEXT : t('이 채널의 첨부는 하나에 {size}까지입니다', { size: formatBytes(limit) });
}

/** 첨부를 금지한 채널(한도 0). */
export const NO_ATTACH_TEXT = t('이 채널은 첨부를 받지 않습니다');

/** `limit`: 채널 첨부 한도(바이트, 없으면 무제한). 붙여넣기는 올리기 전에 거릅니다(쓸모없이 데몬 RAM에 올리지 않게). */
export function usePendingFiles(client: ApiClient | null, volatile: boolean, max = MAX_FILES, limit?: number): PendingFiles {
  const [files, setFiles] = useState<PickedFile[]>([]);
  const [hints, setHints] = useState<HintMap>({});
  const [pasting, setPasting] = useState(0);
  const jobs = useRef(new Map<string, Promise<MediaHints | null>>());
  const queue = useRef<Promise<unknown>>(Promise.resolve());
  const filesRef = useRef(files);
  filesRef.current = files;
  const clientRef = useRef(client);
  clientRef.current = client;

  // 붙여넣기는 데몬 RAM에 있으므로 빼거나 비우면 바로 지웁니다(휘발성 채널 첨부가 남아 있지 않게).
  const discard = useCallback((list: PickedFile[]) => {
    for (const f of list) if (f.paste) clientRef.current?.call('blob.paste_discard', { paste: f.paste }).catch(() => {});
  }, []);
  useEffect(() => () => discard(filesRef.current), [discard]);

  const add = (picked: PickedFile[]) => {
    const room = Math.max(0, max - filesRef.current.length);
    discard(picked.slice(room));
    picked = picked.slice(0, room);
    filesRef.current = [...filesRef.current, ...picked];
    setFiles((f) => [...f, ...picked].slice(0, max));
    if (!client || volatile) return;
    for (const f of picked) {
      if (f.paste || !mediaKindOf(f.name) || jobs.current.has(f.path)) continue;
      const job = queue.current.then(() => captureMediaHints(client.transport, f.path, f.name)).catch(() => null);
      queue.current = job;
      jobs.current.set(f.path, job);
      setHints((h) => ({ ...h, [f.path]: 'pending' }));
      void job.then((r) => {
        if (jobs.current.get(f.path) === job) setHints((h) => ({ ...h, [f.path]: r }));
      });
    }
  };

  const clear = useCallback(() => {
    discard(filesRef.current);
    filesRef.current = [];
    setFiles([]);
    setHints({});
    jobs.current.clear();
  }, [discard]);

  const paste = async (list: File[]): Promise<string | null> => {
    if (!client) return null;
    const take = list.slice(0, Math.max(0, max - filesRef.current.length));
    if (take.length === 0) return list.length ? t('첨부는 {n}개까지입니다', { n: max }) : null;
    setPasting((n) => n + take.length);
    let error: string | null = take.length < list.length ? t('첨부는 {n}개까지입니다', { n: max }) : null;
    const now = new Date();
    for (const f of take) {
      if (limit !== undefined && f.size > limit) {
        error = overLimitText(limit);
        setPasting((n) => n - 1);
        continue;
      }
      try {
        add([await uploadPaste(client, f, pastedName(f, now))]);
      } catch (e) {
        error = e instanceof Error ? e.message : String(e);
      } finally {
        setPasting((n) => n - 1);
      }
    }
    return error;
  };

  return {
    files,
    hints,
    add,
    paste,
    pasting,
    remove: (index) => {
      const gone = filesRef.current[index];
      if (gone) discard([gone]);
      filesRef.current = filesRef.current.filter((_, j) => j !== index);
      setFiles((f) => f.filter((_, j) => j !== index));
    },
    clear,
    async collect() {
      const got = await Promise.all(files.map((f) => jobs.current.get(f.path) ?? Promise.resolve(null)));
      // 설정 → 개인정보에서 끈 때만 메타데이터를 그대로 보냅니다(데몬 기본은 지움).
      const keep = useStore.getState().prefs.stripMetadata ? {} : { keep_metadata: true };
      const inputs = files.map((f, i) =>
        f.paste ? { path: '', paste: f.paste, name: f.name, ...keep } : { path: f.path, name: f.name, ...(got[i] ?? {}), ...keep },
      );
      return { inputs, hinted: got.some((h) => h) };
    },
    dropHints() {
      const none = Promise.resolve(null);
      for (const f of files) if (jobs.current.has(f.path)) jobs.current.set(f.path, none);
      setHints((h) => Object.fromEntries(Object.keys(h).map((k) => [k, null])));
    },
  };
}
