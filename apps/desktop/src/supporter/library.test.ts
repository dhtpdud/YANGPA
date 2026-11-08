// SPDX-License-Identifier: LicenseRef-YANGPA-Supporter
import { describe, expect, it } from 'vitest';
import type { AttachmentView, ChannelView, LibraryItem, MessageView, SpaceSummary } from '../api/types';
import {
  buildFolders,
  compareFiles,
  extOf,
  fileKind,
  folderPath,
  groupFiles,
  libraryFiles,
  libraryRows,
  matchesQuery,
  moveRule,
  rowFiles,
  sanitizeLibrary,
  sortByColumn,
  type LibraryFile,
  type LibraryNames,
} from './library';

const att = (blob: string, name: string, size = 10, mime = 'application/octet-stream'): AttachmentView => ({ blob, name, mime, size, downloaded: true });

function item(id: string, space: string, channel: string, at: number, atts: AttachmentView[], extra: Partial<MessageView> = {}, received = at): LibraryItem {
  const message = { id, author: 'U1', author_device: 'D1', created_at: at, lamport: at, text: '', attachments: atts, embeds: [], reactions: [], deleted: false, volatile: false, author_blocked: false, space, channel, ...extra } as MessageView;
  return { message, received_at: received, excluded: false };
}

const NAMES: LibraryNames = {
  crew: (s) => ({ S1: '게임 모임', S2: '공부방', DM: '민지' })[s] ?? s,
  channel: (_s, c) => ({ C1: '일반', C2: '자료', C3: '사진' })[c] ?? c,
  author: (f) => (f.item.message.mine ? '나' : f.item.message.author),
};

describe('파일 줄', () => {
  it('메시지의 첨부마다 한 줄, 같은 메시지에 같은 블롭이 또 있으면 한 줄로', () => {
    const files = libraryFiles([item('M1', 'S1', 'C1', 1, [att('B1', 'a.png'), att('B1', 'a.png'), att('B2', 'b.txt')]), item('M2', 'S1', 'C2', 2, [att('B1', 'a.png')])]);
    expect(files.map((f) => f.key)).toEqual(['M1:B1', 'M1:B2', 'M2:B1']);
  });

  it('종류·확장자', () => {
    expect(extOf('보고서.최종.PDF')).toBe('pdf');
    expect(extOf('.bashrc')).toBe('');
    expect(extOf('README')).toBe('');
    expect(fileKind(att('B', 'x.png', 1, 'image/png'))).toBe('image');
    expect(fileKind(att('B', '노트.hwp'))).toBe('document');
    expect(fileKind(att('B', 'a.7z'))).toBe('archive');
    expect(fileKind(att('B', 'a.bin'))).toBe('other');
  });
});

describe('정렬·분류', () => {
  const files = libraryFiles([
    item('M1', 'S1', 'C1', 100, [att('B1', 'b.png', 5)]),
    item('M2', 'S2', 'C2', 200, [att('B2', 'a.png', 5)]),
    item('M3', 'S1', 'C2', 300, [att('B3', 'c.txt', 50)]),
    item('M4', 'S2', 'C1', 400, [att('B4', 'file10.txt', 5)], {}, 50),
    item('M5', 'S2', 'C1', 500, [att('B5', 'file9.txt', 5)], {}, 60),
  ]);
  const order = (rules: Parameters<typeof compareFiles>[0]) => [...files].sort(compareFiles(rules, NAMES)).map((f) => f.att.blob);

  it('앞 규칙부터 비교하고 같으면 다음 규칙, 모두 같으면 최근 것부터', () => {
    expect(order([{ key: 'size', desc: false }, { key: 'name', desc: false }])).toEqual(['B2', 'B1', 'B5', 'B4', 'B3']);
    expect(order([{ key: 'size', desc: true }])).toEqual(['B3', 'B5', 'B4', 'B2', 'B1']);
    // 받은 시간은 올린 시간과 따로.
    expect(order([{ key: 'received', desc: false }])).toEqual(['B4', 'B5', 'B1', 'B2', 'B3']);
  });

  it('여러 단계로 나누고, 칸 순서는 규칙 방향을 따름', () => {
    const sorted = [...files].sort(compareFiles([{ key: 'uploaded', desc: false }], NAMES));
    const groups = groupFiles(sorted, [{ key: 'crew', desc: true }, { key: 'ext', desc: false }], NAMES);
    expect(groups.map((g) => g.label)).toEqual(['공부방', '게임 모임']);
    expect(groups[0].groups.map((g) => [g.label, g.files.map((f) => f.att.blob)])).toEqual([
      ['PNG', ['B2']],
      ['TXT', ['B4', 'B5']],
    ]);
    expect(groups[1].groups.map((g) => g.label)).toEqual(['PNG', 'TXT']);
  });

  it('날짜 칸은 글자가 아니라 날짜 순서로', () => {
    const d = (m: number, day: number) => new Date(2026, m - 1, day, 12).getTime();
    const dated = libraryFiles([item('A', 'S1', 'C1', d(9, 30), [att('X', 'x')]), item('B', 'S1', 'C1', d(11, 1), [att('Y', 'y')])]);
    const groups = groupFiles(dated, [{ key: 'uploaded', desc: true }], NAMES);
    expect(groups.map((g) => g.files[0].att.blob)).toEqual(['Y', 'X']);
  });

  it('열 머리: 첫 정렬로 올리고, 이미 첫 정렬이면 방향만 바꿈', () => {
    const rules = [{ key: 'uploaded', desc: true } as const, { key: 'name', desc: false } as const];
    expect(sortByColumn(rules, 'name')).toEqual([{ key: 'name', desc: false }, { key: 'uploaded', desc: true }]);
    expect(sortByColumn(rules, 'uploaded')).toEqual([{ key: 'uploaded', desc: false }, { key: 'name', desc: false }]);
    expect(moveRule(rules, 'name', -1).map((r) => r.key)).toEqual(['name', 'uploaded']);
    expect(moveRule(rules, 'uploaded', -1)).toEqual(rules);
  });

  it('줄: 접은 칸은 머리말만, 갤러리는 칸 수만큼 한 줄', () => {
    const sorted = [...files].sort(compareFiles([{ key: 'uploaded', desc: false }], NAMES));
    const groups = groupFiles(sorted, [{ key: 'crew', desc: false }], NAMES);
    const rows = libraryRows(sorted, groups, false, 1, new Set([groups[0].path]));
    expect(rows.map((r) => r.kind)).toEqual(['group', 'group', 'file', 'file', 'file']);
    expect(rowFiles(rows).map((f) => f.att.blob)).toEqual(['B2', 'B4', 'B5']);
    const tiles = libraryRows(sorted, [], true, 2, new Set());
    expect(tiles.map((r) => (r.kind === 'tiles' ? r.files.length : 0))).toEqual([2, 2, 1]);
  });

  it('찾기는 파일·크루·채널·사람 이름에서 낱말을 모두', () => {
    const f = files[2];
    expect(matchesQuery(f, 'C.TXT 게임', NAMES)).toBe(true);
    expect(matchesQuery(f, 'txt 공부방', NAMES)).toBe(false);
    expect(matchesQuery(f, '   ', NAMES)).toBe(true);
  });
});

