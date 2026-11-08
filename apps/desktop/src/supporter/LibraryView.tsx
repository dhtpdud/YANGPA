// SPDX-License-Identifier: LicenseRef-YANGPA-Supporter
// 라이브러리 탭: 이 기기가 보내거나 받은 첨부를 탐색기처럼 한자리에서 봅니다. 왼쪽은 폴더 나무(전체 콘텐츠 → 크루·1대1 →
// 카테고리 → 채널), 가운데는 자세히·갤러리 보기, 오른쪽은 고른 파일의 미리 보기(채팅의 첨부 카드를 그대로 써서 받기·저장·보관·
// 다시 잇기가 같음). 두 번 누르거나 Enter면 화면 순서(여러 개를 골랐으면 고른 것만)를 콘텐츠 뷰어로 넘깁니다. 목록은 library.list를 쪽마다 끝까지 읽어 메모리에만 둡니다. 정렬·분류는 여러 단계를 우선순위대로
// 겹치고 설정(`library`)에 남습니다(고른 폴더·파일은 남기지 않음).
// 받지 않은 파일은 누를 때만 받습니다: 보인다는 이유로 blob.fetch·blob.status를 부르면 무엇을 보는지 멤버 기기에 드러납니다.
// 갤러리 썸네일은 보이는 줄의 메시지만 library.list(messages)로 묶어 읽습니다(목록 전체에 실으면 프레임이 너무 큼).
// 휘발성 채널의 첨부는 데몬이 기록하지 않아 나오지 않습니다.

import { useVirtualizer } from '@tanstack/react-virtual';
import {
  ArrowDown,
  ArrowUp,
  ArrowUpDown,
  Ban,
  ChevronDown,
  ChevronRight,
  CloudOff,
  Download,
  File as FileIcon,
  FileArchive,
  FileText,
  Film,
  Folder,
  FolderOpen,
  Hash,
  Image as ImageIcon,
  Images,
  Layers,
  LayoutGrid,
  Library,
  List,
  MessageSquare,
  Music,
  PanelRight,
  Pin,
  PinOff,
  Play,
  RefreshCw,
  Save,
  Share2,
  Trash,
  UserRound,
  X,
} from 'lucide-react';
import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState, type KeyboardEvent, type MouseEvent, type ReactNode } from 'react';
import type { ApiClient } from '../api/client';
import type { ChannelId, LibraryItem, MemberView, SpaceId } from '../api/types';
import { useThumbUrl } from '../hooks/media';
import { loadPins, markPinned, pinBlobs, usePinStore } from '../hooks/pins';
import { setSharing } from '../hooks/sharing';
import { t, tc } from '../i18n';
import { anonName, isAnonymousMessage } from '../lib/anon';
import { formatBytes, formatFull, formatStamp } from '../lib/format';
import {
  ALL_FOLDER,
  buildFolders,
  compareFiles,
  fileKind,
  folderPath,
  groupFiles,
  keyLabel,
  kindLabel,
  LIBRARY_KEYS,
  libraryFiles,
  libraryRows,
  matchesQuery,
  moveRule,
  rowFiles,
  sortByColumn,
  toggleRuleDesc,
  type FileKind,
  type FolderNode,
  type LibraryFile,
  type LibraryKey,
  type LibraryNames,
  type LibraryRow,
  type LibraryRule,
} from './library';
import { cx, displayName } from '../lib/misc';
import { isNarrow } from '../lib/platform';
import { plainName } from '../lib/text';
import { attType, type ViewerItem } from './viewer';
import { useStore } from '../store/store';
import { Attachment, saveAttachment } from '../components/Attachment';
import { BackButton } from '../components/BackButton';
import { ChannelIcon, Empty, Popover, Spinner } from '../components/common';
import { MenuButton, MenuSep } from '../components/menus';

const PAGE = 500;
/** 썸네일을 한 번에 묻는 메시지 수(데몬 상한 200). */
const THUMB_BATCH = 100;
const ROW_H = 32;
const GROUP_H = 34;
const TILE_W = 164;
const TILE_H = 188;

async function loadAll(client: ApiClient): Promise<LibraryItem[]> {
  const out: LibraryItem[] = [];
  // 쪽을 읽는 사이 새 글이 오면 뒤 쪽이 한 칸씩 밀려 같은 글이 또 옵니다.
  const seen = new Set<string>();
  for (let offset = 0; ; offset += PAGE) {
    const page = await client.call('library.list', { offset, limit: PAGE });
    for (const it of page.items) {
      if (seen.has(it.message.id)) continue;
      seen.add(it.message.id);
      out.push(it);
    }
    if (offset + PAGE >= page.total) return out;
  }
}

function KindIcon({ kind, size = 16 }: { kind: FileKind; size?: number }) {
  switch (kind) {
    case 'image':
      return <ImageIcon size={size} />;
    case 'video':
      return <Film size={size} />;
    case 'audio':
      return <Music size={size} />;
    case 'document':
      return <FileText size={size} />;
    case 'archive':
      return <FileArchive size={size} />;
    default:
      return <FileIcon size={size} />;
  }
}

/**
 * store에서 크루·채널·사람 이름을 읽는 함수 묶음. '이 기기에 있음'은 받기 진행(blobProgress)을 부를 때 읽습니다: 진행을
 * 구독하면 어느 파일이든 조각이 올 때마다 목록 전체를 다시 정렬하므로, 진행을 그리는 칸(FileState·미리 보기)만 구독합니다.
 */
