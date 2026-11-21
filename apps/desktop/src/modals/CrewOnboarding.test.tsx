// @vitest-environment happy-dom
// 크루 온보딩: 저절로 띄울지 고르는 규칙, 지금 역할로 미리 채운 답, 필수 질문을 답해야 마치고 마치면 이 기기에 기억합니다.

import { act } from 'react';
import { afterEach, describe, expect, it, vi } from 'vitest';
import type { ApiClient } from '../api/client';
import type { MemberView, Onboarding, SpaceView } from '../api/types';
import { EVERYONE_ROLE } from '../api/types';
import { useStore } from '../store/store';
import { button, cleanup, click, render } from '../test/dom';
import { CrewOnboardingModal, currentAnswers, localizeOnboarding, needsOnboarding, OnboardingSettings, onboardingComplete } from './CrewOnboarding';

const SPACE = 'S'.repeat(32);

const member = (user: string, roles: string[]): MemberView => ({
  user,
  profile: { version: 1, name: user, bio: '', avatar: null, banner: null, theme_color: null, status: '' },
  roles,
  online: true,
  automation: false,
});

const OB: Onboarding = {
  welcome: '어서 오세요',
  questions: [
    {
      title: '무엇을 하나요?',
      multiple: true,
      required: true,
      options: [
        { label: '그림', description: '그림 채널', roles: ['art'] },
        { label: '음악', description: '', roles: ['music'] },
      ],
    },
    { title: '어디 사나요?', multiple: false, required: false, options: [{ label: '서울', description: '', roles: ['seoul'] }] },
  ],
};

function view(over: Partial<SpaceView> = {}): SpaceView {
  return {
    space: SPACE,
    kind: 'crew',
    settings: { name: '모임', description: '', icon: null, allow_external_emoji: false },
    owner: 'owner',
    members: [member('owner', []), member('me', ['other'])],
    roles: [EVERYONE_ROLE, 'art', 'music', 'seoul', 'other'].map((id, i) => ({ id, name: id, color: 0, rank: i, permissions: 0, automation: false })),
    channels: [],
    emojis: [],
    my_permissions: [],
    onboarding: OB,
    ...over,
  } as unknown as SpaceView;
}

afterEach(async () => {
  await cleanup();
  useStore.setState({ client: null, views: {}, modal: null, me: null });
});

describe('규칙', () => {
  it('질문이 있고 선택지 역할이 없으며 이 기기에서 마치지 않은 멤버에게만 띄운다', () => {
    expect(needsOnboarding(view(), 'me', [])).toBe(true);
    expect(needsOnboarding(view(), 'me', [SPACE])).toBe(false);
    expect(needsOnboarding(view({ my_permissions: ['manage_space'] }), 'me', [])).toBe(false);
    expect(needsOnboarding(view({ members: [member('me', ['music'])] }), 'me', [])).toBe(false);
    expect(needsOnboarding(view({ onboarding: { welcome: '안녕', questions: [] } }), 'me', [])).toBe(false);
  });

  it('지금 역할로 답을 채우고, 빈 칸이나 역할 없는 선택지는 저장하지 못한다', () => {
    expect(currentAnswers(OB, ['music', 'seoul'])).toEqual([[1], [0]]);
    expect(onboardingComplete(OB)).toBe(true);
    expect(onboardingComplete({ welcome: '', questions: [{ ...OB.questions[1], options: [{ label: '서울', description: '', roles: [] }] }] })).toBe(false);
    expect(onboardingComplete({ welcome: '', questions: [{ ...OB.questions[1], title: ' ' }] })).toBe(false);
  });
});

describe('답하기 창', () => {
  it('필수 질문을 고르기 전에는 마칠 수 없고, 마치면 답을 보내고 이 기기에 기억한다', async () => {
    const call = vi.fn(async () => ({}));
    const savePrefs = vi.fn(async () => undefined);
    useStore.setState({
      client: { call, transport: { savePrefs } } as unknown as ApiClient,
      me: 'me',
      views: { [SPACE]: view() },
      modal: null,
      toasts: [],
      prefs: { ...useStore.getState().prefs, onboarded: [] },
    });
    const onClose = vi.fn();
    const { el } = await render(<CrewOnboardingModal space={SPACE} onClose={onClose} />);
    expect(el.textContent).toContain('어서 오세요');
    expect(button(el, '마치기').disabled).toBe(true);
    await click(button(el, '그림'));
    await click(button(el, '음악'));
    await click(button(el, '서울'));
    expect(button(el, '마치기').disabled).toBe(false);
    await click(button(el, '마치기'));
    expect(call).toHaveBeenCalledWith('crew.onboarding_pick', { space: SPACE, answers: [[0, 1], [0]] });
    expect(useStore.getState().prefs.onboarded).toEqual([SPACE]);
    expect(onClose).toHaveBeenCalled();
  });
});

async function typeInto(input: HTMLInputElement, value: string) {
  await act(async () => {
    Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value')!.set!.call(input, value);
    input.dispatchEvent(new Event('input', { bubbles: true }));
  });
}

async function choose(select: HTMLSelectElement, value: string) {
  await act(async () => {
    Object.getOwnPropertyDescriptor(HTMLSelectElement.prototype, 'value')!.set!.call(select, value);
    select.dispatchEvent(new Event('change', { bubbles: true }));
  });
}

