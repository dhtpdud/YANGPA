// 메시지 본문: 마크다운 노드(lib/markdown.ts)를 React 요소로. HTML을 직접 넣지 않으므로 본문은 항상 이스케이프됩니다.
// 링크(일반·가린 링크)는 http(s)만 기본 브라우저로 엽니다. 가린 링크는 실제 주소를 툴팁으로 보입니다.

import { memo, useMemo, useState, type ReactNode } from 'react';
import type { MemberView } from '../api/types';
import { t } from '../i18n';
import { ANON_NAME, isAnonymousUser } from '../lib/anon';
import { isEmojiOnly } from '../lib/emoji';
import { parseMarkdown, type Block, type Inline } from '../lib/markdown';
import { cx, displayName } from '../lib/misc';
import { isSafeHttpUrl } from '../lib/text';
import { useStore } from '../store/store';
import { CustomEmoji } from './common';

interface RenderCtx {
  members: Map<string, MemberView>;
  me: string | null;
  jumbo: boolean;
  openProfile: (user: string) => void;
  openLink: (href: string) => void;
}

/** 스포일러: 누르기 전에는 내용을 가리고(안의 링크·멘션도 누를 수 없음), 누르면 그 자리에서 보입니다. */
function Spoiler({ children }: { children: ReactNode }) {
  const [shown, setShown] = useState(false);
  if (shown) return <span className="spoiler shown">{children}</span>;
  const reveal = (e: { preventDefault(): void; stopPropagation(): void }) => {
    e.preventDefault();
    e.stopPropagation();
    setShown(true);
  };
  return (
    <span
      className="spoiler"
      role="button"
      tabIndex={0}
      title={t('스포일러: 눌러서 보기')}
      onClick={reveal}
      onKeyDown={(e) => {
        if (e.key === 'Enter' || e.key === ' ') reveal(e);
      }}
    >
      <span className="spoiler-inner" aria-hidden>
        {children}
      </span>
    </span>
  );
}

function linkProps(href: string, ctx: RenderCtx) {
  return {
    href,
    rel: 'noreferrer noopener',
    target: '_blank',
    onClick: (e: { preventDefault(): void }) => {
      // 웹뷰 안에서 이동하지 않고 기본 브라우저로 엽니다.
      e.preventDefault();
      ctx.openLink(href);
    },
  };
}

function renderInline(nodes: Inline[], ctx: RenderCtx): ReactNode[] {
  return nodes.map((n, i) => {
    switch (n.t) {
      case 'text':
        return <span key={i}>{n.v}</span>;
      case 'emoji':
        return <CustomEmoji key={i} id={n.id} name={n.name} animated={n.animated} size={ctx.jumbo ? 48 : 22} />;
      case 'mention': {
        // 익명 자리를 가리키는 멘션은 누구의 프로필도 열지 않습니다.
        if (isAnonymousUser(n.user)) {
          return (
            <span key={i} className="mention">
              @{ANON_NAME}
            </span>
          );
        }
        const m = ctx.members.get(n.user);
        return (
          <button key={i} type="button" className={n.user === ctx.me ? 'mention me' : 'mention'} onClick={() => ctx.openProfile(n.user)}>
            @{m ? displayName(m) : t('알 수 없는 사용자')}
          </button>
        );
      }
      case 'everyone':
        return (
          <span key={i} className="mention" title={n.v === 'everyone' ? t('채널을 볼 수 있는 모두') : t('지금 접속한 모두')}>
            @{n.v}
          </span>
        );
      case 'url':
        return isSafeHttpUrl(n.href) ? (
          <a key={i} {...linkProps(n.href, ctx)}>
            {n.href}
          </a>
        ) : (
          <span key={i}>{n.href}</span>
        );
      case 'link':
        // 파서가 http(s)만 가린 링크로 만들지만, 그릴 때도 한 번 더 확인합니다.
        return isSafeHttpUrl(n.href) ? (
          <a key={i} className="md-link" title={n.href} {...linkProps(n.href, ctx)}>
            {renderInline(n.children, ctx)}
          </a>
        ) : (
          <span key={i}>{renderInline(n.children, ctx)}</span>
        );
      case 'code':
        return <code key={i}>{n.v}</code>;
      case 'bold':
        return <strong key={i}>{renderInline(n.children, ctx)}</strong>;
      case 'italic':
        return <em key={i}>{renderInline(n.children, ctx)}</em>;
      case 'underline':
        return <u key={i}>{renderInline(n.children, ctx)}</u>;
      case 'strike':
        return <s key={i}>{renderInline(n.children, ctx)}</s>;
      case 'spoiler':
        return <Spoiler key={i}>{renderInline(n.children, ctx)}</Spoiler>;
    }
  });
}

function renderBlocks(blocks: Block[], ctx: RenderCtx): ReactNode[] {
  return blocks.map((b, i) => {
    switch (b.t) {
      case 'p':
        // 문단은 인라인으로 두어 한 줄짜리 메시지 뒤에 ‘(수정됨)’이 이어지게 합니다.
        return <span key={i}>{renderInline(b.children, ctx)}</span>;
      case 'heading': {
        const H = (['h1', 'h2', 'h3'] as const)[b.level - 1];
        return (
          <H key={i} className={`md-h md-h${b.level}`}>
            {renderInline(b.children, ctx)}
          </H>
        );
      }
      case 'subtext':
        return (
          <div key={i} className="md-sub">
            {renderInline(b.children, ctx)}
          </div>
        );
      case 'quote':
        return (
          <blockquote key={i} className="md-quote">
            {renderBlocks(b.children, ctx)}
          </blockquote>
        );
      case 'list': {
        const items = b.items.map((it, j) => (
          <li key={j}>
            {renderInline(it.children, ctx)}
            {renderBlocks(it.sub, ctx)}
          </li>
        ));
        return b.ordered ? (
          <ol key={i} className="md-list" start={b.start}>
            {items}
          </ol>
        ) : (
          <ul key={i} className="md-list">
            {items}
          </ul>
        );
      }
      case 'align':
        return (
          <div key={i} className={`md-align md-align-${b.align}`}>
            {renderBlocks(b.children, ctx)}
          </div>
        );
      case 'codeblock':
        return (
          <pre key={i} data-lang={b.lang || undefined}>
            <code>{b.v}</code>
          </pre>
        );
    }
  });
}

export const MessageText = memo(function MessageText({
  text,
  members,
  me,
  className,
}: {
  text: string;
  members: Map<string, MemberView>;
  me: string | null;
  className?: string;
}) {
  const blocks = useMemo(() => parseMarkdown(text), [text]);
  const jumbo = useMemo(() => isEmojiOnly(text), [text]);
  const openModal = useStore((s) => s.openModal);
  const client = useStore((s) => s.client);
  const openAppLink = useStore((s) => s.openAppLink);
  const ctx: RenderCtx = {
    members,
    me,
    jumbo,
    openProfile: (user) => openModal({ kind: 'friendProfile', user }),
    // 친구·초대 링크는 브라우저(웹 초대 페이지)를 거치지 않고 앱 창으로 바로 엽니다.
    openLink: (href) => void (openAppLink(href) || client?.transport.openExternal(href)),
  };
  return <div className={cx('msg-text', jumbo && 'jumbo', className)}>{renderBlocks(blocks, ctx)}</div>;
});
