// 익명 채널 표시 규칙(순수 함수). 데몬이 익명 글·참가자의 사용자를 ANONYMOUS_USER로 가려 보내므로, 앱은 그 자리를
// 멤버 목록에서 찾지 않고 '익명'으로 그리며, 내 글인지는 `mine`으로만 압니다.

import { ANONYMOUS_USER, type ChannelView, type MessageView, type SpaceView, type UserId, type VoiceParticipant } from '../api/types';
import { t } from '../i18n';
import type { VoiceSnapshot } from '../voice/session';
import { canBeAnonymous } from './channel';
import { hasPerm } from './misc';

export const ANON_NAME = t('익명');

/** 켜기·끄기 스위치 아래 한 줄 설명. */
export const ANON_HINT = t("켜면 이 채널의 글, 답글, 통화 참가자가 '용감한 수달'처럼 사람마다 정해진 가명으로, 반응은 '익명'으로 보입니다. 꺼도 켜져 있는 동안 쓴 글은 익명으로 남습니다.");

/** 통화 채널의 익명을 끌 때 확인 창 본문. */
export const ANON_VOICE_OFF_WARNING = t('익명을 끄면 지금 통화 중인 모든 사람이 통화에서 나갑니다.');

/** 익명이 꺼져 데몬이 통화에서 나왔을 때 알림. */
export const ANON_CALL_ENDED = t('익명이 꺼져 통화에서 나왔습니다');

export function isAnonymousUser(user: UserId | null | undefined): boolean {
  return user === ANONYMOUS_USER;
}

export function isAnonymousMessage(m: Pick<MessageView, 'anonymous' | 'author'>): boolean {
  return !!m.anonymous || m.author === ANONYMOUS_USER;
}

/** 내 글인지. 익명 글은 글쓴이가 가려져 있어 `mine`만 믿고, `mine`을 보내지 않는 옛 데몬이면 작성자로 비교합니다. */
export function isMine(m: Pick<MessageView, 'mine' | 'author'>, me: UserId | null | undefined): boolean {
  return !!m.mine || (!!me && m.author === me && m.author !== ANONYMOUS_USER);
}

/** 이 공간·채널에 익명 스위치를 보여 줄지(데몬 규칙과 같음: 크루의, 카테고리가 아닌 채널, 채널 관리 권한). */
export function canSetAnonymous(view: SpaceView | undefined, kind: ChannelView['settings']['kind']): boolean {
  return !!view && canBeAnonymous(kind, view.kind) && hasPerm(view, 'manage_channels');
}

/** 익명 글의 이름: 데몬이 붙인 가명, 없으면(옛 데몬) '익명'. */
export function anonName(m: { anon_name?: string }): string {
  return m.anon_name || ANON_NAME;
}

/** 통화 참가자 이름: 익명 채널이면 가명(옛 데몬이면 '익명 N', 내 줄이면 뒤에 '(나)'). 익명이 아니면 null. */
export function anonParticipantLabel(p: Pick<VoiceParticipant, 'user' | 'anon' | 'anon_name' | 'mine'>, isMe = false): string | null {
  if (p.anon == null && p.user !== ANONYMOUS_USER) return null;
  const base = p.anon_name || (p.anon ? t('익명 {num}', { num: p.anon }) : ANON_NAME);
  return p.mine || isMe ? t('{name} (나)', { name: base }) : base;
}

/**
 * 참가자 목록 → 채널 보기의 `participants`. 실제 사용자는 기기가 여럿이어도 한 번만 세지만, 익명 참가자는 모두 같은
 * ANONYMOUS_USER라 하나로 합치면 수가 줄어 그대로 둡니다.
 */
export function participantUsers(list: Pick<VoiceParticipant, 'user'>[]): UserId[] {
  const seen = new Set<UserId>();
  const out: UserId[] = [];
  for (const p of list) {
    if (p.user === ANONYMOUS_USER) out.push(p.user);
    else if (!seen.has(p.user)) {
      seen.add(p.user);
      out.push(p.user);
    }
  }
  return out;
}

/** 익명으로 들어간 통화인지(참가 응답의 참가자에 번호가 있었음). */
export function joinedAnonymously(participants: Pick<VoiceParticipant, 'anon'>[]): boolean {
  return participants.some((p) => p.anon != null);
}

/**
 * 익명으로 들어간 통화가 익명이 꺼져 끝났는지. 데몬은 익명이 꺼지면 스스로 통화에서 나오므로 앱도 미디어를 멈춰야 합니다.
 * - `channel`: 데몬에서 막 읽은 채널 보기. 익명이 아니면 끝난 것입니다.
 * - `roster`: 새 참가자 목록. 익명 채널이면 늘 번호가 붙으므로, 사람이 있는데 번호가 하나도 없으면 꺼진 것입니다.
 *   (보기는 늦게 갱신될 수 있어 참가자 목록만으로도 판단합니다.)
 */
export function anonCallEnded(
  voice: Pick<VoiceSnapshot, 'anonymous'> | null | undefined,
  next: { channel?: Pick<ChannelView, 'anonymous'>; roster?: Pick<VoiceParticipant, 'anon'>[] },
): boolean {
  if (!voice?.anonymous) return false;
  if (next.channel && !next.channel.anonymous) return true;
  return !!next.roster && next.roster.length > 0 && next.roster.every((p) => p.anon == null);
}
