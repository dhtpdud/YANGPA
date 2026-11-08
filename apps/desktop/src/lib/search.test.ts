import { describe, expect, it } from 'vitest';
import type { MemberView } from '../api/types';
import { applyAuthor, authorSuggestions, pageList, parseSearch, searchTerms } from './search';

const member = (user: string, name: string, nickname?: string) =>
  ({ user, profile: { name }, nickname, roles: [], online: false, automation: false }) as unknown as MemberView;
const MEMBERS = [member('U1', '세영'), member('U2', '김 민지'), member('U3', 'doyun', '도윤')];

describe('검색어 풀이', () => {
  it('from:이름을 사람으로 바꾸고 나머지는 본문 검색어로 둔다', () => {
    expect(parseSearch('from:세영 레이드', MEMBERS)).toEqual({ query: '레이드', author: 'U1', authorName: '세영' });
    expect(parseSearch('공지 from:김_민지', MEMBERS)).toEqual({ query: '공지', author: 'U2', authorName: '김 민지' });
    // 별명이 화면 이름이지만 프로필 이름으로도 찾습니다.
    expect(parseSearch('FROM:DoYun', MEMBERS).author).toBe('U3');
    expect(parseSearch('from:없는사람 글', MEMBERS)).toEqual({ query: '글', unknown: '없는사람' });
    expect(parseSearch('  그냥 글  ', MEMBERS)).toEqual({ query: '그냥 글' });
  });

  it('입력 끝의 from:에 맞는 멤버를 추천하고 고르면 토큰으로 채운다', () => {
    expect(authorSuggestions('레이드 from:', MEMBERS)?.map((m) => m.user)).toEqual(['U1', 'U2', 'U3']);
    expect(authorSuggestions('from:민', MEMBERS)?.map((m) => m.user)).toEqual(['U2']);
    expect(authorSuggestions('레이드', MEMBERS)).toBeNull();
    expect(applyAuthor('레이드 from:민', MEMBERS[1])).toBe('레이드 from:김_민지 ');
  });
});

describe('검색 낱말·쪽 번호', () => {
  it('낱말은 빈칸으로 나누고 큰따옴표는 구절, 다른 낱말에 든 낱말은 뺀다', () => {
    expect(searchTerms('회의 자료 회의')).toEqual(['회의', '자료']);
    expect(searchTerms('"회의  자료" 회의 RUST')).toEqual(['회의 자료', 'rust']);
    expect(searchTerms('  ""  ')).toEqual([]);
  });

  it('첫 쪽·끝 쪽과 지금 쪽 둘레를 보이고 사이는 줄인다', () => {
    expect(pageList(1, 5)).toEqual([1, 2, 3, 4, 5]);
    expect(pageList(1, 20)).toEqual([1, 2, 3, 4, 5, 6, 7, 'gap', 20]);
    expect(pageList(10, 20)).toEqual([1, 'gap', 8, 9, 10, 11, 12, 'gap', 20]);
    expect(pageList(20, 20)).toEqual([1, 'gap', 14, 15, 16, 17, 18, 19, 20]);
    // 빈칸이 한 쪽만 가리면 그 번호를 그대로.
    expect(pageList(5, 20)).toEqual([1, 2, 3, 4, 5, 6, 7, 'gap', 20]);
  });
});
