// SPDX-License-Identifier: LicenseRef-YANGPA-Supporter
// 라이브러리 탭(LibraryView.tsx)의 파일 줄·폴더 나무·여러 단계 정렬과 분류·설정. DOM 없이 돌아 시험에서 바로 부릅니다.
// 줄 하나는 메시지 하나의 첨부 하나입니다(같은 파일을 두 채널에 올렸으면 채널마다 한 줄). 크루·채널·사람 이름은 부르는 쪽이
// `LibraryNames`로 넘겨, 이 파일은 상태(store)를 모릅니다.

import type { AttachmentView, ChannelId, ChannelView, LibraryItem, SpaceId, SpaceSummary } from '../api/types';
import { t } from '../i18n';
import { dayKey, formatDay } from '../lib/format';
import { plainName } from '../lib/text';

export type LibraryKey = 'uploaded' | 'received' | 'name' | 'ext' | 'kind' | 'size' | 'crew' | 'channel' | 'author' | 'origin' | 'stored' | 'sharing';

export const LIBRARY_KEYS: readonly LibraryKey[] = ['uploaded', 'received', 'name', 'ext', 'kind', 'size', 'crew', 'channel', 'author', 'origin', 'stored', 'sharing'];

export function keyLabel(k: LibraryKey): string {
  switch (k) {
    case 'uploaded':
      return t('올린 시간');
    case 'received':
      return t('받은 시간');
    case 'name':
      return t('이름');
    case 'ext':
      return t('파일 확장자');
    case 'kind':
      return t('종류');
    case 'size':
      return t('크기');
    case 'crew':
      return t('공유된 크루');
    case 'channel':
      return t('채널');
    case 'author':
      return t('공유한 사람');
    case 'origin':
      return t('보냄·받음');
    case 'stored':
      return t('이 기기에 있음');
    case 'sharing':
      return t('공유 상태');
  }
}

/** 정렬·분류 규칙 하나. 목록의 앞이 우선순위가 높습니다. */
export interface LibraryRule {
  key: LibraryKey;
  desc: boolean;
}

export type LibraryViewMode = 'details' | 'gallery';

/** 라이브러리 설정(앱 설정 파일 `library`). 고른 폴더·파일·이름은 넣지 않습니다. */
export interface LibraryPrefs {
  view: LibraryViewMode;
  sort: LibraryRule[];
  group: LibraryRule[];
  /** 오른쪽 미리 보기 칸. */
  preview: boolean;
}

export const DEFAULT_LIBRARY: LibraryPrefs = { view: 'details', sort: [{ key: 'uploaded', desc: true }], group: [], preview: true };

function sanitizeRules(raw: unknown): LibraryRule[] {
  if (!Array.isArray(raw)) return [];
  const seen = new Set<LibraryKey>();
  const out: LibraryRule[] = [];
  for (const r of raw) {
    const key = (r as { key?: unknown } | null)?.key as LibraryKey;
    if (!LIBRARY_KEYS.includes(key) || seen.has(key)) continue;
    seen.add(key);
    out.push({ key, desc: (r as { desc?: unknown }).desc === true });
  }
  return out;
}

/** 설정 파일에서 읽은 값(믿지 않음)을 맞는 모양으로. */
export function sanitizeLibrary(raw: unknown): LibraryPrefs {
  const x = raw && typeof raw === 'object' ? (raw as Record<string, unknown>) : {};
  const sort = sanitizeRules(x.sort);
  return {
    view: x.view === 'gallery' ? 'gallery' : 'details',
    sort: sort.length > 0 ? sort : DEFAULT_LIBRARY.sort,
    group: sanitizeRules(x.group),
    preview: x.preview !== false,
  };
}

export interface LibraryFile {
  key: string;
  space: SpaceId;
  channel: ChannelId;
  item: LibraryItem;
  att: AttachmentView;
}

