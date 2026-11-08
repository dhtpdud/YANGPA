// 모더레이션: 신고 보내기, 밴, 크루 설정의 모더레이션 탭(운영 한계 안내, 신고 목록, 밴 목록).
// 신고 사유·처리 메모는 화면 상태에만 두고(데몬이 저장), 웹뷰 저장소에는 쓰지 않습니다.

import { Ban, Check, Flag, Info, MessageSquare, RefreshCw, UserX } from 'lucide-react';
import { useCallback, useEffect, useState } from 'react';
import { RpcError } from '../api/client';
import { ANONYMOUS_USER, ErrorCode, type BanView, type MessageView, type ReportAction, type ReportStatus, type ReportView, type SpaceView } from '../api/types';
import { Avatar, Spinner } from '../components/common';
import { t, tn } from '../i18n';
import { ANON_NAME, isAnonymousMessage } from '../lib/anon';
import { formatFull, formatRelative } from '../lib/format';
import { REPORT_ACTION_LABELS, cx, displayName, excerpt, formatResolution, hasPerm, outranks } from '../lib/misc';
import { chKey, useStore } from '../store/store';
import { Field, Modal } from './Modal';

/** 신고 사유·밴 사유·처리 메모 최대 글자 수(데몬 MAX_REASON_CHARS, MAX_NOTE_CHARS). */
export const MAX_REASON_CHARS = 512;

export function ReportModal({ space, channel, message, onClose }: { space: string; channel: string; message: MessageView; onClose: () => void }) {
  const view = useStore((s) => s.views[space]);
  const client = useStore((s) => s.client);
  const call = useStore((s) => s.call);
  const hideMessage = useStore((s) => s.hideMessage);
  const blockUser = useStore((s) => s.blockUser);
  const canBlock = useStore((s) => s.blocked !== null);
  const [reason, setReason] = useState('');
  const [hide, setHide] = useState(true);
  const [busy, setBusy] = useState(false);
  // 익명 글은 글쓴이를 모르므로 이름 대신 '익명'으로 보이고 차단할 사람도 없습니다.
  const anon = isAnonymousMessage(message);
  const author = anon ? undefined : view?.members.find((m) => m.user === message.author);
  const name = anon ? ANON_NAME : displayName(author, t('알 수 없는 사용자'));
  const submit = async () => {
    if (!client || !reason.trim()) return;
    setBusy(true);
    const r = await call(client.call('message.report', { space, channel, message: message.id, reason: reason.trim(), hide }), {
      ok: t('신고를 보냈습니다. 이 크루 운영진만 볼 수 있습니다.'),
    });
    setBusy(false);
    if (!r) return;
    if (hide) hideMessage(space, channel, message.id);
    onClose();
  };
  return (
    <Modal
      title={t('메시지 신고')}
      onClose={onClose}
      size="sm"
      footer={
        <>
          {canBlock && !anon && (
            <button
              type="button"
              className="btn ghost"
              onClick={() => {
                onClose();
                blockUser(message.author, name);
              }}
            >
              {t('대신 차단하기')}
            </button>
          )}
          <button type="button" className="btn ghost" onClick={onClose}>
            {t('취소')}
          </button>
          <button type="button" className="btn danger" disabled={busy || !reason.trim()} onClick={() => void submit()}>
            {busy && <Spinner size={14} />} <Flag size={14} /> {t('신고')}
          </button>
        </>
      }
    >
      <p className="muted small">
        {t('{name} 님의 메시지를 「{crew}」 운영진(추방·밴·메시지 관리 권한)에게 신고합니다. 신고는 운영진 기기에만 암호화해 보내고, 신고당한 사람과 다른 멤버에게는 보내지 않습니다.', {
          name,
          crew: view?.settings.name ?? t('크루'),
        })}
      </p>
      <blockquote className="report-quote">{message.deleted ? t('삭제된 메시지') : excerpt(message.text, 160) || t('첨부 파일')}</blockquote>
      <Field label={t('사유')} hint={`${reason.length}/${MAX_REASON_CHARS}`}>
        <textarea
          className="input"
          rows={3}
          maxLength={MAX_REASON_CHARS}
          value={reason}
          autoFocus
          placeholder={t('예: 스팸 링크, 괴롭힘')}
          onChange={(e) => setReason(e.target.value)}
        />
      </Field>
      <label className="check">
        <input type="checkbox" checked={hide} onChange={(e) => setHide(e.target.checked)} /> {t('이 기기에서 이 메시지 숨기기')}
      </label>
      <p className="muted small">{t('운영진이 모두 오프라인이면 켜질 때까지 내 기기가 다시 보냅니다. 처리 결과는 알려 주지 않습니다.')}</p>
    </Modal>
  );
}

