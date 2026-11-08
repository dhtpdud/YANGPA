import { describe, expect, it } from 'vitest';
import { MAX_RENDER_CHARS, SPOILER_TEXT, parseMarkdown, stripMarkdown, type Block, type Inline } from './markdown';
import { excerpt } from './misc';

const USER = '7Hq3sZzXfF8aB2cD4eF6gH8jK9mN1pQ3rS5tU7vW9xY';

/** 문단 하나짜리 글의 인라인 노드. */
function inline(text: string): Inline[] {
  const b = parseMarkdown(text);
  expect(b).toHaveLength(1);
  if (b[0].t !== 'p') throw new Error(`문단이 아님: ${b[0].t}`);
  return b[0].children;
}

const txt = (v: string): Inline => ({ t: 'text', v });
const p = (...children: Inline[]): Block => ({ t: 'p', children });

describe('마크다운: 글 조각(예전 tokenize와 같음)', () => {
  it('멘션·링크·이모지·인라인 코드를 나눈다', () => {
    const n = inline(`<@${USER}> 이거 봐 https://example.com/a?b=1. \`code\` <:gg:5>`);
    expect(n.map((s) => s.t)).toEqual(['mention', 'text', 'url', 'text', 'code', 'text', 'emoji']);
    expect(n[2]).toEqual({ t: 'url', href: 'https://example.com/a?b=1' });
  });

  it('@everyone·@here는 낱말 경계에서만 모두 멘션이 된다(데몬 broadcast_mention과 같음)', () => {
    expect(inline('@everyone 공지 (@here)')).toEqual([{ t: 'everyone', v: 'everyone' }, txt(' 공지 ('), { t: 'everyone', v: 'here' }, txt(')')]);
    for (const no of ['a@everyone', '@everyones', '@here_', '\\@here', '`@here`', '@Everyone']) {
      expect(inline(no).map((n) => n.t)).not.toContain('everyone');
    }
    expect(stripMarkdown('**@here** 모여')).toBe('@here 모여');
  });

  it('코드 블록 안은 해석하지 않고, 블록 앞뒤 줄바꿈은 블록이 먹는다', () => {
    expect(parseMarkdown('앞\n```rust\nlet x = "<@abc>" **a**;\n```\n뒤')).toEqual([
      p(txt('앞')),
      { t: 'codeblock', lang: 'rust', v: 'let x = "<@abc>" **a**;' },
      p(txt('뒤')),
    ]);
    // 줄바꿈 없이 붙인 첫 낱말은 언어가 아니라 내용입니다.
    expect(parseMarkdown('```hello```')).toEqual([{ t: 'codeblock', lang: '', v: 'hello' }]);
    // 이스케이프한 ```와 빈 코드 블록은 글자 그대로.
    expect(stripMarkdown('\\```a```')).toBe('```a```');
    expect(parseMarkdown('``````')).toEqual([p(txt('``````'))]);
  });

  it('HTML은 그냥 글자로 남는다', () => {
    expect(parseMarkdown('<img src=x onerror=alert(1)>')).toEqual([p(txt('<img src=x onerror=alert(1)>'))]);
    expect(parseMarkdown('<a href="javascript:alert(1)">x</a>')).toEqual([p(txt('<a href="javascript:alert(1)">x</a>'))]);
  });

  it('너무 긴 본문은 잘라서 그린다', () => {
    const n = inline('가'.repeat(MAX_RENDER_CHARS + 100));
    expect(n).toEqual([txt('가'.repeat(MAX_RENDER_CHARS) + '…')]);
  });
});