function useNames(): LibraryNames {
  const spaces = useStore((s) => s.spaces);
  const views = useStore((s) => s.views);
  const friends = useStore((s) => s.friends);
  return useMemo(() => {
    const summary = new Map(spaces.map((s) => [s.space, s]));
    const dmName = new Map(friends.map((f) => [f.space, f.profile.name]));
    const members = new Map<string, Map<string, MemberView>>();
    const crew = (space: SpaceId) => dmName.get(space) ?? summary.get(space)?.name ?? t('알 수 없는 크루');
    return {
      crew,
      channel: (space, channel) =>
        summary.get(space)?.kind === 'space' ? crew(space) : (views[space]?.channels.find((c) => c.channel === channel)?.settings.name ?? t('알 수 없는 채널')),
      author: (f) => {
        const m = f.item.message;
        if (isAnonymousMessage(m)) return anonName(m);
        if (m.mine) return t('나');
        let map = members.get(f.space);
        if (!map) {
          map = new Map((views[f.space]?.members ?? []).map((x) => [x.user, x]));
          members.set(f.space, map);
        }
        const member = map.get(m.author);
        return member ? displayName(member) : (friends.find((x) => x.user === m.author)?.profile.name ?? t('알 수 없는 사용자'));
      },
      stored: (f) => storedNow(f, useStore.getState().blobProgress),
    };
  }, [spaces, views, friends]);
}

/** 다 받았는지: 메시지 보기의 downloaded는 다시 읽을 때까지 그대로라 받기 진행(blobProgress)도 봅니다. */
function storedNow(f: LibraryFile, progress: Record<string, { have: number; total: number }>): boolean {
  const p = progress[f.att.blob];
  return f.att.downloaded || (!!p && p.total > 0 && p.have >= p.total);
}

/** 보이는 파일의 썸네일을 메시지 묶음으로 읽어 둡니다. 키는 `메시지:블롭`, 썸네일이 없는 첨부는 ''. */
function useThumbs(client: ApiClient | null) {
  const [thumbs, setThumbs] = useState<ReadonlyMap<string, string>>(new Map());
  const asked = useRef(new Set<string>());
  const queue = useRef(new Set<string>());
  const timer = useRef<ReturnType<typeof setTimeout> | undefined>(undefined);
  useEffect(() => () => clearTimeout(timer.current), []);
  const want = useCallback(
    (files: readonly LibraryFile[]) => {
      if (!client) return;
      for (const f of files) {
        const id = f.item.message.id;
        if (fileKind(f.att) === 'other' || asked.current.has(id)) continue;
        asked.current.add(id);
        queue.current.add(id);
      }
      if (queue.current.size === 0 || timer.current) return;
      timer.current = setTimeout(async () => {
        timer.current = undefined;
        const ids = [...queue.current];
        queue.current.clear();
        for (let i = 0; i < ids.length; i += THUMB_BATCH) {
          const page = await client.call('library.list', { messages: ids.slice(i, i + THUMB_BATCH) }).catch(() => null);
          if (!page) continue;
          setThumbs((prev) => {
            const next = new Map(prev);
            for (const it of page.items) for (const a of it.message.attachments) next.set(`${it.message.id}:${a.blob}`, a.thumbnail_base64 ?? '');
            return next;
          });
        }
      }, 60);
    },
    [client],
  );
  return { thumbs, want };
}