export function BanModal({ space, user, onClose }: { space: string; user: string; onClose: () => void }) {
  const view = useStore((s) => s.views[space]);
  const client = useStore((s) => s.client);
  const call = useStore((s) => s.call);
  const member = view?.members.find((m) => m.user === user);
  const name = displayName(member, t('알 수 없는 사용자'));
  const [reason, setReason] = useState('');
  const [deleteRecent, setDeleteRecent] = useState(false);
  const [busy, setBusy] = useState(false);
  const submit = async () => {
    if (!client) return;
    setBusy(true);
    const r = await call(client.call('member.ban', { space, user, reason: reason.trim() || undefined, delete_recent: deleteRecent }), {
      ok: t('{name} 님을 밴했습니다', { name }),
    });
    setBusy(false);
    if (r !== undefined) onClose();
  };
  return (
    <Modal
      title={t('{name} 밴', { name })}
      onClose={onClose}
      size="sm"
      footer={
        <>
          <button type="button" className="btn ghost" onClick={onClose}>
            {t('취소')}
          </button>
          <button type="button" className="btn danger" disabled={busy} onClick={() => void submit()}>
            {busy && <Spinner size={14} />} <Ban size={14} /> {t('밴')}
          </button>
        </>
      }
    >
      <p className="muted small">
        {t('크루에서 내보내고(크루 키를 바꿈) 같은 신원으로는 다시 들어오지 못하게 합니다. 밴 목록은 서명된 크루 상태라 모든 멤버 기기가 같이 지킵니다. 새 신원을 만들어 오는 것은 막을 수 없습니다.')}
      </p>
      <Field label={t('사유(선택)')} hint={t('크루 멤버 모두가 볼 수 있는 기록에 남습니다.')}>
        <textarea className="input" rows={2} maxLength={MAX_REASON_CHARS} value={reason} onChange={(e) => setReason(e.target.value)} />
      </Field>
      <label className="check">
        <input type="checkbox" checked={deleteRecent} onChange={(e) => setDeleteRecent(e.target.checked)} /> {t('최근 24시간 메시지도 지우기')}
      </label>
    </Modal>
  );
}

/** 크루 설정 → 모더레이션. */
export function ModerationPanel({ space }: { space: string }) {
  const view = useStore((s) => s.views[space]);
  const isMod = hasPerm(view, 'kick') || hasPerm(view, 'ban') || hasPerm(view, 'manage_messages');
  const canBan = hasPerm(view, 'ban');
  // 이 화면을 열면 새 신고 표시를 지웁니다.
  const unseen = useStore((s) => s.reportsNew[space] ?? 0);
  useEffect(() => {
    if (unseen > 0) useStore.setState((s) => ({ reportsNew: { ...s.reportsNew, [space]: 0 } }));
  }, [space, unseen]);
  return (
    <div className="stack">
      <ModerationLimits />
      {isMod ? <ReportQueue space={space} /> : <p className="muted small">{t('신고 목록은 운영진(추방·밴·메시지 관리 권한)만 봅니다.')}</p>}
      {canBan && <BanList space={space} />}
    </div>
  );
}

