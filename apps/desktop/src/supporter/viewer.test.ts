// SPDX-License-Identifier: LicenseRef-YANGPA-Supporter
// 뷰어 항목 만들기·설정 정리.

import { describe, expect, it } from 'vitest';
import { ANONYMOUS_USER, type AttachmentView, type MessageView } from '../api/types';
import { buildViewerItems, DEFAULT_VIEWER, imageFit, parseWords, sanitizeViewer, wavePeaks, type ViewerPrefs } from './viewer';

function att(blob: string, mime: string, name = `${blob}.bin`): AttachmentView {
  return { blob, name, mime, size: 1, downloaded: true };
}

function msg(id: string, text: string, attachments: AttachmentView[] = [], extra: Partial<MessageView> = {}): MessageView {
  return {
    id,
    author: 'u1',
    author_device: 'd1',
    created_at: 0,
    lamport: 0,
    text,
    attachments,
    embeds: [],
    reactions: [],
    deleted: false,
    volatile: false,
    ...extra,
  };
}

const keys = (messages: MessageView[], v: Partial<ViewerPrefs> = {}) => buildViewerItems(messages, { ...DEFAULT_VIEWER, ...v }).map((i) => i.key);

const msgs = [
  msg('a', '첫 글'),
  msg('b', '사진 두 장', [att('p1', 'image/png'), att('zip', 'application/zip'), att('p2', 'image/webp'), att('p1', 'image/png')]),
  msg('c', '', [att('v1', 'video/mp4', '고양이.mp4')]),
  msg('d', '지운 글', [], { deleted: true }),
  msg('e', '차단한 사람', [], { author_blocked: true }),
  msg('f', '   '),
];

describe('buildViewerItems', () => {
  it('최신순은 메시지만 뒤집고 첨부는 올린 순서, 첨부가 있는 메시지의 글은 따로 두지 않음', () => {
    expect(keys(msgs)).toEqual(['c:v1', 'b:p1', 'b:zip', 'b:p2', 'a:text']);
    expect(keys(msgs, { order: 'oldest' })).toEqual(['a:text', 'b:p1', 'b:zip', 'b:p2', 'c:v1']);
  });

  it('종류: 그릴 수 없는 형식은 파일, 종류마다 끄기', () => {
    const types = buildViewerItems(
      [msg('x', '', [att('s', 'audio/mpeg'), att('w', 'audio/x-flac'), att('t', 'image/tiff'), att('m', 'video/quicktime')])],
      DEFAULT_VIEWER,
    ).map((i) => i.type);
    expect(types).toEqual(['audio', 'file', 'file', 'file']);
    expect(keys(msgs, { text: false, videos: false })).toEqual(['b:p1', 'b:zip', 'b:p2']);
    expect(keys(msgs, { images: false, files: false })).toEqual(['c:v1', 'a:text']);
    expect(keys([msg('x', '', [att('s', 'audio/ogg')])], { audio: false })).toEqual([]);
  });

  it('사용자 필터는 고른 사람의 글만, 익명 글은 뺌', () => {
    const people = [msg('x', '민지 글'), msg('y', '도윤 글', [], { author: 'u2' }), msg('z', '익명 글', [], { author: ANONYMOUS_USER, anonymous: true })];
    const pick = (authors: string[]) => buildViewerItems(people, DEFAULT_VIEWER, authors).map((i) => i.key);
    expect(pick([])).toEqual(['z:text', 'y:text', 'x:text']);
    expect(pick(['u2'])).toEqual(['y:text']);
    expect(pick(['u1', 'u2', ANONYMOUS_USER])).toEqual(['y:text', 'x:text']);
  });

  it('단어는 글과 첨부 이름에서 대소문자 없이 찾고, 제외가 포함보다 먼저', () => {
    expect(keys(msgs, { include: ['고양이'] })).toEqual(['c:v1']);
    expect(keys(msgs, { include: ['글', '사진'] })).toEqual(['b:p1', 'b:zip', 'b:p2', 'a:text']);
    expect(keys(msgs, { include: ['사진'], exclude: ['두 장'] })).toEqual([]);
    expect(keys([msg('x', 'Hello')], { include: ['hello'] })).toEqual(['x:text']);
  });
});

describe('wavePeaks', () => {
  it('구간마다 모든 채널의 가장 큰 진폭을 가장 큰 막대 기준으로', () => {
    const left = Float32Array.from([0.1, -0.2, 0, 0.05, 0.4, -0.1]);
    const right = Float32Array.from([0, 0, -0.8, 0, 0, 0]);
    expect(wavePeaks([left, right], 3)).toEqual([0.25, 1, 0.5]);
    // 막대가 표본보다 많아도 빈 막대 없이, 소리가 없으면 0.
    expect(wavePeaks([Float32Array.from([0.5, -1])], 4)).toEqual([0.5, 0.5, 1, 1]);
    expect(wavePeaks([new Float32Array(4)], 2)).toEqual([0, 0]);
    expect(wavePeaks([], 2)).toEqual([0, 0]);
  });
});

describe('설정', () => {
  it('단어 목록은 나누고 중복을 빼며, 믿지 않는 값은 기본값·범위로', () => {
    expect(parseWords(' 가, 나\n가 ,,A,a ')).toEqual(['가', '나', 'A']);
    expect(sanitizeViewer({ order: 'oldest', fit: 'x', fillPercent: 5, scrollStep: '600', seek: 'abc', text: false, include: [1, '고양이'] })).toEqual({
      ...DEFAULT_VIEWER,
      order: 'oldest',
      fillPercent: 10,
      scrollStep: 600,
      text: false,
      include: ['고양이'],
    });
    expect(sanitizeViewer(null)).toEqual(DEFAULT_VIEWER);
    expect(parseWords(Array.from({ length: 80 }, (_, i) => `w${i}`))).toHaveLength(30);
  });

  it('맞춤: 자동은 가로가 길면 높이, 세로가 길면 폭, 크기를 모르거나 꺼짐이면 화면 안', () => {
    expect(imageFit(DEFAULT_VIEWER, 1600, 900)).toBe('height');
    expect(imageFit(DEFAULT_VIEWER, 800, 3000)).toBe('width');
    expect(imageFit(DEFAULT_VIEWER)).toBe('');
    expect(imageFit({ ...DEFAULT_VIEWER, fit: 'width' })).toBe('width');
    expect(imageFit({ ...DEFAULT_VIEWER, fill: false }, 1600, 900)).toBe('');
  });
});