export function LibraryView() {
  const client = useStore((s) => s.client);
  const prefs = useStore((s) => s.prefs.library);
  const setPrefs = useStore((s) => s.setPrefs);
  const spaces = useStore((s) => s.spaces);
  const views = useStore((s) => s.views);
  const [items, setItems] = useState<LibraryItem[] | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const setMobilePane = useStore((s) => s.setMobilePane);
  const [folder, setFolder] = useState(ALL_FOLDER);
  const [query, setQuery] = useState('');
  const [selected, setSelected] = useState<ReadonlySet<string>>(new Set());
  const [focus, setFocus] = useState<string | null>(null);
  const anchor = useRef<string | null>(null);
  const [collapsed, setCollapsed] = useState<ReadonlySet<string>>(new Set());
  const [menu, setMenu] = useState<DOMRect | null>(null);
  const names = useNames();
  const { thumbs, want } = useThumbs(client);

  const reload = useCallback(async () => {
    if (!client) return;
    setLoading(true);
    setError(null);
    try {
      setItems(await loadAll(client));
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setLoading(false);
    }
  }, [client]);
  // 탭을 떠나면 App이 이 화면을 내리므로, 다시 들어올 때마다 새로 읽습니다(그동안 오간 파일이 보이게).
  useEffect(() => {
    void reload();
  }, [reload]);
  // 보관 표시: 채팅 첨부 카드가 아직 읽지 않았으면 여기서 읽습니다.
  useEffect(() => {
    if (client && usePinStore.getState().status === 'idle') void loadPins(client);
  }, [client]);

  const lookup = useCallback((space: SpaceId, channel: ChannelId) => views[space]?.channels.find((c) => c.channel === channel), [views]);
  const isDm = useCallback((space: SpaceId) => spaces.find((s) => s.space === space)?.kind === 'space', [spaces]);
  const files = useMemo(() => libraryFiles(items ?? []), [items]);
  const tree = useMemo(() => buildFolders(files, spaces, (s) => (s.kind === 'space' ? names.crew(s.space) : s.name), lookup), [files, spaces, names, lookup]);
  const shown = useMemo(
    () => files.filter((f) => (folder === ALL_FOLDER || folderPath(f, isDm(f.space), lookup).includes(folder)) && matchesQuery(f, query, names)),
    [files, folder, isDm, lookup, query, names],
  );
  const sorted = useMemo(() => [...shown].sort(compareFiles(prefs.sort, names)), [shown, prefs.sort, names]);
  const groups = useMemo(() => groupFiles(sorted, prefs.group, names), [sorted, prefs.group, names]);
  const byKey = useMemo(() => new Map(files.map((f) => [f.key, f])), [files]);
  const picked = useMemo(() => [...selected].map((k) => byKey.get(k)).filter((f): f is LibraryFile => !!f), [selected, byKey]);
  const focused = focus ? byKey.get(focus) : undefined;

  // 폴더를 바꾸거나 다시 읽어 사라진 파일은 고른 것에서 뺍니다.
  useEffect(() => {
    const keep = new Set(shown.map((f) => f.key));
    setSelected((prev) => ([...prev].every((k) => keep.has(k)) ? prev : new Set([...prev].filter((k) => keep.has(k)))));
  }, [shown]);

  const patchItems = useCallback((fn: (it: LibraryItem) => LibraryItem) => setItems((prev) => prev && prev.map(fn)), []);
  const actions = useLibraryActions(patchItems);

  // 뷰어에 넘길 차례: 화면 순서(분류 칸 차례, 접은 칸 포함). 여러 개를 골랐고 그중 하나를 열면 고른 것만.
  const viewOrder = useMemo(() => rowFiles(libraryRows(sorted, groups, false, 1, new Set())), [sorted, groups]);
  const openViewer = useCallback(
    async (f: LibraryFile) => {
      const chosen = viewOrder.filter((x) => selected.has(x.key));
      const list = chosen.length > 1 && selected.has(f.key) ? chosen : viewOrder;
      const at = Math.max(0, list.findIndex((x) => x.key === f.key));
      // 목록에는 썸네일이 없어, 뷰어 썸네일 줄에 보일 앞뒤 항목의 썸네일을 먼저 읽습니다.
      const got = new Map(thumbs);
      const ids = [...new Set(list.slice(Math.max(0, at - 30), at + 31).filter((x) => !got.has(x.key)).map((x) => x.item.message.id))];
      if (client && ids.length > 0) {
        const page = await client.call('library.list', { messages: ids }).catch(() => null);
        for (const it of page?.items ?? []) for (const a of it.message.attachments) got.set(`${it.message.id}:${a.blob}`, a.thumbnail_base64 ?? '');
      }
      const viewerItems: ViewerItem[] = list.map((x) => {
        const thumb = got.get(x.key);
        return { key: x.key, type: attType(x.att), msg: x.item.message, att: thumb ? { ...x.att, thumbnail_base64: thumb } : x.att };
      });
      useStore.getState().openModal({ kind: 'fileViewer', items: viewerItems, start: f.key });
    },
    [viewOrder, selected, thumbs, client],
  );

  const pick = (f: LibraryFile, e: MouseEvent, order: readonly LibraryFile[]) => {
    if (e.shiftKey && anchor.current) {
      const a = order.findIndex((x) => x.key === anchor.current);
      const b = order.findIndex((x) => x.key === f.key);
      if (a >= 0 && b >= 0) {
        const range = order.slice(Math.min(a, b), Math.max(a, b) + 1).map((x) => x.key);
        setSelected(new Set(e.ctrlKey || e.metaKey ? [...selected, ...range] : range));
        setFocus(f.key);
        return;
      }
    }
    if (e.ctrlKey || e.metaKey) {
      const next = new Set(selected);
      if (next.has(f.key)) next.delete(f.key);
      else next.add(f.key);
      setSelected(next);
    } else {
      setSelected(new Set([f.key]));
    }
    anchor.current = f.key;
    setFocus(f.key);
  };

  const openMenu = (f: LibraryFile, e: MouseEvent) => {
    e.preventDefault();
    if (!selected.has(f.key)) {
      setSelected(new Set([f.key]));
      anchor.current = f.key;
    }
    setFocus(f.key);
    setMenu(new DOMRect(e.clientX, e.clientY, 0, 0));
  };

  const onKey = (e: KeyboardEvent, order: readonly LibraryFile[]) => {
    if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === 'a') {
      e.preventDefault();
      setSelected(new Set(order.map((f) => f.key)));
    } else if (e.key === 'Escape') {
      setSelected(new Set());
    } else if (e.key === 'Delete' && picked.length > 0) {
      actions.evict(picked);
    } else if (e.key === 'Enter' && focused) {
      void openViewer(focused);
    }
  };

  // 휴대폰은 고르면 위 미리 보기가 커져 목록이 줄어듭니다. 누른 줄이 아래로 밀려 나가지 않게 합니다.
  useEffect(() => {
    if (isNarrow()) document.querySelector('.lib-row.selected, .lib-tile.selected')?.scrollIntoView({ block: 'nearest' });
  }, [selected]);

  const crumbs = useMemo(() => folderCrumbs(tree, folder), [tree, folder]);
  const totalSize = picked.reduce((n, f) => n + f.att.size, 0);

  return (
    <>
      <aside className="sidebar library-tree" aria-label={t('라이브러리')}>
        <div className="sb-header">
          <span className="gap-s">
            <Library size={18} /> {t('라이브러리')}
          </span>
        </div>
        <div className="sb-scroll">{items && <FolderTree node={tree} depth={0} folder={folder} onPick={(id) => { setFolder(id); setMobilePane('chat'); }} />}</div>
      </aside>
      <main className="pane library-pane">
        <div className="topbar">
          <BackButton />
          <nav className="tb-left library-crumbs" aria-label={tc('파일이 있는 크루·채널', '위치')}>
            {crumbs.map((c, i) => (
              <span key={c.id} className="gap-s">
                {i > 0 && <ChevronRight size={14} className="muted" />}
                <button type="button" className="lib-crumb" onClick={() => setFolder(c.id)}>
                  {c.label}
                </button>
              </span>
            ))}
          </nav>
          <div className="gap-s">
            <input className="input small library-search" type="search" value={query} placeholder={t('이 폴더에서 찾기')} aria-label={t('이 폴더에서 찾기')} onChange={(e) => setQuery(e.target.value)} />
            <ViewToggle />
            <RuleButton kind="sort" rules={prefs.sort} onChange={(sort) => setPrefs({ library: { ...prefs, sort } })} />
            <RuleButton kind="group" rules={prefs.group} onChange={(group) => setPrefs({ library: { ...prefs, group } })} />
            <button type="button" className={cx('icon-btn', prefs.preview && 'active')} title={t('미리 보기')} aria-pressed={prefs.preview} onClick={() => setPrefs({ library: { ...prefs, preview: !prefs.preview } })}>
              <PanelRight size={18} />
            </button>
            <button type="button" className="icon-btn" title={t('다시 읽기')} disabled={loading} onClick={() => void reload()}>
              {loading ? <Spinner /> : <RefreshCw size={18} />}
            </button>
          </div>
        </div>
        {error ? (
          <Empty icon={<CloudOff size={40} />} title={t('라이브러리를 읽지 못했습니다')}>
            {error}
          </Empty>
        ) : !items ? (
          <div className="center grow">
            <Spinner size={24} />
          </div>
        ) : files.length === 0 ? (
          <Empty icon={<Library size={40} />} title={t('아직 주고받은 파일이 없습니다')}>
            {t('채팅에서 보내거나 받은 첨부가 여기 모입니다. 휘발성 채널의 첨부는 메모리에만 있어 나오지 않습니다.')}
          </Empty>
        ) : (
          <FileArea
            sorted={sorted}
            groups={groups}
            names={names}
            gallery={prefs.view === 'gallery'}
            sort={prefs.sort}
            collapsed={collapsed}
            selected={selected}
            thumbs={thumbs}
            want={want}
            onToggleGroup={(path) => {
              const next = new Set(collapsed);
              if (next.has(path)) next.delete(path);
              else next.add(path);
              setCollapsed(next);
            }}
            onSort={(key) => setPrefs({ library: { ...prefs, sort: sortByColumn(prefs.sort, key) } })}
            onPick={pick}
            onOpen={(f) => void openViewer(f)}
            onMenu={openMenu}
            onKey={onKey}
            onClear={() => setSelected(new Set())}
          />
        )}
        <div className="library-status">
          {t('항목 {n}개', { n: shown.length })}
          {picked.length > 0 && ` · ${t('{n}개 선택', { n: picked.length })} (${formatBytes(totalSize)})`}
        </div>
        {menu && picked.length > 0 && <FileMenu at={menu} files={picked} names={names} actions={actions} onView={(f) => void openViewer(f)} onClose={() => setMenu(null)} />}
      </main>
      {prefs.preview && <LibraryPreview files={picked} names={names} thumbs={thumbs} want={want} actions={actions} onView={(f) => void openViewer(f)} />}
    </>
  );
}