function ModerationLimits() {
  return (
    <div className="card mod-limits stack">
      <h3>
        <Info size={16} /> {t('모더레이션의 한계(꼭 읽어 주세요)')}
      </h3>
      <p className="muted small">{t('서버가 없는 구조라서, 운영진이 할 수 있는 일과 없는 일이 중앙 서버 메신저와 다릅니다.')}</p>
      <div className="grid2">
        <div>
          <div className="small strong">{t('할 수 있는 것')}</div>
          <ul className="small">
            <li>{t('메시지 삭제: 삭제 이벤트를 퍼뜨려 정상 클라이언트 화면에서 지웁니다.')}</li>
            <li>{t('추방: 크루 키를 바꿔(키 갱신) 앞으로의 메시지를 읽지 못하게 합니다. 키 갱신이 늦으면 그 사이 글은 읽힐 수 있어 ‘엄격한 키 회전’으로 막습니다.')}</li>
            <li>{t('밴: 서명된 크루 상태라 모든 멤버 기기가 같은 신원의 재가입을 거부합니다.')}</li>
            <li>{t('신고 받기: 신고는 운영진 기기에만 암호화해 오고, 신고당한 사람은 모릅니다.')}</li>
            <li>{t('AI·자동화 계정의 파괴적 작업은 사람이 확인해야 실행됩니다.')}</li>
          </ul>
        </div>
        <div>
          <div className="small strong">{t('할 수 없는 것')}</div>
          <ul className="small">
            <li>{t('이미 받아 간 사본 지우기: 수정된 클라이언트·백업·스크린샷에 남은 것은 지울 수 없습니다.')}</li>
            <li>{t('추방·밴 전 기록 회수: 그전에 받은 기록은 상대 기기에 남습니다.')}</li>
            <li>{t('새 신원 막기: 밴은 신원 단위입니다. 초대 링크를 1회용·만료로 만들어 관리하세요.')}</li>
            <li>{t('신고 내용 확인: 운영진은 자기 기기에 있는 메시지 사본으로만 판단합니다. 없으면 기각만 할 수 있습니다.')}</li>
            <li>{t('휘발성 채널 강제: 화면 촬영·수정된 클라이언트는 막을 수 없습니다. 휘발성 채널 신고는 메모리에만 둡니다.')}</li>
            <li>{t('차단은 내 기기에서만 적용됩니다. 다른 멤버에게는 그 사람이 그대로 보입니다.')}</li>
          </ul>
        </div>
      </div>
    </div>
  );
}

const ACTIONS: ReportAction[] = ['dismiss', 'delete_message', 'kick', 'ban'];
const ACTION_PERM: Record<ReportAction, string | null> = { dismiss: null, delete_message: 'manage_messages', kick: 'kick', ban: 'ban' };
// 동작 이름에 어미를 붙이지 않고 문장째 번역합니다.
const CONFIRM_TEXT: Record<ReportAction, string> = {
  dismiss: t('기각할까요?'),
  delete_message: t('메시지 삭제할까요?'),
  kick: t('추방할까요?'),
  ban: t('밴할까요?'),
};

/**
 * 신고 처리 버튼을 누를 수 있는지. 추방·밴은 권한에 더해 데몬처럼 순위도 봅니다(나보다 낮은 사람만).
 * 작성자를 모르면(이 기기 화면에 사본이 없음) 데몬이 자기 사본으로 확인하므로 막지 않고 그렇게 알립니다.
 */
export function actionGate(view: SpaceView, me: string | null, action: ReportAction, author: string | undefined): { allowed: boolean; title?: string } {
  const perm = ACTION_PERM[action];
  if (perm && !hasPerm(view, perm)) return { allowed: false, title: t('권한이 없습니다') };
  if (action !== 'kick' && action !== 'ban') return { allowed: true };
  if (!author) return { allowed: true, title: t('작성자의 순위는 데몬이 이 기기의 메시지 사본으로 확인합니다') };
  if (!outranks(view, me, author)) return { allowed: false, title: t('나보다 순위가 낮은 사람만 추방·밴할 수 있습니다') };
  return { allowed: true };
}

