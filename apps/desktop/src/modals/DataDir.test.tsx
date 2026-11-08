// @vitest-environment happy-dom
// 데이터 폴더(윈도 설치판·포터블): 크기·예상 시간을 보여 주고, 새 폴더 상태에 따라 고를 수 있는 것만 보이며, 고른 대로 앱 쪽에 맡깁니다.

import { afterEach, describe, expect, it, vi } from 'vitest';
import type { DataDirInfo, DataMovePlan } from '../api/client';
import { useStore } from '../store/store';
import { button, cleanup, click, render } from '../test/dom';
import { DataDirSection, DataMoveModal, formatEta } from './DataDir';

const plan = (over: Partial<DataMovePlan> = {}): DataMovePlan => ({
  from: 'D:\\YANGPA\\Data',
  to: 'E:\\Yangpa',
  usage: { bytes: 600 * 1024 * 1024, files: 1234 },
  same_volume: false,
  estimate_secs: 12.4,
  target: 'missing',
  ...over,
});

function fakeDataDir(info: Partial<DataDirInfo> = {}) {
  return {
    info: vi.fn(async () => ({ changeable: true, dir: 'D:\\YANGPA\\Data', default_dir: 'D:\\YANGPA\\Data', usage: { bytes: 2048, files: 3 }, ...info })),
    pickFolder: vi.fn(async () => 'E:\\Yangpa'),
    plan: vi.fn(async () => plan()),
    change: vi.fn(async () => undefined),
    restart: vi.fn(async () => undefined),
    onProgress: vi.fn(() => () => undefined),
  };
}

afterEach(async () => {
  await cleanup();
  useStore.setState({ client: null, modal: null });
});

describe('데이터 폴더', () => {
  it('예상 시간 문구', () => {
    expect(formatEta(0.2)).toBe('1초 안');
    expect(formatEta(12.4)).toBe('약 13초');
    expect(formatEta(125)).toBe('약 2분 5초');
    expect(formatEta(7300)).toBe('약 2시간 1분');
  });

  it('바꿀 수 있으면 지금 폴더·크기를 보이고, 고른 폴더의 계획으로 확인 창을 연다', async () => {
    const dataDir = fakeDataDir();
    useStore.setState({ client: { transport: { dataDir } } as never });
    const { el } = await render(<DataDirSection />);
    await vi.waitFor(() => expect(el.textContent).toContain('D:\\YANGPA\\Data'));
    expect(el.textContent).toContain('2.0 KiB(파일 3개)');
    await click(button(el, '폴더 바꾸기'));
    await vi.waitFor(() => expect(useStore.getState().modal).toEqual({ kind: 'dataMove', plan: plan() }));
    expect(dataDir.plan).toHaveBeenCalledWith('E:\\Yangpa');
  });

  it('바꿀 수 없는 판이면 바꾸기 단추가 없다', async () => {
    useStore.setState({ client: { transport: { dataDir: fakeDataDir({ changeable: false }) } } as never });
    const { el } = await render(<DataDirSection />);
    await vi.waitFor(() => expect(el.textContent).toContain('이 판은'));
    expect([...el.querySelectorAll('button')].some((b) => b.textContent?.includes('폴더 바꾸기'))).toBe(false);
  });

  it('빈 새 폴더: 크기·예상 시간을 보이고 옮기기와 새로 시작을 고를 수 있다', async () => {
    const dataDir = fakeDataDir();
    useStore.setState({ client: { transport: { dataDir } } as never });
    const { el } = await render(<DataMoveModal plan={plan()} onClose={() => undefined} />);
    expect(el.ownerDocument.body.textContent).toContain('MiB(파일 1,234개)');
    expect(el.ownerDocument.body.textContent).toContain('약 13초(다른 드라이브로 복사한 뒤 원래 것을 지움)');
    await click(button(el.ownerDocument.body, '데이터 옮기기'));
    expect(dataDir.change).toHaveBeenCalledWith('E:\\Yangpa', 'move');
    await vi.waitFor(() => expect(el.ownerDocument.body.textContent).toContain('앱을 다시 시작합니다'));
  });

  it('양파 데이터가 있는 새 폴더는 옮기지 못하고 그 데이터를 쓸 수만 있다, 실패하면 다시 시작을 권한다', async () => {
    const dataDir = fakeDataDir();
    dataDir.change.mockRejectedValueOnce(new Error('잠금 실패'));
    useStore.setState({ client: { transport: { dataDir } } as never });
    const { el } = await render(<DataMoveModal plan={plan({ target: 'yangpa' })} onClose={() => undefined} />);
    const body = el.ownerDocument.body;
    expect([...body.querySelectorAll('button')].some((b) => b.textContent?.includes('데이터 옮기기'))).toBe(false);
    await click(button(body, '그 폴더의 데이터 쓰기'));
    expect(dataDir.change).toHaveBeenCalledWith('E:\\Yangpa', 'switch');
    await vi.waitFor(() => expect(body.textContent).toContain('잠금 실패'));
    await click(button(body, '앱 다시 시작'));
    expect(dataDir.restart).toHaveBeenCalled();
  });

  it('예전 판 위치를 그대로 쓰면 그 경로를 보이고 기본 폴더로 바꿀 수 있다', async () => {
    const old = 'C:\\Users\\a\\AppData\\Roaming\\yangpa\\data';
    const dataDir = fakeDataDir({ dir: old });
    useStore.setState({ client: { transport: { dataDir } } as never });
    const { el } = await render(<DataDirSection />);
    await vi.waitFor(() => expect(el.textContent).toContain(old));
    expect(el.textContent).toContain('기본 폴더: D:\\YANGPA\\Data');
    await click(button(el, '기본 폴더로'));
    expect(dataDir.plan).toHaveBeenCalledWith('D:\\YANGPA\\Data');
    expect(dataDir.pickFolder).not.toHaveBeenCalled();
  });
});