function folderCrumbs(root: FolderNode, id: string): { id: string; label: string }[] {
  const walk = (n: FolderNode, path: FolderNode[]): FolderNode[] | null => {
    const here = [...path, n];
    if (n.id === id) return here;
    for (const c of n.children) {
      const r = walk(c, here);
      if (r) return r;
    }
    return null;
  };
  return (walk(root, []) ?? [root]).map((n) => ({ id: n.id, label: n.label }));
}

function FolderTree({ node, depth, folder, onPick }: { node: FolderNode; depth: number; folder: string; onPick: (id: string) => void }) {
  const [open, setOpen] = useState(depth < 2);
  const icon =
    node.kind === 'all' ? (
      <Library size={16} />
    ) : node.kind === 'dm' ? (
      <UserRound size={16} />
    ) : node.kind === 'channel' && node.channel ? (
      <ChannelIcon channel={node.channel} size={16} />
    ) : node.kind === 'channel' ? (
      <Hash size={16} />
    ) : open ? (
      <FolderOpen size={16} />
    ) : (
      <Folder size={16} />
    );
  return (
    <div role={depth === 0 ? 'tree' : undefined}>
      <div className={cx('lib-folder', folder === node.id && 'active')} style={{ paddingLeft: 6 + depth * 14 }} role="treeitem" aria-selected={folder === node.id} aria-expanded={node.children.length > 0 ? open : undefined}>
        {node.children.length > 0 ? (
          <button type="button" className="icon-btn small" aria-label={open ? t('접기') : t('펼치기')} onClick={() => setOpen(!open)}>
            {open ? <ChevronDown size={14} /> : <ChevronRight size={14} />}
          </button>
        ) : (
          <span className="lib-folder-pad" />
        )}
        <button type="button" className="lib-folder-btn" onClick={() => onPick(node.id)} onDoubleClick={() => setOpen(!open)}>
          {icon}
          <span className="lib-folder-name">{node.label}</span>
          <span className="lib-folder-count">{node.count}</span>
        </button>
      </div>
      {open && node.children.map((c) => <FolderTree key={c.id} node={c} depth={depth + 1} folder={folder} onPick={onPick} />)}
    </div>
  );
}

function ViewToggle() {
  const prefs = useStore((s) => s.prefs.library);
  const setPrefs = useStore((s) => s.setPrefs);
  return (
    <div className="seg" role="group" aria-label={tc('라이브러리 보기 방식', '보기')}>
      <button type="button" className={cx('icon-btn', prefs.view === 'details' && 'active')} title={t('자세히')} aria-pressed={prefs.view === 'details'} onClick={() => setPrefs({ library: { ...prefs, view: 'details' } })}>
        <List size={18} />
      </button>
      <button type="button" className={cx('icon-btn', prefs.view === 'gallery' && 'active')} title={t('갤러리')} aria-pressed={prefs.view === 'gallery'} onClick={() => setPrefs({ library: { ...prefs, view: 'gallery' } })}>
        <LayoutGrid size={18} />
      </button>
    </div>
  );
}