function ReportQueue({ space }: { space: string }) {
  const view = useStore((s) => s.views[space])!;
  const client = useStore((s) => s.client);
  const toast = useStore((s) => s.toast);
  const unseen = useStore((s) => s.reportsNew[space] ?? 0);
  const [status, setStatus] = useState<ReportStatus>('open');
  const [list, setList] = useState<ReportView[] | null>(null);
  const [unsupported, setUnsupported] = useState(false);
  const load = useCallback(async () => {
    if (!client) return;
    try {
      setList(await client.call('report.list', { space, status }));
    } catch (e) {
      if (e instanceof RpcError && e.code === ErrorCode.MethodNotFound) setUnsupported(true);
      else {
        setList([]);
        toast('error', t('신고 목록을 읽지 못했습니다: {error}', { error: (e as Error).message }));
      }
    }
  }, [client, space, status, toast]);
  // 새 신고 알림(report.new)이 오면 다시 읽습니다.
  useEffect(() => {
    void load();
  }, [load, unseen]);
  if (unsupported) return <p className="muted">{t('연결된 데몬이 신고를 지원하지 않습니다.')}</p>;
  return (
    <div className="stack">
      <div className="row between">
        <h3>{t('신고')}</h3>
        <div className="row gap-s">
          <div className="seg">
            <button type="button" className={status === 'open' ? 'active' : ''} onClick={() => setStatus('open')}>
              {t('처리 전')}
            </button>
            <button type="button" className={status === 'resolved' ? 'active' : ''} onClick={() => setStatus('resolved')}>
              {t('처리함')}
            </button>
          </div>
          <button type="button" className="icon-btn small" title={t('새로고침')} onClick={() => void load()}>
            <RefreshCw size={14} />
          </button>
        </div>
      </div>
      {list === null ? (
        <Spinner />
      ) : list.length === 0 ? (
        <p className="muted">{status === 'open' ? t('처리할 신고가 없습니다.') : t('처리한 신고가 없습니다.')}</p>
      ) : (
        <div className="table">
          {list.map((r) => (
            <ReportRow key={r.report_id} space={space} view={view} report={r} onDone={() => void load()} />
          ))}
        </div>
      )}
    </div>
  );
}

function ReportRow({ space, view, report: r, onDone }: { space: string; view: SpaceView; report: ReportView; onDone: () => void }) {
  const client = useStore((s) => s.client);
  const call = useStore((s) => s.call);
  const closeModal = useStore((s) => s.closeModal);
  const jumpToMessage = useStore((s) => s.jumpToMessage);
  const me = useStore((s) => s.me);
  // 이 기기에 불러온 메시지 사본(없으면 운영진도 내용을 모릅니다).
  const msg = useStore((s) => s.channels[chKey(space, r.channel)]?.messages.find((m) => m.id === r.message));
  const [note, setNote] = useState('');
  const [confirm, setConfirm] = useState<ReportAction | null>(null);
  const [busy, setBusy] = useState(false);
  const reporter = view.members.find((m) => m.user === r.reporter);
  const channel = view.channels.find((c) => c.channel === r.channel);
  const msgAnon = !!msg && isAnonymousMessage(msg);
  const author = msg && !msgAnon ? view.members.find((m) => m.user === msg.author) : undefined;
  const authorName = msgAnon ? ANON_NAME : displayName(author);
  const resolve = async (action: ReportAction) => {
    if (!client) return;
    setBusy(true);
    const ok = await call(client.call('report.resolve', { space, report_id: r.report_id, action, note: note.trim() || undefined }), {
      ok: t('신고를 처리했습니다({action})', { action: REPORT_ACTION_LABELS[action] }),
    });
    setBusy(false);
    setConfirm(null);
    if (ok !== undefined) onDone();
  };
  return (
    <div className="tr report-row">
      <Flag size={18} className={r.status === 'open' ? 'warn' : 'muted'} />
      <div className="grow stack-s">
        <div className="row gap-s">
          <strong>{displayName(reporter, t('알 수 없는 사용자'))}</strong>
          <span className="muted small">
            {tn('님이 #{channel}의 메시지를 신고 · {when}', {
              channel: channel?.settings.name ?? '?',
              when: <span title={formatFull(r.reported_at)}>{formatRelative(r.reported_at)}</span>,
            })}
          </span>
          {r.status === 'resolved' && <span className="tag ok">{formatResolution(r.resolution)}</span>}
        </div>
        <div className="report-reason">{r.reason}</div>
        <div className="report-msg muted small">
          {msg ? (
            <>
              <Avatar id={msgAnon ? ANONYMOUS_USER : msg.author} name={authorName} size={16} /> {authorName}: {msg.deleted ? t('삭제된 메시지') : excerpt(msg.text, 120) || t('첨부 파일')}
            </>
          ) : (
            t('이 기기에 불러오지 않은 메시지입니다.')
          )}
          <button
            type="button"
            className="link-btn"
            onClick={() => {
              closeModal();
              void jumpToMessage(space, r.channel, r.message);
            }}
          >
            <MessageSquare size={12} /> {t('메시지 보기')}
          </button>
        </div>
        {r.status === 'open' && (
          <>
            <input
              className="input small"
              placeholder={t('처리 메모(선택). 운영진끼리만 보고, 추방·밴 사유로는 남기지 않습니다')}
              maxLength={MAX_REASON_CHARS}
              value={note}
              onChange={(e) => setNote(e.target.value)}
            />
            {confirm ? (
              <div className="report-actions">
                <span className="small">
                  {CONFIRM_TEXT[confirm]} {confirm === 'ban' ? t('같은 신원은 다시 들어오지 못합니다.') : ''}
                </span>
                <button type="button" className="btn small danger" disabled={busy} onClick={() => void resolve(confirm)}>
                  {busy && <Spinner size={12} />} {t('확인')}
                </button>
                <button type="button" className="btn small ghost" onClick={() => setConfirm(null)}>
                  {t('취소')}
                </button>
              </div>
            ) : (
              <div className="report-actions">
                {ACTIONS.map((a) => {
                  // 익명 글의 작성자 순위는 앱이 모르므로 데몬이 판단합니다.
                  const { allowed, title } = actionGate(view, me, a, msgAnon ? undefined : msg?.author);
                  return (
                    <button
                      key={a}
                      type="button"
                      className={cx('btn small', a === 'dismiss' ? 'ghost' : 'danger-outline')}
                      disabled={busy || !allowed}
                      title={title}
                      onClick={() => (a === 'dismiss' ? void resolve(a) : setConfirm(a))}
                    >
                      {a === 'dismiss' ? <Check size={13} /> : a === 'kick' ? <UserX size={13} /> : a === 'ban' ? <Ban size={13} /> : null}{' '}
                      {REPORT_ACTION_LABELS[a]}
                    </button>
                  );
                })}
              </div>
            )}
          </>
        )}
      </div>
    </div>
  );
}