/** 데몬 줄(메시지) → 파일 줄. 한 메시지에 같은 블롭이 두 번 실렸으면 한 줄로. */
export function libraryFiles(items: readonly LibraryItem[]): LibraryFile[] {
  const out: LibraryFile[] = [];
  for (const item of items) {
    const { space, channel } = item.message;
    if (!space || !channel) continue;
    const seen = new Set<string>();
    for (const att of item.message.attachments) {
      if (seen.has(att.blob)) continue;
      seen.add(att.blob);
      out.push({ key: `${item.message.id}:${att.blob}`, space, channel, item, att });
    }
  }
  return out;
}

export type FileKind = 'image' | 'video' | 'audio' | 'document' | 'archive' | 'other';

const DOC_EXT = new Set(['pdf', 'txt', 'md', 'doc', 'docx', 'hwp', 'hwpx', 'xls', 'xlsx', 'csv', 'ppt', 'pptx', 'odt', 'ods', 'odp', 'rtf', 'epub']);
const ARCHIVE_EXT = new Set(['zip', '7z', 'rar', 'tar', 'gz', 'tgz', 'bz2', 'xz', 'zst']);

/** 소문자 확장자(점 없이). 없으면 ''. */
export function extOf(name: string): string {
  const n = plainName(name);
  const i = n.lastIndexOf('.');
  return i > 0 && i < n.length - 1 ? n.slice(i + 1).toLowerCase() : '';
}

export function fileKind(att: Pick<AttachmentView, 'mime' | 'name'>): FileKind {
  if (att.mime.startsWith('image/')) return 'image';
  if (att.mime.startsWith('video/')) return 'video';
  if (att.mime.startsWith('audio/')) return 'audio';
  const ext = extOf(att.name);
  if (DOC_EXT.has(ext) || att.mime.startsWith('text/')) return 'document';
  if (ARCHIVE_EXT.has(ext)) return 'archive';
  return 'other';
}

const KIND_ORDER: readonly FileKind[] = ['image', 'video', 'audio', 'document', 'archive', 'other'];

export function kindLabel(k: FileKind): string {
  switch (k) {
    case 'image':
      return t('이미지');
    case 'video':
      return t('영상');
    case 'audio':
      return t('오디오');
    case 'document':
      return t('문서');
    case 'archive':
      return t('압축 파일');
    case 'other':
      return t('기타 파일');
  }
}

const SIZE_STEPS = [1 << 20, 100 << 20, 1 << 30];

function sizeBucket(n: number): number {
  const i = SIZE_STEPS.findIndex((s) => n < s);
  return i < 0 ? SIZE_STEPS.length : i;
}

function sizeBucketLabel(i: number): string {
  return [t('1MB 미만'), t('1MB~100MB'), t('100MB~1GB'), t('1GB 이상')][i];
}

/** 화면에 보이는 이름들. 부르는 쪽이 store에서 읽어 넘깁니다. */
export interface LibraryNames {
  crew(space: SpaceId): string;
  channel(space: SpaceId, channel: ChannelId): string;
  author(f: LibraryFile): string;
  /** 다 받았는지(진행 알림까지 본 값, 없으면 메시지 보기의 downloaded). */
  stored?(f: LibraryFile): boolean;
}

export function isStored(f: LibraryFile, names: LibraryNames): boolean {
  return names.stored ? names.stored(f) : f.att.downloaded;
}

const collator = new Intl.Collator(undefined, { numeric: true, sensitivity: 'base' });

/** 정렬 값. 글자는 글자끼리, 수는 수끼리 비교합니다. */
function sortValue(f: LibraryFile, key: LibraryKey, names: LibraryNames): string | number {
  switch (key) {
    case 'uploaded':
      return f.item.message.created_at;
    case 'received':
      return f.item.received_at;
    case 'name':
      return plainName(f.att.name);
    case 'ext':
      return extOf(f.att.name);
    case 'kind':
      return KIND_ORDER.indexOf(fileKind(f.att));
    case 'size':
      return f.att.size;
    case 'crew':
      return names.crew(f.space);
    case 'channel':
      return names.channel(f.space, f.channel);
    case 'author':
      return names.author(f);
    case 'origin':
      return f.item.message.mine ? 0 : 1;
    case 'stored':
      return isStored(f, names) ? 0 : 1;
    case 'sharing':
      return f.item.excluded ? 1 : 0;
  }
}

