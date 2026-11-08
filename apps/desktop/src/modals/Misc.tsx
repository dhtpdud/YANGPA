// 작은 모달: 확인, 이미지 크게 보기. (포럼 새 글은 포럼 화면 안의 작성 페이지: components/ForumCompose.tsx)

import { Flame } from 'lucide-react';
import { useEffect, useState } from 'react';
import { useMediaSave, useSaveAttachment } from '../components/Attachment';
import { Spinner } from '../components/common';
import { useBlobUrl } from '../hooks/media';
import { t } from '../i18n';
import { cx } from '../lib/misc';
import { useStore, type AttachmentRef } from '../store/store';
import { Modal } from './Modal';

export function ConfirmModal({
  title,
  body,
  danger,
  confirmLabel,
  onConfirm,
  onClose,
}: {
  title: string;
  body: string;
  danger?: boolean;
  confirmLabel?: string;
  onConfirm: () => void | Promise<void>;
  onClose: () => void;
}) {
  const [busy, setBusy] = useState(false);
  return (
    <Modal
      title={title}
      onClose={onClose}
      size="sm"
      footer={
        <>
          <button type="button" className="btn ghost" onClick={onClose}>
            {t('취소')}
          </button>
          <button
            type="button"
            className={cx('btn', danger ? 'danger' : 'primary')}
            disabled={busy}
            onClick={async () => {
              setBusy(true);
              try {
                await onConfirm();
              } finally {
                setBusy(false);
                onClose();
              }
            }}
          >
            {busy && <Spinner size={14} />} {confirmLabel ?? t('확인')}
          </button>
        </>
      }
    >
      <p className="pre">{body}</p>
    </Modal>
  );
}

/** 이미지 크게 보기. 휘발성이면 자기 blob: URL을 따로 만들고, 원본 메시지가 만료되면 스스로 닫힙니다. */
export function ImageModal({ att, onClose }: { att: AttachmentRef; onClose: () => void }) {
  // 채널 목록에 없는 곳(검색 결과)에서 연 일반 메시지 이미지는 그대로 둡니다. 휘발성은 목록에 있을 때만 보여 줍니다.
  const exists = useStore((s) => {
    const ch = s.channels[att.msgKey];
    return ch ? ch.messages.some((m) => m.id === att.messageId && !m.deleted) : !att.volatile;
  });
  const src = useBlobUrl(exists ? { space: att.space, blob: att.blob, keyFrom: att.keyFrom, mime: att.mime, volatile: att.volatile } : null);
  // 크게 본 이미지의 "다른 이름으로 저장"도 앱의 저장으로 잇습니다(첨부 카드와 같음).
  useMediaSave(src.url, useSaveAttachment(att.space, att.blob, att.keyFrom, att.name, att.volatile));
  // 메시지가 만료·삭제되면 바로 닫습니다.
  useEffect(() => {
    if (!exists) onClose();
  }, [exists, onClose]);
  if (!exists) return null;
  return (
    <Modal onClose={onClose} size="full" bare className="image-modal">
      <div className="image-view" onMouseDown={(e) => e.target === e.currentTarget && onClose()}>
        {src.url ? <img src={src.url} alt={att.name} decoding="async" draggable={false} /> : <Spinner size={32} />}
        <div className="image-caption">
          {att.volatile && (
            <span className="badge volatile">
              <Flame size={11} /> {t('휘발성')}
            </span>
          )}
          {att.name}
        </div>
      </div>
    </Modal>
  );
}