describe('마크다운: 인라인 서식', () => {
  it('굵게·기울임·밑줄·취소선·스포일러', () => {
    const n = inline('**굵게** *기울임* _기울임_ __밑줄__ ~~취소~~ ||스포||');
    expect(n.filter((x) => x.t !== 'text')).toEqual([
      { t: 'bold', children: [txt('굵게')] },
      { t: 'italic', children: [txt('기울임')] },
      { t: 'italic', children: [txt('기울임')] },
      { t: 'underline', children: [txt('밑줄')] },
      { t: 'strike', children: [txt('취소')] },
      { t: 'spoiler', children: [txt('스포')] },
    ]);
  });

  it('서식은 겹칠 수 있다', () => {
    expect(inline('***둘 다***')).toEqual([{ t: 'bold', children: [{ t: 'italic', children: [txt('둘 다')] }] }]);
    expect(inline('__*밑줄 기울임*__')).toEqual([{ t: 'underline', children: [{ t: 'italic', children: [txt('밑줄 기울임')] }] }]);
    expect(inline('*바깥 **안쪽** 바깥*')).toEqual([
      { t: 'italic', children: [txt('바깥 '), { t: 'bold', children: [txt('안쪽')] }, txt(' 바깥')] },
    ]);
    expect(inline('||**비밀** 링크 https://a.b||')).toEqual([
      { t: 'spoiler', children: [{ t: 'bold', children: [txt('비밀')] }, txt(' 링크 '), { t: 'url', href: 'https://a.b' }] },
    ]);
  });

  it('코드 안에서는 서식을 보지 않고, 닫는 표시를 찾을 때도 코드는 건너뛴다', () => {
    expect(inline('`**x** _y_`')).toEqual([{ t: 'code', v: '**x** _y_' }]);
    expect(inline('**a `**` b**')).toEqual([{ t: 'bold', children: [txt('a '), { t: 'code', v: '**' }, txt(' b')] }]);
    expect(inline('`` a`b ``')).toEqual([{ t: 'code', v: 'a`b' }]);
  });

  it('짝이 없거나 자리가 맞지 않으면 글자 그대로', () => {
    expect(inline('**열기만')).toEqual([txt('**열기만')]);
    expect(inline('2 * 3 = 6, a ** b')).toEqual([txt('2 * 3 = 6, a ** b')]);
    expect(inline('snake_case_name')).toEqual([txt('snake_case_name')]);
    expect(inline('`닫지 않은 코드')).toEqual([txt('`닫지 않은 코드')]);
    expect(inline('****')).toEqual([txt('****')]);
    expect(inline('*앞 공백 *')).toEqual([txt('*앞 공백 *')]);
  });

  it('링크 안의 _·*는 서식이 아니고, 서식 안의 링크는 링크다', () => {
    expect(inline('https://a.com/x_y_z *b*')).toEqual([{ t: 'url', href: 'https://a.com/x_y_z' }, txt(' '), { t: 'italic', children: [txt('b')] }]);
    expect(inline('**https://a.com**')).toEqual([{ t: 'bold', children: [{ t: 'url', href: 'https://a.com' }] }]);
    expect(inline('<https://a.com/<x>')).toEqual([txt('<'), { t: 'url', href: 'https://a.com/' }, txt('<x>')]);
    expect(inline('<https://a.com/b>')).toEqual([{ t: 'url', href: 'https://a.com/b' }]);
  });

  it('\\로 서식 문자를 글자 그대로 쓴다(윈도 경로의 \\는 그대로)', () => {
    expect(inline('\\*별\\* \\_x\\_ \\|\\|y\\|\\| \\`z\\`')).toEqual([txt('*별* _x_ ||y|| `z`')]);
    expect(inline('C:\\Users\\나')).toEqual([txt('C:\\Users\\나')]);
    expect(parseMarkdown('\\# 제목 아님\n\\> 인용 아님\n\\- 목록 아님\n1\\. 번호 아님')).toEqual([p(txt('# 제목 아님\n> 인용 아님\n- 목록 아님\n1. 번호 아님'))]);
  });

  it('가린 링크는 http(s)만, 글이 주소처럼 보이면 가리지 않는다', () => {
    expect(inline('[문서 **보기**](https://ex.com/a_(b))')).toEqual([
      { t: 'link', href: 'https://ex.com/a_(b)', children: [txt('문서 '), { t: 'bold', children: [txt('보기')] }] },
    ]);
    expect(inline('[글](<https://ex.com/x>)')).toEqual([{ t: 'link', href: 'https://ex.com/x', children: [txt('글')] }]);
    expect(inline('[눌러](javascript:alert(1))')).toEqual([txt('[눌러](javascript:alert(1))')]);
    expect(inline('[x](https://user:pw@ex.com)')).toEqual([txt('[x]('), { t: 'url', href: 'https://user:pw@ex.com' }, txt(')')]);
    const fake = inline('[https://good.com](https://evil.com)');
    expect(fake.some((x) => x.t === 'link')).toBe(false);
    expect(stripMarkdown('[https://good.com](https://evil.com)')).toContain('evil.com');
    // 글 속 도메인이 실제 호스트와 다르면(키릴 'а'로 비슷하게 쓴 것 포함) 가리지 않고, 같은 곳이면 가립니다.
    for (const text of ['www.paypal.com/login', 'pаypal.com', '네이버.com 로그인']) {
      expect(inline(`[${text}](https://evil.example/t)`).some((x) => x.t === 'link'), text).toBe(false);
    }
    expect(inline('[github.com/a](https://github.com/a)')).toEqual([{ t: 'link', href: 'https://github.com/a', children: [txt('github.com/a')] }]);
    expect(inline('[docs.rs 문서](https://www.docs.rs/x)')).toEqual([{ t: 'link', href: 'https://www.docs.rs/x', children: [txt('docs.rs 문서')] }]);
    // 가린 링크 글 안에서는 멘션·링크를 만들지 않습니다.
    expect(inline(`[<@${USER}>](https://ex.com)`)).toEqual([{ t: 'link', href: 'https://ex.com', children: [txt(`<@${USER}>`)] }]);
  });
});