function BanList({ space }: { space: string }) {
  const client = useStore((s) => s.client);
  const call = useStore((s) => s.call);
  const view = useStore((s) => s.views[space]);
  const [list, setList] = useState<BanView[] | null>(null);
  const [unsupported, setUnsupported] = useState(false);
  const load = useCallback(async () => {
    if (!client) return;
    try {
      setList(await client.call('member.bans', { space }));
    } catch (e) {
      if (e instanceof RpcError && e.code === ErrorCode.MethodNotFound) setUnsupported(true);
      else setList([]);
    }
  }, [client, space]);
  // 크루 상태가 바뀌면(다른 운영진의 밴·해제) 다시 읽습니다.
  useEffect(() => {
    void load();
  }, [load, view]);
  if (unsupported) return null;
  return (
    <div className="stack">
      <h3>{list ? t('밴 목록 {n}명', { n: list.length }) : t('밴 목록')}</h3>
      {list === null ? (
        <Spinner />
      ) : list.length === 0 ? (
        <p className="muted">{t('밴한 사람이 없습니다.')}</p>
      ) : (
        <div className="table">
          {list.map((b) => {
            const name = b.profile?.name ?? `${b.user.slice(0, 10)}…`;
            const by = view?.members.find((m) => m.user === b.banned_by);
            return (
              <div key={b.user} className="tr">
                <Avatar id={b.user} name={name} size={32} />
                <div className="grow">
                  <strong>{name}</strong>
                  <div className="muted small">
                    {displayName(by, t('알 수 없는 운영진'))} · <span title={formatFull(b.banned_at)}>{formatRelative(b.banned_at)}</span>
                    {b.reason ? ` · ${b.reason}` : ''}
                  </div>
                </div>
                <button
                  type="button"
                  className="btn small"
                  onClick={async () => {
                    if (!client) return;
                    const r = await call(client.call('member.unban', { space, user: b.user }), { ok: t('{name} 님의 밴을 풀었습니다. 다시 초대해야 들어옵니다.', { name }) });
                    if (r !== undefined) void load();
                  }}
                >
                  {t('밴 풀기')}
                </button>
              </div>
            );
          })}
        </div>
      )}
    </div>
  );
}
