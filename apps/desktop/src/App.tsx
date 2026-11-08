// 앱 뼈대: 레일 | 사이드바 | 가운데 | 멤버 목록. 데몬 연결 상태·온보딩·토스트·모달.

import { AlertTriangle, CheckCircle2, CloudOff, Info, X, XCircle } from 'lucide-react';
import { useEffect, useState } from 'react';
import type { NetStatus } from './api/types';
import flameLogo from './assets/flame.png';
import { onWebviewSave } from './components/Attachment';
import { Spinner } from './components/common';
import { HomeView } from './components/HomeView';
import { LibraryView } from './supporter/LibraryView';
import { MemberList } from './components/MemberList';
import { PersonMenu } from './components/menus';
import { SpacePane } from './components/Pane';
import { Rail } from './components/Rail';
import { ScreenPip } from './components/ScreenPip';
import { ChannelSidebar, HomeSidebar } from './components/Sidebar';
import { t } from './i18n';
import { cx } from './lib/misc';
import { ModalRoot } from './modals/Modal';
import { Onboarding } from './modals/Onboarding';
import { useStore } from './store/store';

function Toasts() {
  const toasts = useStore((s) => s.toasts);
  const dismiss = useStore((s) => s.dismissToast);
  return (
    <div className="toasts" aria-live="polite">
      {toasts.map((toast) => (
        <div key={toast.id} className={`toast ${toast.kind}`}>
          {toast.kind === 'success' ? <CheckCircle2 size={16} /> : toast.kind === 'error' ? <XCircle size={16} /> : toast.kind === 'warn' ? <AlertTriangle size={16} /> : <Info size={16} />}
          <span>{toast.text}</span>
          <button type="button" className="icon-btn small" onClick={() => dismiss(toast.id)}>
            <X size={13} />
          </button>
        </div>
      ))}
    </div>
  );
}

/** 오프라인 모드(데몬이 망 없이 떠 있음): 늘 보이게 두고 바로 돌아갈 단추를 둡니다. */
function OfflineBanner() {
  const setPresence = useStore((s) => s.setPresence);
  const toast = useStore((s) => s.toast);
  return (
    <div className="daemon-banner offline">
      <CloudOff size={14} />
      {t('오프라인 모드: Tor 연결과 동기화가 꺼져 있습니다')}
      <button
        type="button"
        className="banner-btn"
        onClick={() => {
          toast('info', t('다시 연결합니다'));
          void setPresence({ mode: 'online' });
        }}
      >
        {t('다시 온라인')}
      </button>
    </div>
  );
}

function DaemonBanner() {
  const daemon = useStore((s) => s.daemon);
  const client = useStore((s) => s.client);
  const offline = useStore((s) => !!s.net?.offline);
  const kind = client?.kind;
  if (daemon.state === 'connected') {
    if (offline) return <OfflineBanner />;
    return kind === 'mock' ? <div className="mock-ribbon">MOCK 데이터(데몬 없이 실행 중)</div> : null; // i18n-ignore: mock 개발용 표시(출시본에 없음)
  }
  return (
    <div className={`daemon-banner ${daemon.state}`}>
      {daemon.state === 'error' ? <AlertTriangle size={14} /> : <Spinner size={12} />}
      {daemon.state === 'spawning' && t('백그라운드 데몬(yangpad)을 시작하는 중…')}
      {daemon.state === 'connecting' && t('데몬에 연결하는 중…')}
      {daemon.state === 'error' && t('데몬에 연결할 수 없습니다: {message}', { message: daemon.message })}
      {daemon.state === 'error' && (
        <button type="button" className="banner-btn" onClick={() => void client?.transport.retryDaemon()}>
          {t('다시 시도')}
        </button>
      )}
    </div>
  );
}

/** 인터넷이 없어 Tor가 준비되지 않아도 시작 화면에 갇히지 않게 하는 상한. */
const STARTUP_WAIT_MS = 60_000;

/** 시작 화면의 진행(0~100)과 문구. Tor가 준비됐으면(또는 Tor가 아니면) null. 친구 연결은 친구마다 '연결 중'으로 보입니다. */
export function startupProgress(net: NetStatus | null): { pct: number; text: string } | null {
  if (!net) return { pct: 0, text: t('네트워크 상태를 읽는 중…') };
  // 오프라인 모드는 Tor를 켜지 않으므로 기다릴 것이 없습니다.
  if (net.offline) return null;
  const boot = net.mode === 'tor' ? (net.bootstrap_progress ?? 0) : 100;
  return boot >= 100 ? null : { pct: boot, text: t('Tor 회로 만드는 중… {pct}%', { pct: boot }) };
}