/** 정렬·분류 규칙 편집: 위가 우선. 방향 바꾸기, 위아래로 옮기기, 빼기, 더하기. 정렬은 하나 이상 남깁니다. */
function RuleButton({ kind, rules, onChange }: { kind: 'sort' | 'group'; rules: LibraryRule[]; onChange: (r: LibraryRule[]) => void }) {
  const [at, setAt] = useState<DOMRect | null>(null);
  const title = kind === 'sort' ? tc('라이브러리 정렬 단추', '정렬') : t('분류');
  const unused = LIBRARY_KEYS.filter((k) => !rules.some((r) => r.key === k));
  const min = kind === 'sort' ? 1 : 0;
  return (
    <>
      <button type="button" className={cx('icon-btn', kind === 'group' && rules.length > 0 && 'active')} title={title} aria-haspopup="dialog" onClick={(e) => setAt(e.currentTarget.getBoundingClientRect())}>
        {kind === 'sort' ? <ArrowUpDown size={18} /> : <Layers size={18} />}
      </button>
      {at && (
        <Popover anchor={at} onClose={() => setAt(null)} placement="bottom-start" className="menu wide lib-rules">
          <div className="lib-rules-title">{title}</div>
          <div className="lib-rules-hint">{kind === 'sort' ? t('위에 있는 규칙부터 비교합니다. 같으면 다음 규칙으로.') : t('위에 있는 규칙으로 먼저 나누고, 그 안을 다음 규칙으로 다시 나눕니다.')}</div>
          {rules.length === 0 && <div className="lib-rules-empty">{t('분류하지 않음')}</div>}
          {rules.map((r, i) => (
            <div key={r.key} className="lib-rule">
              <span className="lib-rule-n">{i + 1}</span>
              <span className="lib-rule-name">{keyLabel(r.key)}</span>
              <button type="button" className="btn small ghost" onClick={() => onChange(toggleRuleDesc(rules, r.key))}>
                {r.desc ? <ArrowDown size={13} /> : <ArrowUp size={13} />} {r.desc ? t('내림차순') : t('오름차순')}
              </button>
              <button type="button" className="icon-btn small" title={t('우선순위 올리기')} disabled={i === 0} onClick={() => onChange(moveRule(rules, r.key, -1))}>
                <ArrowUp size={14} />
              </button>
              <button type="button" className="icon-btn small" title={t('우선순위 내리기')} disabled={i === rules.length - 1} onClick={() => onChange(moveRule(rules, r.key, 1))}>
                <ArrowDown size={14} />
              </button>
              <button type="button" className="icon-btn small" title={t('빼기')} disabled={rules.length <= min} onClick={() => onChange(rules.filter((x) => x.key !== r.key))}>
                <X size={14} />
              </button>
            </div>
          ))}
          {unused.length > 0 && (
            <select
              className="input small"
              value=""
              aria-label={t('규칙 더하기')}
              onChange={(e) => e.target.value && onChange([...rules, { key: e.target.value as LibraryKey, desc: e.target.value === 'uploaded' || e.target.value === 'received' }])}
            >
              <option value="">+ {t('규칙 더하기')}</option>
              {unused.map((k) => (
                <option key={k} value={k}>
                  {keyLabel(k)}
                </option>
              ))}
            </select>
          )}
        </Popover>
      )}
    </>
  );
}

/** 자세히 보기의 열. 이름 열만 늘어납니다. */
const COLUMNS: { key: LibraryKey; label: () => string }[] = [
  { key: 'name', label: () => t('이름') },
  { key: 'size', label: () => t('크기') },
  { key: 'kind', label: () => t('종류') },
  { key: 'uploaded', label: () => t('올린 시간') },
  { key: 'received', label: () => t('받은 시간') },
  { key: 'author', label: () => t('공유한 사람') },
  { key: 'crew', label: () => t('크루') },
  { key: 'channel', label: () => t('채널') },
  { key: 'stored', label: () => t('상태') },
];

function FileArea(p: {
  sorted: LibraryFile[];
  groups: ReturnType<typeof groupFiles>;
  names: LibraryNames;
  gallery: boolean;
  sort: LibraryRule[];
  collapsed: ReadonlySet<string>;
  selected: ReadonlySet<string>;
  thumbs: ReadonlyMap<string, string>;
  want: (files: readonly LibraryFile[]) => void;
  onToggleGroup: (path: string) => void;
  onSort: (key: LibraryKey) => void;
  onPick: (f: LibraryFile, e: MouseEvent, order: readonly LibraryFile[]) => void;
  onOpen: (f: LibraryFile) => void;
  onMenu: (f: LibraryFile, e: MouseEvent) => void;
  onKey: (e: KeyboardEvent, order: readonly LibraryFile[]) => void;
  onClear: () => void;
}) {
  const scrollRef = useRef<HTMLDivElement>(null);
  const [cols, setCols] = useState(4);
  useLayoutEffect(() => {
    const el = scrollRef.current;
    if (!el || !p.gallery) return;
    const fit = () => setCols(Math.max(1, Math.floor((el.clientWidth - 16) / TILE_W)));
    fit();
    const ro = new ResizeObserver(fit);
    ro.observe(el);
    return () => ro.disconnect();
  }, [p.gallery]);
  const rows = useMemo(() => libraryRows(p.sorted, p.groups, p.gallery, cols, p.collapsed), [p.sorted, p.groups, p.gallery, cols, p.collapsed]);
  const order = useMemo(() => rowFiles(rows), [rows]);
  const virt = useVirtualizer({
    count: rows.length,
    getScrollElement: () => scrollRef.current,
    estimateSize: (i) => (rows[i].kind === 'group' ? GROUP_H : rows[i].kind === 'file' ? ROW_H : TILE_H),
    getItemKey: (i) => rowKey(rows[i]),
    overscan: 6,
  });
  const items = virt.getVirtualItems();
  // 갤러리에서 그린 줄의 썸네일만 묻습니다.
  const first = items[0]?.index ?? 0;
  const last = items[items.length - 1]?.index ?? -1;
  const { gallery, want } = p;
  useEffect(() => {
    if (gallery) want(rowFiles(rows.slice(first, last + 1)));
  }, [gallery, want, rows, first, last]);
  const primary = p.sort[0];

  return (
    <div
      ref={scrollRef}
      className={cx('library-scroll', p.gallery ? 'gallery' : 'details')}
      tabIndex={0}
      onKeyDown={(e) => p.onKey(e, order)}
      onMouseDown={(e) => e.target === e.currentTarget && p.onClear()}
    >
      {!p.gallery && (
        <div className="lib-head">
          {COLUMNS.map((c) => {
            const idx = p.sort.findIndex((r) => r.key === c.key);
            return (
              <button key={c.key} type="button" className="lib-cell" aria-sort={idx === 0 ? (primary.desc ? 'descending' : 'ascending') : undefined} onClick={() => p.onSort(c.key)}>
                {c.label()}
                {idx === 0 && (primary.desc ? <ArrowDown size={12} /> : <ArrowUp size={12} />)}
                {idx > 0 && <span className="lib-sort-n">{idx + 1}</span>}
              </button>
            );
          })}
        </div>
      )}
      {rows.length === 0 && <div className="lib-none">{t('이 폴더에 맞는 파일이 없습니다')}</div>}
      <div className="lib-rows" role="listbox" aria-multiselectable aria-label={t('파일 목록')} style={{ height: virt.getTotalSize() }}>
        {items.map((vi) => {
          const row = rows[vi.index];
          return (
            <div key={vi.key} className="lib-vrow" style={{ transform: `translateY(${vi.start}px)`, height: vi.size }}>
              <RowView row={row} {...p} order={order} />
            </div>
          );
        })}
      </div>
    </div>
  );
}