function compareValues(a: string | number, b: string | number): number {
  return typeof a === 'number' && typeof b === 'number' ? a - b : collator.compare(String(a), String(b));
}

/** 규칙을 앞에서부터 차례로 비교합니다. 모두 같으면 최근에 올린 것, 그다음 키 순(늘 같은 순서). */
export function compareFiles(rules: readonly LibraryRule[], names: LibraryNames): (a: LibraryFile, b: LibraryFile) => number {
  return (a, b) => {
    for (const r of rules) {
      const c = compareValues(sortValue(a, r.key, names), sortValue(b, r.key, names));
      if (c !== 0) return r.desc ? -c : c;
    }
    return b.item.message.created_at - a.item.message.created_at || (a.key < b.key ? -1 : a.key > b.key ? 1 : 0);
  };
}

/** 분류 칸 하나의 값: 묶는 열쇠(`id`), 머리말(`label`), 칸끼리의 순서(`order`). */
function groupValue(f: LibraryFile, key: LibraryKey, names: LibraryNames): { id: string; label: string; order: string | number } {
  switch (key) {
    case 'uploaded':
    case 'received': {
      const ms = key === 'uploaded' ? f.item.message.created_at : f.item.received_at;
      // dayKey는 자릿수를 맞추지 않아 글자 순서가 날짜 순서와 다르므로, 그날 0시로 줄 세웁니다.
      const start = new Date(ms).setHours(0, 0, 0, 0);
      return { id: dayKey(ms), label: formatDay(ms), order: start };
    }
    case 'name': {
      const ch = [...plainName(f.att.name)][0]?.toLocaleUpperCase() ?? '';
      return { id: ch, label: ch || t('이름 없음'), order: ch };
    }
    case 'ext': {
      const ext = extOf(f.att.name);
      return { id: ext, label: ext ? ext.toUpperCase() : t('확장자 없음'), order: ext };
    }
    case 'kind': {
      const k = fileKind(f.att);
      return { id: k, label: kindLabel(k), order: KIND_ORDER.indexOf(k) };
    }
    case 'size': {
      const b = sizeBucket(f.att.size);
      return { id: String(b), label: sizeBucketLabel(b), order: b };
    }
    case 'crew': {
      const name = names.crew(f.space);
      return { id: f.space, label: name, order: name };
    }
    case 'channel': {
      const name = names.channel(f.space, f.channel);
      return { id: `${f.space}/${f.channel}`, label: `${name} · ${names.crew(f.space)}`, order: name };
    }
    case 'author': {
      const name = names.author(f);
      return { id: name, label: name, order: name };
    }
    case 'origin':
      return f.item.message.mine ? { id: 'sent', label: t('내가 올림'), order: 0 } : { id: 'got', label: t('받음'), order: 1 };
    case 'stored':
      return isStored(f, names) ? { id: 'here', label: t('이 기기에 있음'), order: 0 } : { id: 'away', label: t('이 기기에 없음'), order: 1 };
    case 'sharing':
      return f.item.excluded ? { id: 'off', label: t('공유 중단'), order: 1 } : { id: 'on', label: t('공유 중'), order: 0 };
  }
}

export interface LibraryGroup {
  /** 바깥 칸부터 이은 열쇠(접기 상태에 씀). */
  path: string;
  key: LibraryKey;
  label: string;
  /** 이 칸에 든 모든 파일(정렬 순서). */
  files: LibraryFile[];
  /** 다음 분류 규칙으로 나눈 칸. 마지막 규칙이면 비어 있습니다. */
  groups: LibraryGroup[];
}

