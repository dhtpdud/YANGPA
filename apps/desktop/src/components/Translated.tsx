// AI 번역을 켠 크루의 본문과 짧은 글(채널·크루·역할 이름, 채널 설명, 포럼 제목·미리보기·태그). 번역되면 번역 아이콘과 원본 언어를
// 붙이고, 본문은 누르면 원문을, 짧은 글은 툴팁에 원문을 보입니다.

import { Languages } from 'lucide-react';
import { useState } from 'react';
import type { ChannelId, MemberView, SpaceId, SpaceView } from '../api/types';
import { useTranslated } from '../hooks/useTranslated';
import { t } from '../i18n';
import { trOn } from '../lib/aiTr';
import { findChannel, useStore } from '../store/store';
import { EmojiText } from './common';
import { MessageText } from './MessageText';

interface BodyProps {
  text: string;
  space: SpaceId;
  channel: ChannelId;
  members: Map<string, MemberView>;
  me: string | null;
  /** 내가 쓴 글은 ‘내가 쓴 글도 번역’을 켰을 때만 번역합니다. */
  mine?: boolean;
}

/** 메시지 본문. 번역이 꺼져 있으면 MessageText 그대로(공간 보기를 구독하지 않게 나눔). */
export function MessageBody(p: BodyProps) {
  const on = useStore((s) => (!p.mine || s.prefs.aiTr.mine) && trOn(s.prefs.aiTr, s.views[p.space], s.identity?.user));
  return on ? <TranslatedBody {...p} /> : <MessageText text={p.text} members={p.members} me={p.me} space={p.space} />;
}

function TranslatedBody({ text, space, channel, members, me }: BodyProps) {
  const view = useStore((s) => s.views[space]);
  const ch = useStore((s) => findChannel(s.views, space, channel));
  const tr = useTranslated(text, view, ch);
  const [original, setOriginal] = useState(false);
  if (!tr) return <MessageText text={text} members={members} me={me} space={space} />;
  return (
    <>
      <MessageText text={original ? text : tr.text} members={members} me={me} space={space} />
      <button type="button" className="tr-mark" title={original ? t('번역 보기') : t('원문 보기')} onClick={() => setOriginal(!original)}>
        <Languages size={12} />
        {tr.source ? t('{lang}에서 번역됨', { lang: tr.source }) : t('번역됨')}
      </button>
    </>
  );
}

/** 채널·크루 이름. ‘채널·크루 이름도 번역’을 켠 크루에서만 번역합니다(원문은 툴팁). */
export function TrName({ text, space }: { text: string; space: SpaceView | undefined }) {
  return <TrText text={text} space={space?.space} />;
}

interface TrTextProps {
  text: string;
  space: SpaceId | undefined;
  /** 있으면 그 채널의 내용(채널 권한·휘발성 규칙을 따름), 없으면 이름(‘채널·크루 이름도 번역’을 켰을 때만). */
  channel?: ChannelId;
  /** 커스텀 이모지 표기를 그림으로(역할 이름). */
  emoji?: boolean;
}

/** 짧은 글 한 줄. 번역이 꺼져 있으면 원문 그대로(공간 보기를 구독하지 않게 나눔). */
export function TrText(p: TrTextProps) {
  const on = useStore((s) => !!p.space && trOn(s.prefs.aiTr, s.views[p.space], s.identity?.user) && (!!p.channel || s.prefs.aiTr.names));
  return on ? <TranslatedText {...p} /> : p.emoji ? <EmojiText text={p.text} /> : <>{p.text}</>;
}

function TranslatedText({ text, space, channel, emoji }: TrTextProps) {
  const tr = useTr(text, space, channel);
  const shown = (s: string) => (emoji ? <EmojiText text={s} /> : <>{s}</>);
  if (!tr) return shown(text);
  const title = tr.source ? t('{lang}에서 번역됨: {text}', { lang: tr.source, text }) : t('번역됨: {text}', { text });
  return (
    <span title={title}>
      {shown(tr.text)}
      <Languages size={11} className="tr-name-icon" aria-label={title} />
    </span>
  );
}

function useTr(text: string, space: SpaceId | undefined, channel?: ChannelId) {
  const view = useStore((s) => (space ? s.views[space] : undefined));
  const ch = useStore((s) => (space && channel ? findChannel(s.views, space, channel) : undefined));
  return useTranslated(text, view, ch);
}

/** 문자열이 필요한 자리(입력창 안내·툴팁·문장 속 이름)에 쓰는 번역. 꺼져 있거나 아직이면 원문. */
export function useTrText(text: string, space: SpaceId | undefined, channel?: ChannelId): string {
  return useTr(text, space, channel)?.text ?? text;
}