function rowKey(r: LibraryRow): string {
  return r.kind === 'group' ? `g${r.group.path}` : r.kind === 'file' ? `f${r.file.key}` : `t${r.files[0].key}`;
}

function RowView(p: Parameters<typeof FileArea>[0] & { row: LibraryRow; order: readonly LibraryFile[] }) {
  const { row } = p;
  if (row.kind === 'group') {
    return (
      <button type="button" className="lib-group" aria-expanded={!row.collapsed} onClick={() => p.onToggleGroup(row.group.path)}>
        {/* 자세히 보기를 옆으로 밀어도 머리말은 왼쪽에 붙어 보이게. */}
        <span className="lib-group-in" style={{ paddingLeft: 8 + row.depth * 16 }}>
          {row.collapsed ? <ChevronRight size={14} /> : <ChevronDown size={14} />}
          <span className="lib-group-key">{keyLabel(row.group.key)}</span>
          <span className="lib-group-name">{row.group.label}</span>
          <span className="lib-group-count">{row.group.files.length}</span>
        </span>
      </button>
    );
  }
  const handlers = (f: LibraryFile) => ({
    onClick: (e: MouseEvent) => p.onPick(f, e, p.order),
    onDoubleClick: () => p.onOpen(f),
    onContextMenu: (e: MouseEvent) => p.onMenu(f, e),
  });
  if (row.kind === 'tiles') {
    return (
      <div className="lib-tiles" style={{ paddingLeft: 8 + row.depth * 16 }}>
        {row.files.map((f) => (
          <Tile key={f.key} f={f} thumb={p.thumbs.get(f.key)} names={p.names} selected={p.selected.has(f.key)} {...handlers(f)} />
        ))}
      </div>
    );
  }
  const f = row.file;
  const kind = fileKind(f.att);
  const m = f.item.message;
  return (
    <div role="option" aria-selected={p.selected.has(f.key)} className={cx('lib-row', p.selected.has(f.key) && 'selected')} {...handlers(f)}>
      <span className="lib-cell lib-name" style={{ paddingLeft: 8 + row.depth * 16 }} title={plainName(f.att.name)}>
        <KindIcon kind={kind} />
        <span className="ellipsis">{plainName(f.att.name)}</span>
      </span>
      <span className="lib-cell num">{formatBytes(f.att.size)}</span>
      <span className="lib-cell">{kindLabel(kind)}</span>
      <span className="lib-cell" title={formatFull(m.created_at)}>
        {formatStamp(m.created_at)}
      </span>
      <span className="lib-cell" title={formatFull(f.item.received_at)}>
        {formatStamp(f.item.received_at)}
      </span>
      <span className="lib-cell ellipsis">{p.names.author(f)}</span>
      <span className="lib-cell ellipsis">{p.names.crew(f.space)}</span>
      <span className="lib-cell ellipsis">{p.names.channel(f.space, f.channel)}</span>
      <span className="lib-cell">
        <FileState f={f} names={p.names} />
      </span>
    </div>
  );
}

/** 받기 진행·이 기기에 없음·보관·공유 중단 표시. */
function FileState({ f, names }: { f: LibraryFile; names: LibraryNames }) {
  const progress = useStore((s) => s.blobProgress[f.att.blob]);
  const pinned = usePinStore((s) => s.ids.has(f.att.blob));
  const stored = names.stored ? names.stored(f) : f.att.downloaded;
  return (
    <span className="lib-state">
      {!stored && progress && progress.total > 0 ? (
        <span>{Math.round((progress.have / progress.total) * 100)}%</span>
      ) : (
        !stored && (
          <span title={t('이 기기에 없음')}>
            <CloudOff size={14} />
          </span>
        )
      )}
      {pinned && (
        <span className="lib-kept" title={t('보관 중(저장 공간 정리에서 뺌)')}>
          <Pin size={14} />
        </span>
      )}
      {f.item.excluded && (
        <span className="lib-off" title={t('공유 중단: 이 기기에서 남에게 내주지 않음')}>
          <Ban size={14} />
        </span>
      )}
    </span>
  );
}

/** 이 기기에서 올린 파일(데몬이 늘 남겨 보관할 것이 없음). 채팅의 첨부 카드와 같은 기준. */
function uploadedHere(f: LibraryFile): boolean {
  const me = useStore.getState().myDevice;
  return !!me && f.item.message.author_device === me;
}

function Tile(p: { f: LibraryFile; thumb?: string; names: LibraryNames; selected: boolean; onClick: (e: MouseEvent) => void; onDoubleClick: () => void; onContextMenu: (e: MouseEvent) => void }) {
  const kind = fileKind(p.f.att);
  const url = useThumbUrl(p.thumb || undefined, false);
  const name = plainName(p.f.att.name);
  return (
    <div role="option" aria-selected={p.selected} className={cx('lib-tile', p.selected && 'selected')} title={name} onClick={p.onClick} onDoubleClick={p.onDoubleClick} onContextMenu={p.onContextMenu}>
      <span className="lib-tile-img">
        {url ? <img src={url} alt="" decoding="async" draggable={false} /> : <KindIcon kind={kind} size={40} />}
        {kind === 'video' && (
          <span className="lib-tile-badge">
            <Play size={11} fill="currentColor" />
          </span>
        )}
        <span className="lib-tile-state">
          <FileState f={p.f} names={p.names} />
        </span>
      </span>
      <span className="lib-tile-name">{name}</span>
      <span className="lib-tile-meta">{formatBytes(p.f.att.size)}</span>
    </div>
  );
}

// ───────────── 동작 ─────────────

interface LibraryActions {
  goto(f: LibraryFile): void;
  save(f: LibraryFile): void;
  fetch(files: readonly LibraryFile[]): void;
  share(files: readonly LibraryFile[], on: boolean): void;
  pin(files: readonly LibraryFile[], on: boolean): void;
  evict(files: readonly LibraryFile[]): void;
}