function Splash({ progress, note, onSkip }: { progress: { pct: number; text: string } | null; note?: string; onSkip?: () => void }) {
  return (
    <div className="splash">
      <img className="ob-logo" src={flameLogo} alt="" />
      {progress ? (
        <div className="progress splash-progress" role="progressbar" aria-valuenow={progress.pct} aria-valuemin={0} aria-valuemax={100}>
          <div className="track">
            <div className="bar" style={{ width: `${progress.pct}%` }} />
          </div>
          <span>{progress.text}</span>
        </div>
      ) : (
        <Spinner size={24} />
      )}
      {note && <p className="splash-note">{note}</p>}
      {onSkip && (
        <button type="button" className="link-btn small" onClick={onSkip}>
          {t('기다리지 않고 열기')}
        </button>
      )}
      <DaemonBanner />
    </div>
  );
}

export function App() {
  const phase = useStore((s) => s.phase);
  const net = useStore((s) => s.net);
  const firstSyncing = useStore((s) => s.identity?.first_syncing === true && !s.firstSyncSkipped);
  // 한 번 열면 그 뒤 Tor가 다시 회로를 만들어도 시작 화면으로 돌아가지 않습니다.
  const [opened, setOpened] = useState(false);
  const selected = useStore((s) => s.selected);
  const view = useStore((s) => (s.selected.space === 'home' ? undefined : s.views[s.selected.space]));
  const theme = useStore((s) => s.prefs.theme);
  const memberList = useStore((s) => s.prefs.memberList);
  const library = useStore((s) => s.library);
  const mobilePane = useStore((s) => s.mobilePane);
  const libraryPreview = useStore((s) => s.prefs.library.preview);
  const boot = useStore((s) => s.boot);
  const client = useStore((s) => s.client);

  useEffect(() => {
    void boot();
  }, [boot]);

  useEffect(() => client?.transport.watchSaveRequests?.((req) => onWebviewSave(req, (text) => useStore.getState().toast('warn', text))), [client]);

  useEffect(
    () =>
      client?.transport.onBackButton?.(() => {
        // 열린 메뉴가 있으면 그것부터 닫습니다(Popover는 Escape로 닫힘).
        if (document.querySelector('.popover')) document.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape' }));
        else if (!useStore.getState().mobileBack()) client.transport.minimize?.();
      }),
    [client],
  );

  useEffect(() => {
    document.documentElement.dataset.theme = theme;
  }, [theme]);

  useEffect(() => {
    if (phase === 'ready' && !opened && !startupProgress(net)) setOpened(true);
  }, [phase, net, opened]);

  useEffect(() => {
    if (phase !== 'ready') return;
    const timer = setTimeout(() => setOpened(true), STARTUP_WAIT_MS);
    return () => clearTimeout(timer);
  }, [phase]);

  if (phase === 'loading') return <Splash progress={null} />;
  if (phase === 'ready' && !opened) return <Splash progress={startupProgress(net)} onSkip={() => setOpened(true)} />;
  if (phase === 'onboarding') {
    return (
      <>
        <Onboarding />
        <div className="banner-overlay">
          <DaemonBanner />
        </div>
        <Toasts />
      </>
    );
  }

  if (firstSyncing) {
    return (
      <>
        <Splash
          progress={null}
          note={t('계정을 동기화하는 중입니다. 잠시만 기다려 주세요.')}
          onSkip={() => useStore.setState({ firstSyncSkipped: true })}
        />
        <ModalRoot />
        <Toasts />
      </>
    );
  }

  const home = selected.space === 'home';
  const channel = view && selected.channel ? view.channels.find((c) => c.channel === selected.channel) : undefined;
  const showMembers = !home && view && (memberList || mobilePane === 'members') && channel?.settings.kind !== 'voice';
  return (
    <div className="app">
      <DaemonBanner />
      <div className={cx('layout', `m-${mobilePane}`, library ? libraryPreview && 'with-preview' : showMembers && 'with-members')}>
        <Rail />
        {library ? (
          <LibraryView />
        ) : home ? (
          <HomeSidebar />
        ) : view ? (
          <ChannelSidebar space={view} />
        ) : (
          <aside className="sidebar" />
        )}
        {library ? null : home ? (
          <HomeView />
        ) : view ? (
          <SpacePane space={view} />
        ) : (
          <main className="pane center">
            <Spinner size={24} />
          </main>
        )}
        {!library && showMembers && <MemberList space={view} />}
      </div>
      <PersonMenu />
      <ModalRoot />
      <Toasts />
      <ScreenPip />
    </div>
  );
}
