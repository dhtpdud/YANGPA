// SPDX-License-Identifier: LicenseRef-YANGPA-Supporter
// 채널 콘텐츠 뷰어(ContentViewer.tsx)의 항목 만들기·설정. DOM 없이 돌아 시험에서 바로 부릅니다.
// 메시지 → 항목: 첨부(이미지·영상·오디오·파일)는 하나씩, 첨부가 없는 메시지는 글 하나. 첨부가 있는 메시지의 글은 위쪽 제목줄에 보입니다.

import { isRenderableMime } from '../api/client';
import type { AttachmentView, ChannelView, MessageView } from '../api/types';
import { isAnonymousMessage } from '../lib/anon';
import { isP2pLocked } from '../lib/channel';
import { clamp } from '../lib/misc';

/** 웹뷰가 그릴 수 없는 형식의 이미지·영상·오디오는 'file'(첨부 카드로 받기·저장만). */
export type ViewerItemType = 'text' | 'image' | 'video' | 'audio' | 'file';

export interface ViewerItem {
  key: string;
  type: ViewerItemType;
  msg: MessageView;
  att?: AttachmentView;
}

export type ViewerFit = 'auto' | 'height' | 'width';

/** 뷰어 설정(앱 설정 파일 `viewer`). 보던 위치·메시지 ID는 넣지 않습니다(휘발성 채널 내용이 디스크로 가지 않게). */
export interface ViewerPrefs {
  /** 'newest'는 최근 것부터(불러오는 방향), 'oldest'는 기록을 처음까지 다 읽은 뒤 처음부터. */
  order: 'newest' | 'oldest';
  text: boolean;
  images: boolean;
  videos: boolean;
  audio: boolean;
  files: boolean;
  /** 비어 있지 않으면 글이나 첨부 이름에 이 단어가 하나라도 있는 메시지만. */
  include: string[];
  /** 글이나 첨부 이름에 이 단어가 하나라도 있는 메시지는 뺌(포함보다 먼저). */
  exclude: string[];
  /** 이미지로 화면을 채움(끄면 화면 안에 다 들어오게 맞춤). */
  fill: boolean;
  /** 'auto'는 가로가 긴 이미지를 높이에, 세로가 긴 이미지를 폭에 맞춤. */
  fit: ViewerFit;
  /** 맞춘 쪽(높이나 폭)을 화면의 몇 %로 채울지. */
  fillPercent: number;
  /** 높이에 맞춘 이미지를 오른쪽에서 왼쪽으로 읽음(일본 만화책). */
  rtl: boolean;
  /** 아래 막대(위치·썸네일)를 늘 보이고 콘텐츠는 그 위까지만(영상 조작 막대가 가려지지 않게). */
  pinStrip: boolean;
  /** 화면보다 큰 이미지를 ↑/↓ 한 번에 스크롤하는 길이(px). */
  scrollStep: number;
  /** ←/→로 영상을 옮기는 초. */
  seek: number;
}

export const DEFAULT_VIEWER: ViewerPrefs = {
  order: 'newest',
  text: true,
  images: true,
  videos: true,
  audio: true,
  files: true,
  include: [],
  exclude: [],
  fill: true,
  fit: 'auto',
  fillPercent: 100,
  rtl: false,
  pinStrip: false,
  scrollStep: 400,
  seek: 5,
};

export const VIEWER_LIMITS = { fillPercent: [10, 100], scrollStep: [20, 5000], seek: [1, 60] } as const;

/** 단어 목록 상한(두 목록이 다 차도 한글 기준 약 7KB라 설정 파일 64KiB 안에 들게). */
const MAX_WORDS = 30;
const MAX_WORD_LEN = 40;

/** 쉼표·줄바꿈으로 나눈 단어(앞뒤 공백 없이, 대소문자 무시 중복 없이). */
export function parseWords(value: unknown): string[] {
  const list = Array.isArray(value) ? value : String(value ?? '').split(/[,\n]/);
  const seen = new Set<string>();
  const out: string[] = [];
  for (const raw of list) {
    if (typeof raw !== 'string') continue;
    const word = raw.trim().slice(0, MAX_WORD_LEN);
    const k = word.toLowerCase();
    if (!word || seen.has(k)) continue;
    seen.add(k);
    out.push(word);
    if (out.length >= MAX_WORDS) break;
  }
  return out;
}

function num(v: unknown, [lo, hi]: readonly [number, number], fallback: number): number {
  const n = Math.round(Number(v));
  return Number.isFinite(n) ? clamp(n, lo, hi) : fallback;
}

