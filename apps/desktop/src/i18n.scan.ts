/// <reference types="node" />
// 번역 키 모으기(i18n.test.ts가 씀). 앱 코드의 t()/tn() 첫 인자와, 화면에 그대로 뜨는 데몬·앱 셸 오류 문구
// (`RpcError::종류("…")`·`CmdError::종류("…")`의 글자 그대로인 것)가 키입니다. t() 밖의 한글 글자도 찾아냅니다.

import { readdirSync, readFileSync } from 'node:fs';
import { dirname, join, relative } from 'node:path';
import { fileURLToPath } from 'node:url';
import ts from 'typescript';

const HANGUL = /[가-힣ㄱ-ㆎ]/;
const SRC = dirname(fileURLToPath(import.meta.url));
const REPO = join(SRC, '../../..');

/** 원문이 그대로 남아도 되는 파일: 데몬 없이 띄우는 개발용 가짜 데이터, 한국어 이모지 검색어. */
const RAW_OK = new Set(['api/mock.ts', 'api/mockData.ts', 'lib/emojiData.ts']);
/** 화면에 뜨는 오류 문구를 만드는 Rust 소스. */
const RUST_DIRS = ['crates/yangpa-daemon/src', 'crates/yangpa-rpc/src', 'apps/desktop/src-tauri/src'];

export interface Offender {
  file: string;
  line: number;
  text: string;
}

function files(dir: string, ext: RegExp): string[] {
  return readdirSync(dir, { withFileTypes: true }).flatMap((d) => {
    const p = join(dir, d.name);
    return d.isDirectory() ? files(p, ext) : ext.test(d.name) ? [p] : [];
  });
}

const isKeyCall = (n: ts.Node): n is ts.CallExpression =>
  ts.isCallExpression(n) && ts.isIdentifier(n.expression) && ['t', 'tn', 'tc'].includes(n.expression.text);
const isLiteral = (n: ts.Node | undefined): n is ts.StringLiteral | ts.NoSubstitutionTemplateLiteral =>
  !!n && (ts.isStringLiteral(n) || ts.isNoSubstitutionTemplateLiteral(n));

/** 앱 코드(시험·e2e 제외)의 키(→ 처음 쓰인 파일, 번역할 때 문맥)와, t() 밖 한글·글자 그대로가 아닌 t() 첫 인자. */
export function scanApp(): { keys: Map<string, string>; raw: Offender[] } {
  const keys = new Map<string, string>();
  const raw: Offender[] = [];
  for (const path of files(SRC, /\.tsx?$/)) {
    const rel = relative(SRC, path).replace(/\\/g, '/');
    if (/\.test\.tsx?$|^(e2e|test)\//.test(rel) || rel === 'i18n.scan.ts') continue;
    const text = readFileSync(path, 'utf8');
    const sf = ts.createSourceFile(rel, text, ts.ScriptTarget.Latest, true, rel.endsWith('x') ? ts.ScriptKind.TSX : ts.ScriptKind.TS);
    const lines = text.split('\n');
    const report = (n: ts.Node, s: string) => {
      const line = sf.getLineAndCharacterOfPosition(n.getStart()).line;
      if (RAW_OK.has(rel) || /i18n-ignore/.test(lines[line] + (lines[line - 1] ?? ''))) return;
      raw.push({ file: rel, line: line + 1, text: s.trim().slice(0, 80) });
    };
    const visit = (n: ts.Node): void => {
      if (isKeyCall(n)) {
        // tc(문맥, 원문)의 키는 `원문\u0004문맥`(i18n.ts).
        const ctx = (n.expression as ts.Identifier).text === 'tc' ? n.arguments[0] : undefined;
        const [first, ...rest] = ctx ? n.arguments.slice(1) : n.arguments;
        if (ctx && !isLiteral(ctx)) report(ctx, `tc()의 문맥은 글자 그대로여야 합니다: ${ctx.getText()}`);
        else if (isLiteral(first)) {
          const key = isLiteral(ctx) ? `${first.text}\u0004${ctx.text}` : first.text;
          if (!keys.has(key)) keys.set(key, rel);
        } else if (first) report(first, `t()의 첫 인자는 글자 그대로여야 합니다: ${first.getText()}`);
        rest.forEach(visit);
        return;
      }
      // 콘솔 로그는 화면에 뜨지 않습니다.
      if (ts.isCallExpression(n) && /^console\./.test(n.expression.getText())) return;
      if (ts.isImportDeclaration(n) || ts.isExportDeclaration(n)) return;
      if ((ts.isStringLiteral(n) || ts.isNoSubstitutionTemplateLiteral(n) || ts.isJsxText(n)) && HANGUL.test(n.text)) report(n, n.text);
      // 템플릿은 글자 부분만 봅니다(`${t('…')}` 안은 아래 자식 방문이 따로 봄).
      if (ts.isTemplateExpression(n) && [n.head, ...n.templateSpans.map((s) => s.literal)].some((p) => HANGUL.test(p.text))) report(n, n.getText());
      ts.forEachChild(n, visit);
    };
    visit(sf);
  }
  return { keys, raw };
}

/** 데몬·앱 셸 오류 문구 중 한글이 든 글자 그대로의 것(시험 모듈 제외). 값이 섞인 format!은 앱이 종류별 일반 문구로 바꿉니다. */
export function scanRust(): Set<string> {
  const keys = new Set<string>();
  const re = /\b(?:RpcError|CmdError)::\w+\(\s*"((?:[^"\\]|\\.)*)"/g;
  for (const dir of RUST_DIRS) {
    for (const path of files(join(REPO, dir), /\.rs$/)) {
      if (/(^|[\\/])(tests?|testutil)\.rs$/.test(path)) continue;
      const text = readFileSync(path, 'utf8').split(/#\[cfg\(test\)\]/)[0];
      for (const m of text.matchAll(re)) {
        // 줄 끝 `\`로 이은 Rust 문자열은 다음 줄 앞 공백까지 지웁니다.
        const s = JSON.parse(`"${m[1].replace(/\\\r?\n\s*/g, '')}"`) as string;
        if (HANGUL.test(s)) keys.add(s);
      }
    }
  }
  return keys;
}

/** `{이름}` 자리 모음(정렬). */
export function slots(s: string): string[] {
  return [...new Set([...s.matchAll(/\{(\w+)\}/g)].map((m) => m[1]))].sort();
}