describe('마크다운: 블록', () => {
  it('줄 맨 앞의 #·##·###만 제목, -#는 작은 글씨', () => {
    expect(parseMarkdown('# 큰\n## 중간\n### 작은\n#### 아님\n#붙임\n-# 덧붙임\n글 # 가운데')).toEqual([
      { t: 'heading', level: 1, children: [txt('큰')] },
      { t: 'heading', level: 2, children: [txt('중간')] },
      { t: 'heading', level: 3, children: [txt('작은')] },
      p(txt('#### 아님\n#붙임')),
      { t: 'subtext', children: [txt('덧붙임')] },
      p(txt('글 # 가운데')),
    ]);
  });

  it('> 는 이어진 줄까지, >>> 는 글 끝까지 인용하고 인용은 한 겹만', () => {
    expect(parseMarkdown('> 하나\n> **둘**\n밖')).toEqual([
      { t: 'quote', children: [p(txt('하나\n'), { t: 'bold', children: [txt('둘')] })] },
      p(txt('밖')),
    ]);
    expect(parseMarkdown('앞\n>>> 전부\n# 제목\n```\n코드\n```')).toEqual([
      p(txt('앞')),
      { t: 'quote', children: [p(txt('전부')), { t: 'heading', level: 1, children: [txt('제목')] }, { t: 'codeblock', lang: '', v: '코드' }] },
    ]);
    expect(parseMarkdown('> > 겹침')).toEqual([{ t: 'quote', children: [p(txt('> 겹침'))] }]);
    expect(parseMarkdown('>붙임')).toEqual([p(txt('>붙임'))]);
  });

  it('목록: -·* 와 번호, 들여 쓰면 하위 목록, 종류가 바뀌면 새 목록', () => {
    expect(parseMarkdown('- 하나\n* **둘**\n  - 안쪽\n3. 셋째\n4. 넷째\n끝')).toEqual([
      {
        t: 'list',
        ordered: false,
        start: 1,
        items: [
          { children: [txt('하나')], sub: [] },
          {
            children: [{ t: 'bold', children: [txt('둘')] }],
            sub: [{ t: 'list', ordered: false, start: 1, items: [{ children: [txt('안쪽')], sub: [] }] }],
          },
        ],
      },
      {
        t: 'list',
        ordered: true,
        start: 3,
        items: [
          { children: [txt('셋째')], sub: [] },
          { children: [txt('넷째')], sub: [] },
        ],
      },
      p(txt('끝')),
    ]);
  });

  it('날짜·빈칸 없는 표시는 목록이 아니다', () => {
    expect(parseMarkdown('2024. 10. 23. 정모')).toEqual([p(txt('2024. 10. 23. 정모'))]);
    expect(parseMarkdown('-붙임\n*기울임*')).toEqual([p(txt('-붙임\n'), { t: 'italic', children: [txt('기울임')] })]);
  });

  it('블록 앞의 빈 줄은 화면에 남게 줄바꿈을 하나 더 둔다', () => {
    expect(parseMarkdown('글\n\n# 제목')).toEqual([p(txt('글\n\n')), { t: 'heading', level: 1, children: [txt('제목')] }]);
  });

  it('같은 표시가 아주 많아도 빨리 끝난다', () => {
    const t0 = Date.now();
    const n = inline('_a '.repeat(2600) + '*a '.repeat(600) + '[x](y '.repeat(200));
    expect(n.every((x) => x.t === 'text')).toBe(true);
    expect(Date.now() - t0).toBeLessThan(2000);
  });
});