function useLibraryActions(patch: (fn: (it: LibraryItem) => LibraryItem) => void): LibraryActions {
  return useMemo(() => {
    const st = () => useStore.getState();
    return {
      goto(f) {
        const m = f.item.message;
        const ch = st().views[f.space]?.channels.find((c) => c.channel === f.channel);
        // 포럼은 게시글을 열고 그 안에서 찾습니다(검색 결과와 같음).
        const post = ch?.settings.kind === 'forum' ? (m.thread ?? m.id) : undefined;
        void st().jumpToMessage(f.space, f.channel, m.id, post);
      },
      save(f) {
        void saveAttachment(f.space, f.att.blob, f.item.message.id, plainName(f.att.name), false);
      },
      fetch(files) {
        const { client, call } = st();
        if (!client) return;
        for (const f of uniqueBy(files, (x) => `${x.space}/${x.att.blob}`)) {
          if (!f.att.downloaded) void call(client.call('blob.fetch', { space: f.space, blob: f.att.blob, priority: 200 }));
        }
      },
      share(files, on) {
        // 이미 그 상태인 메시지는 건드리지 않습니다.
        const targets = files.filter((f) => f.item.excluded === on).map((f) => ({ space: f.space, channel: f.channel, id: f.item.message.id }));
        void setSharing(targets, on, (ids) => patch((it) => (ids.has(it.message.id) ? { ...it, excluded: !on } : it)));
      },
      pin(files, on) {
        void pinBlobs(
          files.filter((f) => !uploadedHere(f)).map((f) => ({ space: f.space, blob: f.att.blob })),
          on,
        );
      },
      evict(files) {
        const { client, openModal } = st();
        if (!client) return;
        // 받다 만 조각도 지웁니다(진행 기록이 있으면 조각이 있음).
        const targets = uniqueBy(
          files.filter((f) => f.att.downloaded || !!st().blobProgress[f.att.blob]),
          (x) => x.att.blob,
        );
        if (targets.length === 0) {
          st().toast('info', t('이 기기에 둔 데이터가 없습니다'));
          return;
        }
        const mine = targets.some((f) => f.item.message.mine);
        openModal({
          kind: 'confirm',
          title: targets.length === 1 ? t('이 기기에서 지우기: {name}', { name: plainName(targets[0].att.name) }) : t('이 기기에서 파일 {n}개 지우기', { n: targets.length }),
          body:
            t('이 기기에 둔 파일 데이터만 지웁니다. 메시지와 첨부는 그대로 남고, 가진 사람이 있으면 다시 받을 수 있습니다.') +
            (mine ? '\n\n' + t('내가 올린 파일이 들어 있습니다. 지우면 이 기기에서 더는 내주지 못하고, 받아 둔 사람이 없으면 다시 받을 수 없습니다.') : ''),
          danger: true,
          confirmLabel: t('지우기'),
          onConfirm: async () => {
            const gone = new Set<string>();
            for (const f of targets) {
              const r = await st().call(client.call('blob.evict', { space: f.space, blob: f.att.blob }));
              if (r === undefined) continue;
              gone.add(f.att.blob);
              st().markEvicted(f.att.blob);
              // 데몬도 지우면서 보관을 풉니다.
              markPinned(f.att.blob, false);
            }
            if (gone.size === 0) return;
            // 블롭 데이터는 기기에 하나라 같은 파일을 실은 다른 메시지도 함께 비워집니다.
            patch((it) =>
              it.message.attachments.some((a) => gone.has(a.blob)) ? { ...it, message: { ...it.message, attachments: it.message.attachments.map((a) => (gone.has(a.blob) ? { ...a, downloaded: false } : a)) } } : it,
            );
            st().toast('success', t('파일 {n}개를 이 기기에서 지웠습니다', { n: gone.size }));
          },
        });
      },
    };
  }, [patch]);
}

function uniqueBy<T>(list: readonly T[], key: (x: T) => string): T[] {
  const seen = new Set<string>();
  return list.filter((x) => {
    const k = key(x);
    if (seen.has(k)) return false;
    seen.add(k);
    return true;
  });
}

function FileMenu({
  at,
  files,
  names,
  actions,
  onView,
  onClose,
}: {
  at: DOMRect;
  files: LibraryFile[];
  names: LibraryNames;
  actions: LibraryActions;
  onView: (f: LibraryFile) => void;
  onClose: () => void;
}) {
  const one = files.length === 1 ? files[0] : undefined;
  const stored = files.filter((f) => names.stored?.(f) ?? f.att.downloaded);
  const sharing = files.some((f) => !f.item.excluded);
  const keep = useKeep(files, names);
  const run = (fn: () => void) => () => {
    onClose();
    fn();
  };
  return (
    <Popover anchor={at} onClose={onClose} placement="bottom-start" className="menu wide">
      <MenuButton icon={<Images size={15} />} label={t('뷰어로 보기')} onClick={run(() => onView(files[0]))} />
      {one && <MenuButton icon={<MessageSquare size={15} />} label={t('메시지로 가기')} onClick={run(() => actions.goto(one))} />}
      {one && stored.length === 1 && <MenuButton icon={<Save size={15} />} label={t('다른 이름으로 저장')} onClick={run(() => actions.save(one))} />}
      {stored.length < files.length && <MenuButton icon={<Download size={15} />} label={t('원본 받기')} onClick={run(() => actions.fetch(files))} />}
      <MenuSep />
      <MenuButton
        icon={sharing ? <Ban size={15} /> : <Share2 size={15} />}
        label={sharing ? t('공유 중단') : t('공유 재개')}
        sub={files.some((f) => f.item.message.attachments.length > 1) ? t('같은 메시지의 다른 첨부도 함께') : undefined}
        onClick={run(() => actions.share(files, !sharing))}
      />
      {keep && <MenuButton icon={keep.all ? <PinOff size={15} /> : <Pin size={15} />} label={keep.all ? t('보관 풀기') : t('보관하기')} sub={keep.sub} onClick={run(() => actions.pin(files, !keep.all))} />}
      <MenuButton icon={<Trash size={15} />} label={t('이 기기에서 지우기')} danger disabled={stored.length === 0} onClick={run(() => actions.evict(files))} />
    </Popover>
  );
}