/** 설정 파일에서 읽은 값(믿지 않음)을 맞는 모양으로. */
export function sanitizeViewer(raw: unknown): ViewerPrefs {
  const x = raw && typeof raw === 'object' ? (raw as Record<string, unknown>) : {};
  const d = DEFAULT_VIEWER;
  return {
    order: x.order === 'oldest' ? 'oldest' : 'newest',
    text: x.text !== false,
    images: x.images !== false,
    videos: x.videos !== false,
    audio: x.audio !== false,
    files: x.files !== false,
    include: parseWords(x.include),
    exclude: parseWords(x.exclude),
    fill: x.fill !== false,
    fit: x.fit === 'height' || x.fit === 'width' ? x.fit : 'auto',
    fillPercent: num(x.fillPercent, VIEWER_LIMITS.fillPercent, d.fillPercent),
    rtl: x.rtl === true,
    pinStrip: x.pinStrip === true,
    scrollStep: num(x.scrollStep, VIEWER_LIMITS.scrollStep, d.scrollStep),
    seek: num(x.seek, VIEWER_LIMITS.seek, d.seek),
  };
}

function mentionsAny(m: MessageView, words: string[]): boolean {
  const hay = [m.text, ...m.attachments.map((a) => a.name)].join('\n').toLowerCase();
  return words.some((w) => hay.includes(w.toLowerCase()));
}

/** 첨부 하나의 뷰어 항목 종류(라이브러리가 고른 파일을 넘길 때도 씀). */
export function attType(a: AttachmentView): ViewerItemType {
  if (!isRenderableMime(a.mime)) return 'file';
  const kind = a.mime.split('/')[0];
  return kind === 'image' || kind === 'video' || kind === 'audio' ? kind : 'file';
}

const SHOWN: Record<ViewerItemType, 'text' | 'images' | 'videos' | 'audio' | 'files'> = { text: 'text', image: 'images', video: 'videos', audio: 'audio', file: 'files' };

/**
 * 메시지(화면 순서, 오래된 것부터) → 뷰어 항목. 지운 글과 차단한 사람의 글은 뺍니다(목록이 접어 둔 것을 펼치지 않게).
 * 최신순이면 메시지 순서만 뒤집고 한 메시지 안의 첨부는 올린 순서 그대로입니다.
 * `authors`가 있으면 그 사람이 쓴 글만(익명 글은 누가 썼는지 모르므로 뺌).
 */
export function buildViewerItems(messages: MessageView[], v: ViewerPrefs, authors: readonly string[] = []): ViewerItem[] {
  const out: ViewerItem[] = [];
  const list = v.order === 'newest' ? [...messages].reverse() : messages;
  for (const m of list) {
    if (m.deleted || m.author_blocked) continue;
    if (authors.length > 0 && (isAnonymousMessage(m) || !authors.includes(m.author))) continue;
    if (v.include.length > 0 && !mentionsAny(m, v.include)) continue;
    if (v.exclude.length > 0 && mentionsAny(m, v.exclude)) continue;
    const seen = new Set<string>();
    const items: ViewerItem[] = [];
    for (const att of m.attachments) {
      if (seen.has(att.blob)) continue;
      seen.add(att.blob);
      items.push({ key: `${m.id}:${att.blob}`, type: attType(att), msg: m, att });
    }
    if (items.length === 0 && m.text.trim()) items.push({ key: `${m.id}:text`, type: 'text', msg: m });
    for (const it of items) if (v[SHOWN[it.type]]) out.push(it);
  }
  return out;
}

/**
 * 파형 막대 높이(0~1, 가장 큰 막대가 1). 막대마다 그 구간의 모든 채널에서 가장 큰 진폭을 씁니다. 조용한 녹음도 모양이
 * 보이게 가장 큰 값으로 나눕니다.
 */
export function wavePeaks(channels: Float32Array[], bars: number): number[] {
  const len = channels[0]?.length ?? 0;
  const peaks = new Array<number>(bars).fill(0);
  if (len === 0) return peaks;
  for (let b = 0; b < bars; b++) {
    const from = Math.floor((b * len) / bars);
    const to = Math.max(from + 1, Math.floor(((b + 1) * len) / bars));
    for (const ch of channels) for (let i = from; i < to && i < len; i++) peaks[b] = Math.max(peaks[b], Math.abs(ch[i]));
  }
  const top = Math.max(...peaks);
  return top > 0 ? peaks.map((p) => p / top) : peaks;
}

/** 이미지를 맞출 쪽. ''이면 화면 안에 다 들어오게(꺼짐·크기 모름). */
export function imageFit(v: ViewerPrefs, w?: number, h?: number): '' | 'height' | 'width' {
  if (!v.fill) return '';
  if (v.fit !== 'auto') return v.fit;
  if (!w || !h) return '';
  return w >= h ? 'height' : 'width';
}

/** 채널 화면에 메시지 목록이 그려지는 경우만(Pane.tsx의 MessageList 갈래와 같음): 텍스트 채널, 잠기지 않음, 들어와 있음. */
export function viewerAvailable(c: ChannelView | undefined): boolean {
  if (!c || c.settings.kind !== 'text' || isP2pLocked(c)) return false;
  if (c.settings.security === 'volatile' && c.volatile_full) return false;
  return c.settings.join_mode !== 'join_leave' || !!c.joined;
}