describe('설정 탭', () => {
  it('답변 타일을 누르면 편집 창이 뜨고, 고친 이름·역할이 저장할 설정에 들어간다', async () => {
    const call = vi.fn(async () => ({}));
    useStore.setState({
      client: { call } as unknown as ApiClient,
      me: 'owner',
      views: { [SPACE]: view({ my_permissions: ['manage_space', 'manage_roles'] }) },
      modal: null,
      toasts: [],
    });
    const { el } = await render(<OnboardingSettings space={SPACE} />);
    expect(el.querySelectorAll('.co-qcard')).toHaveLength(2);
    await click(button(el, '그림'));
    const editor = document.querySelector<HTMLElement>('.co-option-editor')!;
    await typeInto(editor.querySelector<HTMLInputElement>('input[aria-label="선택지 이름"]')!, '그림 그리기');
    await choose(editor.querySelector<HTMLSelectElement>('select')!, 'seoul');
    await click(button(editor, '저장'));
    expect(document.querySelector('.co-option-editor')).toBeNull();
    expect(el.textContent).toContain('그림 그리기');

    // 새 답변은 이름과 역할이 있어야 넣을 수 있다.
    await click(button(el.querySelectorAll<HTMLElement>('.co-qcard')[1], '답변 추가하기'));
    const add = document.querySelector<HTMLElement>('.co-option-editor')!;
    expect(button(add, '저장').disabled).toBe(true);
    await typeInto(add.querySelector<HTMLInputElement>('input[aria-label="선택지 이름"]')!, '부산');
    await choose(add.querySelector<HTMLSelectElement>('select')!, 'other');
    await click(button(add, '저장'));

    await click(button(el.querySelector<HTMLElement>('.save-bar')!, '저장'));
    const sent = (call.mock.calls[0] as unknown as [string, { onboarding: Onboarding }])[1].onboarding;
    expect(sent.questions[0].options[0]).toEqual({ label: '그림 그리기', description: '그림 채널', roles: ['art', 'seoul'] });
    expect(sent.questions[1].options.map((o) => o.label)).toEqual(['서울', '부산']);
  });
});

describe('언어별 온보딩', () => {
  const EN = { '어서 오세요': 'Welcome', '그림': 'Drawing', '그림 채널': 'Drawing channel' };

  it('번역이 있는 칸만 바꾸고 나머지는 원문, 역할·순서는 그대로', () => {
    const shown = localizeOnboarding(OB, EN);
    expect(shown.welcome).toBe('Welcome');
    expect(shown.questions[0].title).toBe('무엇을 하나요?');
    expect(shown.questions[0].options[0]).toEqual({ label: 'Drawing', description: 'Drawing channel', roles: ['art'] });
    expect(shown.questions[0].options[1].label).toBe('음악');
    expect(localizeOnboarding(OB, { constructor: 'x' }).welcome).toBe('어서 오세요');
    expect(localizeOnboarding(OB, undefined)).toBe(OB);
  });

  it('답하기 창은 고른 언어의 번역을 보이고 답 번호는 같다', async () => {
    const call = vi.fn(async () => ({}));
    useStore.setState({
      client: { call, transport: { savePrefs: vi.fn(async () => undefined) } } as unknown as ApiClient,
      me: 'me',
      views: { [SPACE]: view({ onboarding_langs: { en: EN } }) },
      modal: null,
      toasts: [],
      prefs: { ...useStore.getState().prefs, onboarded: [] },
    });
    const { el } = await render(<CrewOnboardingModal space={SPACE} lang="en" onClose={vi.fn()} />);
    expect(el.textContent).toContain('Welcome');
    await click(button(el, 'Drawing'));
    await click(button(el, '마치기'));
    expect(call).toHaveBeenCalledWith('crew.onboarding_pick', { space: SPACE, answers: [[0], []] });
  });

  it('설정에서 언어를 고르면 원문 칸마다 번역을 적고, 빈 칸·원문과 같은 칸은 빼고 저장한다', async () => {
    const call = vi.fn(async () => ({}));
    useStore.setState({
      client: { call } as unknown as ApiClient,
      me: 'owner',
      views: { [SPACE]: view({ my_permissions: ['manage_space', 'manage_roles'], onboarding_langs: { en: { '어서 오세요': 'Welcome' } } }) },
      modal: null,
      toasts: [],
    });
    const { el } = await render(<OnboardingSettings space={SPACE} />);
    await choose(el.querySelector<HTMLSelectElement>('select[aria-label="편집할 언어"]')!, 'en');
    // 원문 편집(답변 타일)은 숨고, 원문마다 번역 칸이 보입니다.
    expect(el.querySelector('.co-option')).toBeNull();
    expect(el.querySelector<HTMLTextAreaElement>('textarea[aria-label="어서 오세요"]')!.value).toBe('Welcome');
    await typeInto(el.querySelector<HTMLInputElement>('input[aria-label="그림"]')!, ' Drawing ');
    await typeInto(el.querySelector<HTMLInputElement>('input[aria-label="음악"]')!, '음악');
    await click(button(el.querySelector<HTMLElement>('.save-bar')!, '저장'));
    expect(call).toHaveBeenCalledWith('crew.set_onboarding_lang', { space: SPACE, lang: 'en', texts: { '어서 오세요': 'Welcome', 그림: 'Drawing' } });
  });
});