describe('마크다운 평문(미리보기)', () => {
  it('서식 표시를 빼고 스포일러는 가린다', () => {
    expect(stripMarkdown(`# 제목\n**굵게** ||비밀|| [링크](https://a.b) <:gg:5> <@${USER}>\n> 인용\n- 항목\n\`\`\`\n코드\n\`\`\``)).toBe(
      `제목\n굵게 ${SPOILER_TEXT} 링크 :gg: @…\n인용\n항목\n코드`,
    );
  });

  it('잘린 미리보기는 닫히지 않은 두 글자 표시·코드를 끝에서 닫는다(한 글자 표시는 그대로)', () => {
    expect(stripMarkdown('**잘린 굵게', { lenient: true })).toBe('잘린 굵게');
    expect(stripMarkdown('**잘린 굵게')).toBe('**잘린 굵게');
    expect(stripMarkdown('앞\n```js\nconst a = 1', { lenient: true })).toBe('앞\nconst a = 1');
    expect(stripMarkdown('`잘린 코드', { lenient: true })).toBe('잘린 코드');
    expect(stripMarkdown('2*3 은 6', { lenient: true })).toBe('2*3 은 6');
  });

  it('한 줄 발췌(답장·신고)도 서식을 뺀다', () => {
    expect(excerpt('**공지**\n> ||스포|| 입니다')).toBe(`공지 ${SPOILER_TEXT} 입니다`);
    expect(excerpt('가'.repeat(100), 10)).toBe('가'.repeat(10) + '…');
  });
});

describe('마크다운: 정렬', () => {
  it(':::center … ::: 사이를 가운데로, 안의 블록도 그대로 푼다', () => {
    expect(parseMarkdown('앞\n:::center\n# 제목\n**굵게**\n:::\n뒤')).toEqual([
      p(txt('앞')),
      { t: 'align', align: 'center', children: [{ t: 'heading', level: 1, children: [txt('제목')] }, p({ t: 'bold', children: [txt('굵게')] })] },
      p(txt('뒤')),
    ]);
    expect(parseMarkdown(':::right\n오른쪽\n:::')).toEqual([{ t: 'align', align: 'right', children: [p(txt('오른쪽'))] }]);
  });

  it('닫는 줄이 없으면 글자 그대로, 잘린 미리보기는 끝에서 닫는다', () => {
    expect(parseMarkdown(':::center\n가운데')).toEqual([p(txt(':::center\n가운데'))]);
    expect(parseMarkdown(':::center\n가운데', { lenient: true })).toEqual([{ t: 'align', align: 'center', children: [p(txt('가운데'))] }]);
  });

  it('겹쳐 쓰지 않고, 코드 블록 안의 ::: 는 닫는 줄이 아니다', () => {
    expect(parseMarkdown(':::center\n```\n:::\n```\n끝\n:::')).toEqual([
      { t: 'align', align: 'center', children: [{ t: 'codeblock', lang: '', v: ':::' }, p(txt('끝'))] },
    ]);
    const b = parseMarkdown(':::center\n:::right\n안\n:::');
    expect(b).toEqual([{ t: 'align', align: 'center', children: [p(txt(':::right\n안'))] }]);
  });

  it('미리보기 평문에는 정렬 표시가 없다', () => {
    expect(stripMarkdown('a\n:::center\n가운데 ||비밀||\n:::\nb')).toBe(`a\n가운데 ${SPOILER_TEXT}\nb`);
  });
});
