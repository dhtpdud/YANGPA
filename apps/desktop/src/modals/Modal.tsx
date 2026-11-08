// 모달 틀과 모달 라우터.

import { X } from 'lucide-react';
import { useEffect, useRef, type ReactNode } from 'react';
import { t } from '../i18n';
import { cx } from '../lib/misc';
import { useStore, type Modal as ModalState } from '../store/store';
import { ContentViewer } from '../supporter/ContentViewer';
import { AddFriendModal } from './AddFriend';
import { ChannelSettingsModal } from './ChannelSettings';
import { CrewSettingsModal } from './CrewSettings';
import { ConvertCrewModal, CreateCrewModal, JoinCrewModal } from './Crew';
import { DataMoveModal } from './DataDir';
import { DeviceLinkApproveModal } from './DeviceLink';
import { FriendProfileModal } from './FriendProfile';
import { ConfirmModal, ImageModal } from './Misc';
import { BanModal, ReportModal } from './Moderation';
import { SupporterModal } from '../supporter/SupporterModal';
import { RateLimitModal } from './RateLimits';
import { ScreenSourceModal } from './ScreenSource';
import { TrustWarningModal } from './Trust';
import { UserSettingsModal } from './UserSettings';

export function Modal({
  title,
  onClose,
  children,
  footer,
  size = 'md',
  className,
  bare,
}: {
  title?: ReactNode;
  onClose: () => void;
  children: ReactNode;
  footer?: ReactNode;
  size?: 'sm' | 'md' | 'lg' | 'full';
  className?: string;
  bare?: boolean;
}) {
  const ref = useRef<HTMLDivElement>(null);
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== 'Escape') return;
      // 겹친 창(신뢰 경고)이 떠 있으면 맨 위 창만 닫습니다.
      const overlays = document.querySelectorAll('.overlay');
      if (overlays.length > 0 && overlays[overlays.length - 1] !== ref.current?.parentElement) return;
      onClose();
    };
    document.addEventListener('keydown', onKey);
    const prev = document.activeElement as HTMLElement | null;
    ref.current?.querySelector<HTMLElement>('input, textarea, select, button:not(.modal-x)')?.focus();
    return () => {
      document.removeEventListener('keydown', onKey);
      prev?.focus?.();
    };
  }, [onClose]);
  return (
    <div className="overlay" onMouseDown={(e) => e.target === e.currentTarget && onClose()}>
      <div ref={ref} className={cx('modal', `modal-${size}`, bare && 'bare', className)} role="dialog" aria-modal>
        {!bare && (
          <div className="modal-head">
            <h2>{title}</h2>
            <button type="button" className="icon-btn modal-x" title={t('닫기')} onClick={onClose}>
              <X size={20} />
            </button>
          </div>
        )}
        <div className="modal-body">{children}</div>
        {footer && <div className="modal-foot">{footer}</div>}
      </div>
    </div>
  );
}

export function Field({ label, hint, children }: { label: ReactNode; hint?: ReactNode; children: ReactNode }) {
  return (
    <label className="field">
      <span className="field-label">{label}</span>
      {children}
      {hint && <span className="field-hint">{hint}</span>}
    </label>
  );
}

export function ModalRoot() {
  const modal = useStore((s) => s.modal);
  const close = useStore((s) => s.closeModal);
  const openModal = useStore((s) => s.openModal);
  // 새 기기 허용은 모달 자리와 따로 둡니다(다른 창이 열려도 사라지지 않게).
  const approve = useStore((s) => s.deviceLinkRequest !== null) && <DeviceLinkApproveModal />;
  if (!modal) return approve || null;
  // 신뢰 경고·확인 창은 연 창 위에 겹칩니다. 아래 창은 같은 자리에 그대로 그려 입력 중이던 내용·탭이 남습니다.
  const base = modal.kind === 'trustWarning' || modal.kind === 'confirm' ? modal.back : modal;
  // 확인 창을 닫을 때 그 사이 다른 창이 열렸으면(확인 동작이 연 창) 그대로 둡니다.
  const closeConfirm = () => {
    if (useStore.getState().modal !== modal || modal.kind !== 'confirm') return;
    if (modal.back) openModal(modal.back);
    else close();
  };
  return (
    <>
      {base && <ModalView modal={base} close={close} />}
      {modal.kind === 'confirm' && <ConfirmModal {...modal} onClose={closeConfirm} />}
      {modal.kind === 'trustWarning' && (
        <TrustWarningModal
          target={modal.target}
          onConfirm={modal.onConfirm}
          onCancel={modal.onCancel}
          onClose={() => (modal.back ? openModal(modal.back) : close())}
        />
      )}
      {approve}
    </>
  );
}

function ModalView({ modal, close }: { modal: ModalState; close: () => void }) {
  const openModal = useStore((s) => s.openModal);
  switch (modal.kind) {
    case 'addFriend':
      return <AddFriendModal key={modal.url} url={modal.url} onClose={close} />;
    case 'friendProfile':
      return <FriendProfileModal user={modal.user} tab={modal.tab} onClose={close} />;
    case 'createCrew':
      return <CreateCrewModal onClose={close} />;
    case 'joinCrew':
      return <JoinCrewModal key={modal.url} url={modal.url} onClose={close} />;
    case 'convertCrew':
      return <ConvertCrewModal space={modal.space} onClose={close} />;
    case 'channelCreate':
      return <ChannelSettingsModal space={modal.space} parent={modal.parent} initialKind={modal.channelKind} onClose={close} />;
    case 'channelSettings':
      return <ChannelSettingsModal space={modal.space} channel={modal.channel} onClose={close} />;
    case 'crewSettings':
      return <CrewSettingsModal space={modal.space} tab={modal.tab} onClose={close} />;
    case 'userSettings':
      return <UserSettingsModal tab={modal.tab} onClose={close} />;
    case 'image':
      return <ImageModal att={modal.att} onClose={close} />;
    case 'supporter':
      return <SupporterModal onClose={close} />;
    case 'viewer':
      return <ContentViewer space={modal.space} channel={modal.channel} onClose={close} />;
    case 'fileViewer':
      return <ContentViewer list={{ items: modal.items, start: modal.start }} onClose={close} />;
    case 'report':
      return <ReportModal space={modal.space} channel={modal.channel} message={modal.message} onClose={close} />;
    case 'ban':
      return <BanModal space={modal.space} user={modal.user} onClose={close} />;
    case 'confirm':
      // 확인 창은 ModalRoot가 위에 겹쳐 그립니다(아래 창으로 오지 않음).
      return null;
    case 'rateLimit': {
      // 프로필·설정에서 열었으면 닫을 때 그 창으로 돌아갑니다.
      const back = modal.back;
      return <RateLimitModal scope={modal.scope} onClose={back ? () => openModal(back) : close} />;
    }
    case 'trustWarning':
      // 경고 창은 ModalRoot가 위에 겹쳐 그립니다(아래 창으로 오지 않음).
      return null;
    case 'dataMove':
      return <DataMoveModal plan={modal.plan} onClose={close} />;
    case 'screenSource':
      return <ScreenSourceModal onPick={modal.onPick} onClose={close} />;
  }
}