/** 고른 파일의 보관 단추 상태. 이 기기에서 올린 파일만 골랐거나 보관을 모르는 데몬이면 null(단추 없음). */
function useKeep(files: readonly LibraryFile[], names: LibraryNames): { all: boolean; sub?: string } | null {
  const status = usePinStore((s) => s.status);
  const ids = usePinStore((s) => s.ids);
  const can = files.filter((f) => !uploadedHere(f));
  if (status === 'unsupported' || can.length === 0) return null;
  const all = can.every((f) => ids.has(f.att.blob));
  // blob.pin은 이 기기에 없는 파일을 받기 시작합니다.
  const sub = !all && can.some((f) => !(names.stored?.(f) ?? f.att.downloaded)) ? t('이 기기에 없는 파일은 받아서 보관') : undefined;
  return { all, sub };
}

function LibraryPreview({
  files,
  names,
  thumbs,
  want,
  actions,
  onView,
}: {
  files: LibraryFile[];
  names: LibraryNames;
  thumbs: ReadonlyMap<string, string>;
  want: (files: readonly LibraryFile[]) => void;
  actions: LibraryActions;
  onView: (f: LibraryFile) => void;
}) {
  const myDevice = useStore((s) => s.myDevice);
  const f = files.length === 1 ? files[0] : undefined;
  // 받기가 끝나면 '이 기기에서 지우기'를 켜도록 이 파일의 진행만 구독합니다.
  useStore((s) => (f ? s.blobProgress[f.att.blob] : undefined));
  useEffect(() => {
    if (f) want([f]);
  }, [f, want]);
  const keep = useKeep(files, names);
  const keepBtn = keep && (
    <button type="button" className="btn small" title={keep.sub} onClick={() => actions.pin(files, !keep.all)}>
      {keep.all ? <PinOff size={14} /> : <Pin size={14} />} {keep.all ? t('보관 풀기') : t('보관하기')}
    </button>
  );
  if (files.length > 1) {
    const size = files.reduce((n, x) => n + x.att.size, 0);
    const sharing = files.some((x) => !x.item.excluded);
    return (
      <aside className="library-preview" aria-label={t('미리 보기')}>
        <div className="lib-prev-title">{t('{n}개 선택', { n: files.length })}</div>
        <div className="muted small">{formatBytes(size)}</div>
        <div className="lib-prev-actions">
          <button type="button" className="btn small" onClick={() => onView(files[0])}>
            <Images size={14} /> {t('뷰어로 보기')}
          </button>
          <button type="button" className="btn small" onClick={() => actions.fetch(files)}>
            <Download size={14} /> {t('원본 받기')}
          </button>
          <button type="button" className="btn small" onClick={() => actions.share(files, !sharing)}>
            {sharing ? <Ban size={14} /> : <Share2 size={14} />} {sharing ? t('공유 중단') : t('공유 재개')}
          </button>
          {keepBtn}
          <button type="button" className="btn small danger-outline" onClick={() => actions.evict(files)}>
            <Trash size={14} /> {t('이 기기에서 지우기')}
          </button>
        </div>
      </aside>
    );
  }
  if (!f) {
    return (
      <aside className="library-preview" aria-label={t('미리 보기')}>
        <Empty icon={<FileIcon size={32} />} title={t('파일을 고르세요')}>
          {t('Ctrl·Shift를 누른 채 누르면 여러 개를 고릅니다. 오른쪽 클릭으로 공유 중단·보관·지우기를 할 수 있습니다.')}
        </Empty>
      </aside>
    );
  }
  const m = f.item.message;
  const thumb = thumbs.get(f.key);
  const att = thumb ? { ...f.att, thumbnail_base64: thumb } : f.att;
  const stored = names.stored?.(f) ?? f.att.downloaded;
  const facts: [string, ReactNode][] = [
    [t('크기'), formatBytes(f.att.size)],
    [t('종류'), `${kindLabel(fileKind(f.att))}${f.att.mime ? ` · ${f.att.mime}` : ''}`],
    [t('올린 시간'), formatFull(m.created_at)],
    [t('받은 시간'), formatFull(f.item.received_at)],
    [t('공유한 사람'), names.author(f)],
    [tc('파일이 있는 크루·채널', '위치'), `${names.crew(f.space)} › ${names.channel(f.space, f.channel)}`],
    [t('공유 상태'), f.item.excluded ? t('공유 중단: 이 기기에서 남에게 내주지 않음') : t('공유 중')],
  ];
  return (
    <aside className="library-preview" aria-label={t('미리 보기')}>
      <div className="lib-prev-att">
        {/* 이 기기에서 지우면 카드가 들고 있던 받기 상태(blob.status)를 버리게 새로 그립니다. */}
        <Attachment key={`${f.key}:${stored ? 1 : 0}`} att={att} space={f.space} channel={f.channel} messageId={m.id} msgKey={`library:${m.id}`} volatile={false} own={!!myDevice && m.author_device === myDevice} />
      </div>
      <div className="lib-prev-title" title={plainName(f.att.name)}>
        {plainName(f.att.name)}
      </div>
      <dl className="lib-facts">
        {facts.map(([k, v]) => (
          <div key={k}>
            <dt>{k}</dt>
            <dd>{v}</dd>
          </div>
        ))}
      </dl>
      <div className="lib-prev-actions">
        <button type="button" className="btn small" onClick={() => onView(f)}>
          <Images size={14} /> {t('뷰어로 보기')}
        </button>
        <button type="button" className="btn small" onClick={() => actions.goto(f)}>
          <MessageSquare size={14} /> {t('메시지로 가기')}
        </button>
        <button type="button" className="btn small" onClick={() => actions.share([f], f.item.excluded)}>
          {f.item.excluded ? <Share2 size={14} /> : <Ban size={14} />} {f.item.excluded ? t('공유 재개') : t('공유 중단')}
        </button>
        {keepBtn}
        <button type="button" className="btn small danger-outline" disabled={!stored} onClick={() => actions.evict([f])}>
          <Trash size={14} /> {t('이 기기에서 지우기')}
        </button>
      </div>
    </aside>
  );
}