/** 정렬한 파일을 분류 규칙 차례로 칸에 나눕니다. 칸 안의 파일 순서는 그대로(정렬 규칙을 따름). */
export function groupFiles(files: readonly LibraryFile[], rules: readonly LibraryRule[], names: LibraryNames, parent = ''): LibraryGroup[] {
  const [rule, ...rest] = rules;
  if (!rule) return [];
  const byId = new Map<string, { label: string; order: string | number; files: LibraryFile[] }>();
  for (const f of files) {
    const v = groupValue(f, rule.key, names);
    const g = byId.get(v.id);
    if (g) g.files.push(f);
    else byId.set(v.id, { label: v.label, order: v.order, files: [f] });
  }
  return [...byId.entries()]
    .sort(([ia, a], [ib, b]) => {
      const c = compareValues(a.order, b.order) || collator.compare(ia, ib);
      return rule.desc ? -c : c;
    })
    .map(([id, g]) => {
      const path = `${parent}/${rule.key}:${id}`;
      return { path, key: rule.key, label: g.label, files: g.files, groups: groupFiles(g.files, rest, names, path) };
    });
}

/** 가상 목록의 줄: 분류 머리말, 파일 하나(자세히), 파일 여러 개(갤러리 한 줄). `depth`는 들여쓰기 단계. */
export type LibraryRow =
  | { kind: 'group'; group: LibraryGroup; depth: number; collapsed: boolean }
  | { kind: 'file'; file: LibraryFile; depth: number }
  | { kind: 'tiles'; files: LibraryFile[]; depth: number };

/** 분류 칸을 펼친 차례대로 줄을 만듭니다. 접은 칸은 머리말만. 갤러리면 `cols`개씩 한 줄. */
export function libraryRows(sorted: readonly LibraryFile[], groups: readonly LibraryGroup[], gallery: boolean, cols: number, collapsed: ReadonlySet<string>): LibraryRow[] {
  const out: LibraryRow[] = [];
  const pushFiles = (files: readonly LibraryFile[], depth: number) => {
    if (!gallery) for (const file of files) out.push({ kind: 'file', file, depth });
    else for (let i = 0; i < files.length; i += Math.max(1, cols)) out.push({ kind: 'tiles', files: files.slice(i, i + Math.max(1, cols)), depth });
  };
  const walk = (gs: readonly LibraryGroup[], depth: number) => {
    for (const group of gs) {
      const c = collapsed.has(group.path);
      out.push({ kind: 'group', group, depth, collapsed: c });
      if (c) continue;
      if (group.groups.length > 0) walk(group.groups, depth + 1);
      else pushFiles(group.files, depth + 1);
    }
  };
  if (groups.length > 0) walk(groups, 0);
  else pushFiles(sorted, 0);
  return out;
}

/** 줄에 보이는 파일(접은 칸 빼고), 화면 순서. Shift 고르기의 범위에 씁니다. */
export function rowFiles(rows: readonly LibraryRow[]): LibraryFile[] {
  return rows.flatMap((r) => (r.kind === 'file' ? [r.file] : r.kind === 'tiles' ? r.files : []));
}

/** 규칙 목록 고치기(정렬·분류 창): 방향 바꾸기, 우선순위 올리고 내리기. */
export function toggleRuleDesc(rules: readonly LibraryRule[], key: LibraryKey): LibraryRule[] {
  return rules.map((r) => (r.key === key ? { ...r, desc: !r.desc } : r));
}

export function moveRule(rules: readonly LibraryRule[], key: LibraryKey, by: -1 | 1): LibraryRule[] {
  const i = rules.findIndex((r) => r.key === key);
  const j = i + by;
  if (i < 0 || j < 0 || j >= rules.length) return [...rules];
  const out = [...rules];
  [out[i], out[j]] = [out[j], out[i]];
  return out;
}

/** 자세히 보기 열 머리를 누름: 그 키를 첫 정렬로(이미 첫 정렬이면 방향만 바꿈), 나머지 규칙은 뒤에 그대로. */
export function sortByColumn(rules: readonly LibraryRule[], key: LibraryKey): LibraryRule[] {
  const first = rules[0];
  if (first?.key === key) return [{ key, desc: !first.desc }, ...rules.slice(1)];
  const old = rules.find((r) => r.key === key);
  return [{ key, desc: old?.desc ?? false }, ...rules.filter((r) => r.key !== key)];
}