describe('폴더 나무', () => {
  const ch = (channel: string, name: string, position: number, parent: string | null = null, kind = 'text') => ({ channel, settings: { name, parent, position, kind } }) as unknown as ChannelView;
  const CHANNELS: Record<string, ChannelView[]> = { S1: [ch('K1', '게임', 0, null, 'category'), ch('C1', '일반', 1, 'K1'), ch('C2', '자료', 0)], DM: [ch('C9', 'dm', 0)] };
  const lookup = (s: string, c: string) => CHANNELS[s]?.find((x) => x.channel === c);
  const spaces = [
    { space: 'S1', kind: 'crew', name: '게임 모임' },
    { space: 'DM', kind: 'space', name: '' },
  ] as SpaceSummary[];

  it('크루 → 카테고리 → 채널, 1대1은 채널 칸 없이, 크루가 1대1보다 먼저', () => {
    const files = libraryFiles([item('M1', 'S1', 'C1', 1, [att('B1', 'a')]), item('M2', 'S1', 'C2', 2, [att('B2', 'b')]), item('M3', 'DM', 'C9', 3, [att('B3', 'c')])]);
    const root = buildFolders(files, spaces, (s) => (s.kind === 'space' ? '민지' : s.name), lookup);
    expect(root.count).toBe(3);
    expect(root.children.map((n) => [n.kind, n.label, n.count])).toEqual([
      ['crew', '게임 모임', 2],
      ['dm', '민지', 1],
    ]);
    // 채널 순서(position)대로: 자료(0)가 카테고리(0) 앞일 수 있어 이름으로 다시 가름.
    expect(root.children[0].children.map((n) => [n.kind, n.label])).toEqual([
      ['category', '게임'],
      ['channel', '자료'],
    ]);
    expect(root.children[0].children[0].children.map((n) => n.label)).toEqual(['일반']);
    expect(root.children[1].children).toEqual([]);
    expect(folderPath(files[0] as LibraryFile, false, lookup)).toEqual(['all', 'space:S1', 'cat:S1/K1', 'ch:S1/C1']);
  });
});

describe('설정', () => {
  it('모르는 키·겹친 키는 버리고 정렬은 비지 않게', () => {
    expect(sanitizeLibrary({ view: 'gallery', sort: [{ key: 'size', desc: true }, { key: 'size' }, { key: 'nope' }], group: [{ key: 'crew' }], preview: false })).toEqual({
      view: 'gallery',
      sort: [{ key: 'size', desc: true }],
      group: [{ key: 'crew', desc: false }],
      preview: false,
    });
    expect(sanitizeLibrary({ sort: [] }).sort).toEqual([{ key: 'uploaded', desc: true }]);
    expect(sanitizeLibrary('x').view).toBe('details');
  });
});