// ───────────── 폴더 나무 ─────────────

/** 폴더 ID: 'all' | 'space:<공간>' | 'cat:<공간>/<카테고리>' | 'ch:<공간>/<채널>'. */
export const ALL_FOLDER = 'all';

export type FolderKind = 'all' | 'crew' | 'dm' | 'category' | 'channel';

export interface FolderNode {
  id: string;
  kind: FolderKind;
  label: string;
  space?: SpaceId;
  channel?: ChannelView;
  count: number;
  children: FolderNode[];
}

type ChannelLookup = (space: SpaceId, channel: ChannelId) => ChannelView | undefined;

/** 파일이 드는 폴더들(바깥부터). 1대1 대화는 채널이 하나라 채널 칸을 두지 않습니다. */
export function folderPath(f: Pick<LibraryFile, 'space' | 'channel'>, dm: boolean, lookup: ChannelLookup): string[] {
  const out = [ALL_FOLDER, `space:${f.space}`];
  if (dm) return out;
  const parent = lookup(f.space, f.channel)?.settings.parent;
  if (parent && lookup(f.space, parent)) out.push(`cat:${f.space}/${parent}`);
  out.push(`ch:${f.space}/${f.channel}`);
  return out;
}

function byPosition(a: FolderNode, b: FolderNode): number {
  return (a.channel?.settings.position ?? 0) - (b.channel?.settings.position ?? 0) || collator.compare(a.label, b.label);
}

/**
 * 왼쪽 폴더 나무: 전체 콘텐츠 → 크루(이름순, 그다음 1대1 대화) → 카테고리 → 채널(크루의 채널 순서). 파일이 있는 폴더만.
 * `spaceName`은 크루 이름이나 1대1 상대 이름.
 */
export function buildFolders(files: readonly LibraryFile[], spaces: readonly SpaceSummary[], spaceName: (s: SpaceSummary) => string, lookup: ChannelLookup): FolderNode {
  const root: FolderNode = { id: ALL_FOLDER, kind: 'all', label: t('전체 콘텐츠'), count: 0, children: [] };
  const nodes = new Map<string, FolderNode>([[ALL_FOLDER, root]]);
  const kinds = new Map(spaces.map((s) => [s.space, s]));
  for (const f of files) {
    const summary = kinds.get(f.space);
    const dm = summary?.kind === 'space';
    let parent = root;
    for (const id of folderPath(f, dm, lookup)) {
      let node = nodes.get(id);
      if (!node) {
        if (id.startsWith('space:')) {
          node = { id, kind: dm ? 'dm' : 'crew', label: summary ? spaceName(summary) : t('알 수 없는 크루'), space: f.space, count: 0, children: [] };
        } else {
          const cid = id.slice(id.indexOf('/') + 1);
          const ch = lookup(f.space, cid);
          node = { id, kind: id.startsWith('cat:') ? 'category' : 'channel', label: ch?.settings.name ?? t('알 수 없는 채널'), space: f.space, channel: ch, count: 0, children: [] };
        }
        nodes.set(id, node);
        parent.children.push(node);
      }
      node.count++;
      parent = node;
    }
  }
  const sortTree = (n: FolderNode) => {
    if (n.kind === 'all') n.children.sort((a, b) => Number(a.kind === 'dm') - Number(b.kind === 'dm') || collator.compare(a.label, b.label));
    else n.children.sort(byPosition);
    n.children.forEach(sortTree);
  };
  sortTree(root);
  return root;
}

/** 크루·채널·사람 이름과 파일 이름에 검색어(공백으로 나눈 낱말 모두)가 든 파일. */
export function matchesQuery(f: LibraryFile, query: string, names: LibraryNames): boolean {
  const words = query.toLocaleLowerCase().split(/\s+/).filter(Boolean);
  if (words.length === 0) return true;
  const hay = [plainName(f.att.name), names.crew(f.space), names.channel(f.space, f.channel), names.author(f)].join('\n').toLocaleLowerCase();
  return words.every((w) => hay.includes(w));
}
