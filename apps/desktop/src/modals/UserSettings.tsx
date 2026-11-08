// 사용자 설정: 프로필, 기기, 후원 확인, 차단 목록, 동기화(시더·피어 정책), 저장 공간(받은 첨부 정리), 네트워크(Tor 상태·
// 신뢰 스위치 경고·STUN 서버), API 토큰, 감사 기록, 확인 대기, 진단(로컬 전용), 모양, 정보(라이선스 고지). 앱은 이 기기나 상대의 IP 주소를 어디에도 보이지 않습니다.

import { AlertTriangle, Check, Copy, KeyRound, Languages, Laptop, Moon, Pencil, RefreshCw, Shield, Smartphone, Sun, Trash2, X } from 'lucide-react';
import { useCallback, useEffect, useState, type ReactNode } from 'react';
import type { PickedFile } from '../api/client';
import {
  DATA_TYPES,
  SCOPES,
  type AuditEntry,
  type DataType,
  type DeviceId,
  type DiagMetrics,
  type PeerPolicy,
  type PeerSetParams,
  type PeerView,
  type Scope,
  type SeedSetParams,
  type SeederPolicy,
  type TokenView,
} from '../api/types';
import { BadgePicker } from '../components/Badges';
import { Avatar, Spinner, StillImg, Toggle, copyText, liveFlair, useImageBlobSrc } from '../components/common';
import { currentLang, LANGS, t, type Lang } from '../i18n';
import { hasEmoji } from '../lib/emoji';
import { formatBytes, formatFull, formatLimit, formatRelative } from '../lib/format';
import { colorHex, cx, hexToInt, roleColor } from '../lib/misc';
import { isAndroid } from '../lib/platform';
import { useStore, type NotifyCrew, type UserTab } from '../store/store';
import { Field, Modal } from './Modal';
import { ScopedLimitsSection } from './RateLimits';
import { SupporterPanel } from '../supporter/SupporterModal';
import { DataDirSection } from './DataDir';
import { DeviceLinkSection } from './DeviceLink';
import { StorageTab } from './StorageSettings';
import { ChannelTopicsSection } from './ChannelTopics';
import { BridgesSection } from './Bridges';
import { FileCircuitsSection } from './FileCircuits';
import { DirectModeWarning, StunServersSection } from './StunServers';
import { MediaTab } from './MediaSettings';

const TABS: Array<[UserTab, string, string?]> = [
  ['profile', t('프로필'), t('내 계정')],
  ['devices', t('기기')],
  ['supporter', t('후원')],
  ['privacy', t('개인정보')],
  ['sync', t('동기화'), t('앱 설정')],
  ['storage', t('저장 공간')],
  ['network', t('네트워크·Tor')],
  ['media', t('음성·화면공유')],
  ['appearance', t('모양·언어')],
  ['notify', t('알림')],
  ['tokens', t('API 토큰'), t('자동화')],
  ['confirm', t('확인 대기')],
  ['audit', t('감사 기록')],
  ['diag', t('진단')],
  ['about', t('정보')],
];

export function UserSettingsModal({ tab, onClose }: { tab?: UserTab; onClose: () => void }) {
  const [current, setCurrent] = useState<UserTab>(tab ?? 'profile');
  const confirmations = useStore((s) => s.confirmations);
  return (
    <Modal onClose={onClose} size="full" className="settings-modal" title={t('사용자 설정')}>
      <div className="settings">
        <nav className="settings-nav">
          {TABS.map(([id, label, section]) => (
            <div key={id}>
              {section && <div className="sn-title">{section}</div>}
              <button type="button" className={cx('sn-item', current === id && 'active')} onClick={() => setCurrent(id)}>
                {label}
                {id === 'confirm' && confirmations.length > 0 && <span className="ch-badge">{confirmations.length}</span>}
              </button>
            </div>
          ))}
        </nav>
        <section className="settings-body">
          {current === 'profile' && <ProfileTab />}
          {current === 'devices' && <DevicesTab />}
          {current === 'supporter' && <SupporterPanel />}
          {current === 'privacy' && <PrivacyTab />}
          {current === 'sync' && <SyncTab />}
          {current === 'storage' && (
            <div className="stack">
              <DataDirSection />
              <StorageTab />
            </div>
          )}
          {current === 'network' && <NetworkTab />}
          {current === 'media' && <MediaTab />}
          {current === 'appearance' && <AppearanceTab />}
          {current === 'notify' && <NotifyTab />}
          {current === 'tokens' && <TokensTab />}
          {current === 'confirm' && <ConfirmTab />}
          {current === 'audit' && <AuditTab />}
          {current === 'diag' && <DiagTab />}
          {current === 'about' && <AboutTab />}
        </section>
      </div>
    </Modal>
  );
}

/** 고른 새 이미지, 지우기(빈 경로), 그대로(null). */
type ImageChange = PickedFile | 'clear' | null;

const imagePath = (c: ImageChange) => (c === null ? undefined : c === 'clear' ? '' : c.path);

/** 내 프로필 이미지를 읽을 공간(데몬은 내가 멤버인 공간을 통해서만 프로필 이미지를 냅니다). */
function useMyImageSpace(): string | undefined {
  return useStore((s) => Object.keys(s.views)[0] ?? s.friends[0]?.space ?? s.spaces[0]?.space);
}

/** 미리보기의 아바타·배너를 눌러 이미지를 고르고, 올리면 연필이 덮입니다. 이미지가 있으면 구석의 ×로 지웁니다. */
function ImageEdit({
  kind,
  current,
  change,
  onChange,
  className,
  children,
}: {
  kind: 'avatar' | 'banner';
  current: string | null;
  change: ImageChange;
  onChange: (c: ImageChange) => void;
  className: string;
  children?: ReactNode;
}) {
  const client = useStore((s) => s.client);
  const has = change === 'clear' ? false : change !== null || current !== null;
  const pickLabel = kind === 'banner' ? t('배너 바꾸기') : t('아바타 바꾸기');
  const clearLabel = kind === 'banner' ? t('배너 지우기') : t('아바타 지우기');
  return (
    <div className={cx('img-edit', className)}>
      <button
        type="button"
        className="img-edit-pick"
        title={pickLabel}
        aria-label={pickLabel}
        onClick={async () => {
          const f = await client?.transport.pickFiles({ multiple: false, images: true });
          if (f?.[0]) onChange(f[0]);
        }}
      >
        {children}
        <span className="img-edit-overlay" aria-hidden>
          <Pencil size={20} />
        </span>
      </button>
      {has && (
        <button type="button" className="img-edit-clear" title={clearLabel} aria-label={clearLabel} onClick={() => onChange(current ? 'clear' : null)}>
          <X size={12} />
        </button>
      )}
    </div>
  );
}

/** 저장 전 바뀔 이미지 안내(고른 이미지는 저장해야 미리보기에 보입니다). */
const changeText = (label: string, c: ImageChange) => (c === null ? null : c === 'clear' ? t('{label}: 저장하면 지웁니다', { label }) : `${label}: ${c.name}`);

function ProfileTab() {
  const identity = useStore((s) => s.identity)!;
  const client = useStore((s) => s.client);
  const call = useStore((s) => s.call);
  const p = identity.profile;
  const [name, setName] = useState(p.name);
  const [status, setStatus] = useState(p.status);
  const [bio, setBio] = useState(p.bio);
  const [color, setColor] = useState(roleColor(p.theme_color ?? 0) ?? '#e8541e');
  const [useColor, setUseColor] = useState(p.theme_color != null);
  const [avatar, setAvatar] = useState<ImageChange>(null);
  const [banner, setBanner] = useState<ImageChange>(null);
  const space = useMyImageSpace();
  const bannerImg = useImageBlobSrc(space, banner === null ? p.banner : null);
  // 후원자 표시: 카드 배너는 월 14달러, 후광은 월 50달러 이상 정기 후원자와 같은 금액 이상 GitHub 1회 후원자(30일)만(`flair.list`).
  const flair = useStore((s) => liveFlair(s.flair[identity.user]));
  const haloAllowed = !!flair?.halo_allowed;
  const savedHalo = flair?.halo ?? null;
  const [useHalo, setUseHalo] = useState(savedHalo !== null);
  const [haloA, setHaloA] = useState(colorHex(savedHalo?.[0] ?? 0xe8541e));
  const [haloB, setHaloB] = useState(colorHex(savedHalo?.[1] ?? 0xffc24b));
  // 설정을 연 뒤 표시가 늦게 오거나 다른 기기에서 바뀌면 고르던 값을 저장된 값으로 맞춥니다.
  const savedKey = savedHalo?.join() ?? '';
  useEffect(() => {
    setUseHalo(savedKey !== '');
    if (savedHalo) {
      setHaloA(colorHex(savedHalo[0]));
      setHaloB(colorHex(savedHalo[1]));
    }
  }, [savedKey]);
  const halo: [number, number] | null = useHalo ? [hexToInt(haloA), hexToInt(haloB)] : null;
  const haloDirty = haloAllowed && (halo?.join() ?? null) !== (savedHalo?.join() ?? null);
  const profileDirty =
    name !== p.name ||
    status !== p.status ||
    bio !== p.bio ||
    useColor !== (p.theme_color != null) ||
    (useColor && hexToInt(color) !== p.theme_color) ||
    avatar !== null ||
    banner !== null;
  const dirty = profileDirty || haloDirty;
  // 예전에 넣은 이모지 이름은 그대로 두고 다른 것만 고칠 수 있게, 바꿀 때만 봅니다(데몬과 같음).
  const nameBad = name.trim() !== p.name && hasEmoji(name);
  const save = async () => {
    if (!client) return;
    if (haloDirty) {
      const v = await call(client.call('flair.set', { halo }), profileDirty ? undefined : { ok: t('프로필을 저장했습니다') });
      if (v) useStore.setState((s) => ({ flair: { ...s.flair, [v.user]: v } }));
    }
    if (!profileDirty) return;
    const r = await call(
      client.call('profile.set', {
        name: name.trim(),
        status: status.trim(),
        bio,
        theme_color: useColor ? hexToInt(color) : null,
        avatar_path: imagePath(avatar),
        banner_path: imagePath(banner),
      }),
      { ok: t('프로필을 저장했습니다') },
    );
    if (r) {
      useStore.setState({ identity: { ...identity, profile: r } });
      setAvatar(null);
      setBanner(null);
    }
  };
  return (
    <div className="stack">
      <h3>{t('프로필')}</h3>
      <div className="profile-preview" style={{ ['--banner' as string]: useColor ? color : 'var(--accent)' }}>
        <ImageEdit kind="banner" current={p.banner} change={banner} onChange={setBanner} className="pp-banner">
          {bannerImg.src && <StillImg className="pf-banner-img" src={bannerImg.src} onError={bannerImg.onError} />}
        </ImageEdit>
        <ImageEdit kind="avatar" current={p.avatar} change={avatar} onChange={setAvatar} className="pp-avatar-edit">
          <Avatar id={identity.user} name={name || '?'} size={72} color={useColor ? hexToInt(color) : null} className="pp-avatar" halo={haloAllowed ? halo : undefined} />
        </ImageEdit>
        <div className="pp-name">{name}</div>
        <div className="muted small">{status}</div>
      </div>
      <p className="muted small">
        {[changeText(t('아바타'), avatar), changeText(t('배너'), banner)].filter(Boolean).join(' · ') ||
          t('아바타·배너를 눌러 바꿉니다. 이미지는 친구·크루 멤버에게만 암호화해 보냅니다.')}
      </p>
      {!flair?.banner && !isAndroid() && <p className="muted small">{t('프로필 카드 배경의 배너는 월 14달러 이상 정기 후원자와 GitHub에서 14달러 이상 1회 후원한 사람(30일 동안)에게만 보입니다. 프로필 창의 배너는 누구나 보입니다.')}</p>}
      <div className="grid2">
        <Field label={t('이름')} hint={nameBad ? t('이름에 이모지를 넣을 수 없습니다') : undefined}>
          <input className="input" value={name} maxLength={64} onChange={(e) => setName(e.target.value)} />
        </Field>
        <Field label={t('상태 메시지')}>
          <input className="input" value={status} maxLength={128} onChange={(e) => setStatus(e.target.value)} />
        </Field>
      </div>
      <Field label={t('소개')}>
        <textarea className="input" rows={3} value={bio} maxLength={1000} onChange={(e) => setBio(e.target.value)} />
      </Field>
      <Field label={t('테마 색')}>
        <div className="row gap">
          <Toggle checked={useColor} onChange={setUseColor} label={t('테마 색 사용')} />
          <input type="color" className="color" value={color} disabled={!useColor} onChange={(e) => setColor(e.target.value)} />
        </div>
      </Field>
      <Field label={t('후광')}>
        {haloAllowed ? (
          <div className="row gap">
            <Toggle checked={useHalo} onChange={setUseHalo} label={t('후광 사용')} />
            <input type="color" className="color" value={haloA} disabled={!useHalo} aria-label={t('후광 첫째 색')} onChange={(e) => setHaloA(e.target.value)} />
            <input type="color" className="color" value={haloB} disabled={!useHalo} aria-label={t('후광 둘째 색')} onChange={(e) => setHaloB(e.target.value)} />
          </div>
        ) : isAndroid() ? (
          <p className="muted small">{t('후광은 후원자만 쓸 수 있습니다')}</p>
        ) : (
          <p className="muted small">{t('후광은 월 50달러 이상 정기 후원자와 GitHub에서 50달러 이상 1회 후원한 사람(30일 동안)만 쓸 수 있습니다')}</p>
        )}
      </Field>
      <h3>{t('뱃지')}</h3>
      <p className="muted small">{t('모은 뱃지는 프로필 이름 아래에 보입니다. 두 개까지 골라 이름 앞이나 뒤에 붙일 수 있습니다.')}</p>
      <BadgePicker />
      <div className="kv">
        <span>{t('사용자 ID')}</span>
        <span className="mono small">{identity.user}</span>
        <span>{t('이 기기 ID')}</span>
        <span className="mono small">{identity.device}</span>
      </div>
      {dirty && (
        <div className="save-bar sticky">
          <span>{t('저장하지 않은 변경이 있습니다')}</span>
          <button type="button" className="btn primary small" disabled={!name.trim() || nameBad} onClick={() => void save()}>
            {t('저장')}
          </button>
        </div>
      )}
    </div>
  );
}

export function DevicesTab() {
  const identity = useStore((s) => s.identity)!;
  const client = useStore((s) => s.client);
  const call = useStore((s) => s.call);
  const openModal = useStore((s) => s.openModal);
  const deleteAccount = useStore((s) => s.deleteAccount);
  const [devices, setDevices] = useState(identity.devices);
  const [renaming, setRenaming] = useState<{ device: DeviceId; name: string } | null>(null);
  const load = useCallback(() => client?.call('device.list', {}).then(setDevices).catch(() => undefined), [client]);
  useEffect(() => {
    void load();
  }, [load]);
  const rename = async () => {
    if (!client || !renaming) return;
    await call(client.call('device.rename', renaming));
    setRenaming(null);
    void load();
  };
  return (
    <div className="stack">
      <h3>{t('기기')}</h3>
      <p className="muted small">
        {t('같은 신원으로 쓰는 기기들입니다. 잃어버린 기기는 폐기하세요. 폐기하면 모든 그룹에서 그 기기를 빼고 키를 바꿉니다.')}
      </p>
      <h4>{t('내 기기')}</h4>
      <div className="table">
        {/* 서명된 목록은 늘기만 해서 폐기 기록이 남으므로 화면에서만 뺍니다. */}
        {devices.filter((d) => d.revoked_at == null).map((d) => {
          const current = d.device === identity.device;
          return (
            <div key={d.device} className="tr">
              {d.mobile ? <Smartphone size={22} /> : <Laptop size={22} />}
              <div className="grow">
                {renaming?.device === d.device ? (
                  <form
                    className="row gap-s"
                    onSubmit={(e) => {
                      e.preventDefault();
                      void rename();
                    }}
                  >
                    <input
                      className="input"
                      value={renaming.name}
                      maxLength={100}
                      autoFocus
                      aria-label={t('기기 이름')}
                      onChange={(e) => setRenaming({ ...renaming, name: e.target.value })}
                    />
                    <button type="submit" className="btn small" disabled={!renaming.name.trim()}>
                      {t('저장')}
                    </button>
                    <button type="button" className="btn small" onClick={() => setRenaming(null)}>
                      {t('취소')}
                    </button>
                  </form>
                ) : (
                  <div className="row gap-s">
                    <strong>{d.name}</strong>
                    {current && <span className="tag ok">{t('이 기기')}</span>}
                    <button
                      type="button"
                      className="icon-btn"
                      title={t('이름 바꾸기')}
                      aria-label={t('이름 바꾸기')}
                      onClick={() => setRenaming({ device: d.device, name: d.name })}
                    >
                      <Pencil size={14} />
                    </button>
                  </div>
                )}
                <div className="muted small mono">
                  {d.device.slice(0, 20)}… · {t('{when} 추가', { when: formatRelative(d.added_at) })}
                </div>
              </div>
              {!current && (
                <button
                  type="button"
                  className="btn small danger-outline"
                  onClick={() =>
                    openModal({
                      kind: 'confirm',
                      title: t('기기 폐기'),
                      body: t('「{name}」을(를) 폐기합니다. 되돌릴 수 없고, 그 기기는 다시 켜지면 이 계정의 데이터를 모두 지웁니다. 그 기기에만 있는 지난 대화도 함께 사라지니, 꺼져 있을 뿐이라면 폐기하지 마세요.', { name: d.name }),
                      danger: true,
                      confirmLabel: t('폐기'),
                      onConfirm: async () => {
                        if (!client) return;
                        await call(client.call('device.revoke', { device: d.device }), { ok: t('기기를 폐기했습니다') });
                        void load();
                      },
                    })
                  }
                >
                  {t('폐기')}
                </button>
              )}
            </div>
          );
        })}
      </div>
      <DeviceLinkSection onDone={load} />
      <h3>{t('계정 삭제')}</h3>
      <div className="card row between gap">
        <div className="muted small">{t('이 기기의 계정과 메시지·파일을 모두 지웁니다. 서버가 없어 내 다른 기기의 계정과 다른 사람이 이미 받은 메시지는 지워지지 않습니다. 내 다른 기기가 없으면 이 계정은 다시 찾을 수 없습니다.')}</div>
        <button
          type="button"
          className="btn small danger-outline"
          onClick={() =>
            openModal({
              kind: 'confirm',
              title: t('계정 삭제'),
              body: t('이 기기의 계정과 모든 데이터를 지웁니다. 되돌릴 수 없습니다.'),
              danger: true,
              confirmLabel: t('계정 삭제'),
              onConfirm: deleteAccount,
            })
          }
        >
          {t('계정 삭제')}
        </button>
      </div>
    </div>
  );
}

function PrivacyTab() {
  const blocked = useStore((s) => s.blocked);
  const blockedDevices = useStore((s) => s.blockedDevices);
  const refresh = useStore((s) => s.refreshBlocked);
  const unblock = useStore((s) => s.unblockUser);
  const unblockDevice = useStore((s) => s.unblockDevice);
  const stripMetadata = useStore((s) => s.prefs.stripMetadata);
  const setPrefs = useStore((s) => s.setPrefs);
  useEffect(() => {
    void refresh();
  }, [refresh]);
  return (
    <div className="stack">
      <h3>{t('첨부')}</h3>
      <div className="card row between">
        <div>
          <strong>{t('첨부 메타데이터 지우기')}</strong>
          <div className="muted small">
            {t(
              '사진(JPEG·PNG·WebP·GIF)과 동영상(MP4·MOV)에서 촬영 위치·기기·시각 같은 정보를 지우고 보냅니다. 화질은 그대로이고 사진 회전 정보만 남깁니다. HEIC·PDF·문서 같은 다른 형식은 그대로 보냅니다.',
            )}
          </div>
        </div>
        <Toggle checked={stripMetadata} label={t('첨부 메타데이터 지우기')} onChange={(v) => setPrefs({ stripMetadata: v })} />
      </div>
      <h3>{t('차단한 사용자')}</h3>
      <p className="muted small">
        {t(
          '차단은 이 기기에만 저장되고 상대에게 알리지 않습니다. 차단한 사람의 메시지는 접어 두고, 알림을 내지 않고, 친구 요청을 조용히 거절하며, 첨부를 자동으로 받지 않습니다. 같은 크루의 다른 사람에게는 그 사람의 메시지가 그대로 보입니다.',
        )}
      </p>
      {blocked === null ? (
        <p className="muted">{t('연결된 데몬이 차단을 지원하지 않습니다.')}</p>
      ) : blocked.length === 0 ? (
        <p className="muted">{t('차단한 사용자가 없습니다.')}</p>
      ) : (
        <div className="table">
          {blocked.map((b) => (
            <div key={b.user} className="tr">
              <Avatar id={b.user} name={b.profile?.name ?? '?'} size={32} />
              <div className="grow">
                <strong>{b.profile?.name ?? t('알 수 없는 사용자')}</strong>
                <div className="muted small mono">
                  {b.user.slice(0, 16)}… · {t('{when} 차단', { when: formatRelative(b.blocked_at) })}
                </div>
              </div>
              <button type="button" className="btn small" onClick={() => void unblock(b.user)}>
                {t('차단 풀기')}
              </button>
            </div>
          ))}
        </div>
      )}
      {blockedDevices !== null && (
        <>
          <h3>{t('차단한 기기')}</h3>
          <p className="muted small">
            {t('다른 사람 계정의 기기 하나만 막은 목록입니다. 그 기기와는 연결하지 않고 그 기기가 쓴 메시지는 접어 둡니다. 계정의 다른 기기는 그대로입니다.')}
          </p>
          {blockedDevices.length === 0 ? (
            <p className="muted">{t('차단한 기기가 없습니다.')}</p>
          ) : (
            <div className="table">
              {blockedDevices.map((b) => (
                <div key={b.device} className="tr">
                  <Avatar id={b.user} name={b.profile?.name ?? '?'} size={32} />
                  <div className="grow">
                    <strong>
                      {b.profile?.name ?? t('알 수 없는 사용자')} · {b.name || t('이름 없는 기기')}
                    </strong>
                    <div className="muted small mono">
                      {b.device.slice(0, 16)}… · {t('{when} 차단', { when: formatRelative(b.blocked_at) })}
                    </div>
                  </div>
                  <button type="button" className="btn small" onClick={() => void unblockDevice(b.user, b.device)}>
                    {t('차단 풀기')}
                  </button>
                </div>
              ))}
            </div>
          )}
        </>
      )}
    </div>
  );
}

const TYPE_LABEL: Record<DataType, string> = {
  text: t('텍스트'),
  image: t('이미지'),
  video: t('동영상'),
  file: t('파일'),
  emoji: t('이모지'),
  history_bundle: t('기록 묶음'),
};

function SyncTab() {
  const client = useStore((s) => s.client);
  const call = useStore((s) => s.call);
  const tor = useStore((s) => s.net?.mode === 'tor');
  const [seed, setSeed] = useState<SeederPolicy | null>(null);
  const [peer, setPeer] = useState<PeerPolicy | null>(null);
  useEffect(() => {
    if (!client) return;
    void client.call('seed.get', {}).then(setSeed).catch(() => undefined);
    void client.call('peer.get', {}).then(setPeer).catch(() => undefined);
  }, [client]);
  if (!seed || !peer) return <Spinner />;
  const setS = async (patch: Partial<SeederPolicy>, params: SeedSetParams) => {
    if (!client) return;
    const r = await call(client.call('seed.set', params));
    if (r !== undefined) setSeed({ ...seed, ...patch });
  };
  const setP = async (patch: Partial<PeerPolicy>) => {
    if (!client) return;
    const r = await call(client.call('peer.set', patch as PeerSetParams));
    if (r !== undefined) setPeer({ ...peer, ...patch });
  };
  const mbps = (v: number | null) => (v == null ? '' : String(Math.round((v / 1024 / 1024) * 10) / 10));
  return (
    <div className="stack">
      <h3>{t('시더(나눠 주기)')}</h3>
      <div className="card row between">
        <div>
          <strong>{t('이 기기를 시더로 쓰기')}</strong>
          <div className="muted small">
            {t('켜 두면 오프라인이었던 멤버가 이 기기에서 기록을 받아 갑니다. 크루별 설정은 크루 설정 → 시더 커버리지.')}
          </div>
        </div>
        <Toggle checked={seed.enabled} label={t('시더')} onChange={(v) => void setS({ enabled: v }, { enabled: v })} />
      </div>
      {seed.prefetch !== undefined && (
        <div className="card row between">
          <div>
            <strong>{t('열지 않은 채널의 첨부도 미리 받기')}</strong>
            <div className="muted small">
              {t(
                '늘 켜 두는 기기(헤드리스 시더)용. 볼 수 있는 모든 채널의 첨부를 저장 공간 한도 안에서 받아 두고 나눠 줍니다. 열지 않은 채널도 제공하게 됩니다.',
              )}
            </div>
          </div>
          <Toggle checked={seed.prefetch} label={t('미리 받기')} disabled={!seed.enabled} onChange={(v) => void setS({ prefetch: v }, { prefetch: v })} />
        </div>
      )}
      <Field label={t('나눠 줄 데이터')}>
        <div className="chips">
          {DATA_TYPES.map((dt) => (
            <button
              key={dt}
              type="button"
              className={cx('chip', seed.types.includes(dt) && 'active')}
              onClick={() => {
                const types = seed.types.includes(dt) ? seed.types.filter((x) => x !== dt) : [...seed.types, dt];
                void setS({ types }, { types });
              }}
            >
              {TYPE_LABEL[dt]}
            </button>
          ))}
        </div>
      </Field>
      <div className="grid2">
        <Field
          label={t('올리기 속도 제한(MiB/s, 비우면 무제한)')}
          hint={t('지금: {limit} · 0이면 아무것도 내주지 않음', { limit: formatLimit(seed.upload_limit_bps) })}
        >
          <input
            className="input"
            type="number"
            min={0}
            step={0.5}
            defaultValue={mbps(seed.upload_limit_bps)}
            onBlur={(e) => {
              const v = e.target.value.trim() ? Math.round(Number(e.target.value) * 1024 * 1024) : null;
              // 0은 올리지 않음(남에게 내주지 않음).
              if (v !== seed.upload_limit_bps && (v === null || v >= 0)) void setS({ upload_limit_bps: v }, { upload_limit_bps: v });
            }}
          />
        </Field>
        <Field label={t('동시에 보내 줄 기기 수')}>
          <input
            className="input"
            type="number"
            min={1}
            max={64}
            defaultValue={seed.max_concurrent}
            onBlur={(e) => {
              const v = Math.max(1, Math.min(64, Number(e.target.value) || 1));
              if (v !== seed.max_concurrent) void setS({ max_concurrent: v }, { max_concurrent: v });
            }}
          />
        </Field>
      </div>
      <h3>{t('받기')}</h3>
      <Field label={t('자동으로 받을 데이터')}>
        <div className="chips">
          {DATA_TYPES.map((dt) => (
            <button
              key={dt}
              type="button"
              className={cx('chip', peer.types.includes(dt) && 'active')}
              onClick={() => void setP({ types: peer.types.includes(dt) ? peer.types.filter((x) => x !== dt) : [...peer.types, dt] })}
            >
              {TYPE_LABEL[dt]}
            </button>
          ))}
        </div>
      </Field>
      <div className="grid2">
        <Field
          label={t('받기 속도 제한(MiB/s, 비우면 무제한)')}
          hint={t('지금: {limit} · 0이면 첨부를 받지 않음', { limit: formatLimit(peer.download_limit_bps) })}
        >
          <input
            className="input"
            type="number"
            min={0}
            step={0.5}
            defaultValue={mbps(peer.download_limit_bps)}
            onBlur={(e) => {
              const v = e.target.value.trim() ? Math.round(Number(e.target.value) * 1024 * 1024) : null;
              // 0은 받지 않음(첨부 조각을 요청하지 않음).
              if (v !== peer.download_limit_bps && (v === null || v >= 0)) void setP({ download_limit_bps: v });
            }}
          />
        </Field>
        <Field label={t('자동 받기 최대 크기(MB)')} hint={t('이보다 큰 첨부는 ‘원본 받기’를 눌러야 받습니다.')}>
          <input
            className="input"
            type="number"
            min={0}
            defaultValue={Math.round(peer.auto_download_max_bytes / 1024 / 1024)}
            onBlur={(e) => {
              const v = Math.max(0, Math.round(Number(e.target.value) || 0)) * 1024 * 1024;
              if (v !== peer.auto_download_max_bytes) void setP({ auto_download_max_bytes: v });
            }}
          />
        </Field>
      </div>
      {tor && (
        <div className="muted small">
          {t(
            '모든 연결이 Tor로 오가서 Tor 회로가 먼저 한계라 파일은 보통 초당 수십~수백 KiB로 오갑니다. 위 속도 제한은 그보다 낮게 잡았을 때만 영향을 줍니다. P2P 전용 채널에 참가하면 그 채널의 파일은 참가한 기기끼리 직접 오갑니다.',
          )}
        </div>
      )}
      <ScopedLimitsSection back={{ kind: 'userSettings', tab: 'sync' }} />
    </div>
  );
}

/** Tor라서 꺼지거나 느려지는 것(앱 사용자 기기는 늘 Tor로 돕니다). */
export const TOR_LIMITS = [
  t('고화질 화면공유(Tor 통화는 소리와 비상용 저화질 화면만, P2P 전용 채널 안은 예외)'),
  t('같은 네트워크(LAN) 기기 자동 발견(mDNS)'),
  t('직접 연결(홀펀칭) — 연결이 Tor 회로를 거쳐 느려집니다(P2P 전용 채널 안, 신뢰한 상대, 내 기기끼리는 예외)'),
  t('링크 미리보기 플레이어 불러오기'),
  t('큰 첨부의 자동 받기(대역폭 절약)'),
];

export function NetworkTab() {
  const net = useStore((s) => s.net);
  const client = useStore((s) => s.client);
  const skipTrustWarning = useStore((s) => s.prefs.skipTrustWarning);
  const setPrefs = useStore((s) => s.setPrefs);
  const [peers, setPeers] = useState<PeerView[] | null>(null);
  const refresh = useCallback(async () => {
    if (!client) return;
    const st = await client.call('net.status', {}).catch(() => null);
    if (st) useStore.setState({ net: st });
    setPeers(await client.call('net.peers', {}).catch(() => []));
  }, [client]);
  useEffect(() => {
    void refresh();
  }, [refresh]);
  if (!net) return <Spinner />;
  // 직접 연결은 데몬을 그렇게 띄웠을 때뿐(상시 시더·중계 서버·시험)이라 고를 수 있는 설정이 없습니다.
  const tor = net.mode === 'tor';
  const progress = tor && net.bootstrap_progress !== undefined && net.bootstrap_progress < 100 ? net.bootstrap_progress : null;
  return (
    <div className="stack">
      <h3>{t('네트워크')}</h3>
      <div className={cx('card tor-card', tor && 'on')}>
        <div>
          <strong>
            <Shield size={15} /> Tor
          </strong>
          {/* README "앱이 바깥과 통신하는 곳" 표와 같은 목록입니다. 한쪽을 고치면 다른 쪽도 고칩니다. */}
          <div className="muted small">
            {t('연결은 기본으로 Tor로 오가서 대화 상대와 중계 서버는 내 IP 주소를 알 수 없습니다. 다만 아래 경우에는 적힌 곳에 내 IP가 보입니다.')}
            <ul className="tor-ip">
              <li>{t('Tor에 들어가지 못할 때(브리지 설정을 바꾼 적이 없으면 자동): Snowflake 브로커와 자원봉사 프록시')}</li>
              <li>{t('기기가 2대 이상일 때(자동, 오프라인 모드 제외): 내 다른 기기, 공개 STUN 서버(기본 Google·Cloudflare), 공유기(UPnP)')}</li>
              <li>{t('P2P 전용 채널에 참가했을 때: 함께 참가한 기기, STUN 서버')}</li>
              <li>{t('신뢰 스위치를 켰을 때: 신뢰한 사람·크루 멤버, STUN 서버(한쪽만 신뢰하면 신뢰한 쪽 IP만)')}</li>
              <li>{t('크루 STUN 서버를 쓰기로 했을 때: 그 크루의 STUN 서버')}</li>
              <li>{t('Patreon으로 후원을 확인할 때: patreon.com(Patreon이 Tor 출구를 막아 직접 연결)')}</li>
            </ul>
          </div>
        </div>
        {progress !== null && (
          <div className="progress">
            <div className="bar" style={{ width: `${progress}%` }} />
            <span>{t('Tor 회로 만드는 중… {progress}%', { progress })}</span>
          </div>
        )}
        {tor && net.onion_addr && <div className="muted small mono">{t('어니언 주소: {addr}', { addr: net.onion_addr })}</div>}
        {!tor && <DirectModeWarning />}
        <div className="tor-off">
          <div className="small strong">{t('Tor라서 꺼지거나 느려지는 것')}</div>
          <ul>
            {TOR_LIMITS.map((limit) => (
              <li key={limit} className={tor ? 'off' : ''}>
                {tor ? <X size={12} /> : <Check size={12} />} {limit}
              </li>
            ))}
          </ul>
        </div>
      </div>
      {tor && <BridgesSection />}
      <div className="card row between">
        <div>
          <strong>{t('신뢰 스위치를 켤 때 IP 노출 경고')}</strong>
          <div className="muted small">
            {t('상대 프로필·크루 설정에서 신뢰 스위치를 켤 때마다 경고합니다. 경고에서 ‘다시 보지 않기’를 고르면 꺼지고, 여기서 다시 켤 수 있습니다.')}
          </div>
        </div>
        <Toggle checked={!skipTrustWarning} label={t('신뢰 스위치 IP 노출 경고')} onChange={(v) => setPrefs({ skipTrustWarning: !v })} />
      </div>
      <FileCircuitsSection />
      <ChannelTopicsSection />
      <StunServersSection />
      <div className="row between">
        <h3>{t('상태')}</h3>
        <button type="button" className="btn small" onClick={() => void refresh()}>
          <RefreshCw size={13} /> {t('새로고침')}
        </button>
      </div>
      <div className="kv">
        <span>{t('피어 ID')}</span>
        <span className="mono small">{net.peer_id}</span>
        <span>{t('연결 방식')}</span>
        <span>{tor ? 'Tor' : t('직접 연결(Tor 없음)')}</span>
        <span>NAT</span>
        <span>
          {
            {
              unknown: t('확인 중'),
              public: t('공개(직접 연결 가능)'),
              private: t('사설(중계·홀펀칭)'),
              disabled: t('꺼짐'),
            }[net.nat]
          }
        </span>
        <span>{t('연결')}</span>
        <span>
          {t('{n}개', { n: net.connections })} · {t('피어 {n}명', { n: net.peers })}
        </span>
      </div>
      <h3>{t('연결된 피어')}</h3>
      <div className="table">
        {peers === null && <Spinner />}
        {peers?.length === 0 && <p className="muted small">{t('연결된 피어가 없습니다.')}</p>}
        {peers?.map((p) => (
          <div key={p.peer_id} className="tr">
            <span className="mono small grow">{p.peer_id.slice(0, 24)}…</span>
            {p.relayed && <span className="tag">{t('중계')}</span>}
            <span className="small">{p.rtt_ms != null ? `${p.rtt_ms}ms` : '-'}</span>
          </div>
        ))}
      </div>
    </div>
  );
}

function AppearanceTab() {
  const prefs = useStore((s) => s.prefs);
  const setPrefs = useStore((s) => s.setPrefs);
  // 문구는 시작할 때 한 번 정해지므로(i18n.ts) 저장을 기다렸다가 화면을 다시 불러옵니다. 통화는 그때 끊깁니다.
  const changeLang = async (lang: Lang) => {
    const s = useStore.getState();
    await s.client?.transport.savePrefs({ ...s.prefs, lang }).catch(() => undefined);
    location.reload();
  };
  return (
    <div className="stack">
      <h3>{t('모양')}</h3>
      <div className="theme-pick">
        <button type="button" className={cx('theme-card dark', prefs.theme === 'dark' && 'active')} onClick={() => setPrefs({ theme: 'dark' })}>
          <Moon size={18} /> {t('어두운 테마')}
        </button>
        <button type="button" className={cx('theme-card light', prefs.theme === 'light' && 'active')} onClick={() => setPrefs({ theme: 'light' })}>
          <Sun size={18} /> {t('밝은 테마')}
        </button>
      </div>
      <h3>
        <Languages size={16} /> {t('언어')}
      </h3>
      <div className="card row between">
        <div>
          <strong>{t('화면 언어')}</strong>
          <div className="muted small">{t('바꾸면 앱 화면을 다시 불러옵니다. 통화 중이면 통화가 끊깁니다. 받은 메시지는 번역하지 않습니다.')}</div>
        </div>
        <select className="select" aria-label={t('화면 언어')} value={currentLang()} onChange={(e) => void changeLang(e.target.value as Lang)}>
          {LANGS.map(([code, name]) => (
            <option key={code} value={code}>
              {name}
            </option>
          ))}
        </select>
      </div>
      <h3>{t('창')}</h3>
      <div className="card row between">
        <div>
          <strong>{isAndroid() ? t('앱을 닫아도 메시지 받기') : t('닫으면 트레이로 내리기')}</strong>
          <div className="muted small">
            {isAndroid()
              ? t('켜면 앱을 닫아도 계속 돌아 메시지·알림을 받습니다. 알림 표시줄에 상시 알림이 보이고 배터리를 더 씁니다. 끄면 앱을 여는 동안만 받습니다.')
              : t(
                  '켜면 창을 닫아도 앱이 트레이에서 계속 돌아 메시지·알림을 받습니다. 트레이 아이콘을 누르면 창이 다시 열리고, 아이콘 메뉴의 "끝내기"로 앱을 끝냅니다. 끄면 창을 닫을 때 앱도 끝납니다. 리눅스는 트레이 아이콘이 없어 늘 닫으면 끝납니다.',
                )}
          </div>
        </div>
        <Toggle checked={prefs.closeToTray} label={isAndroid() ? t('앱을 닫아도 메시지 받기') : t('닫으면 트레이로 내리기')} onChange={(v) => setPrefs({ closeToTray: v })} />
      </div>
      {/* 시작 프로그램 등록은 윈도만 합니다(src-tauri/src/platform.rs). */}
      {navigator.userAgent.includes('Windows') && (
        <>
          <div className="card row between">
            <div>
              <strong>{t('윈도 시작 시 자동으로 켜기')}</strong>
              <div className="muted small">{t('켜면 윈도에 로그인할 때 앱이 함께 켜져 메시지·알림을 바로 받습니다.')}</div>
            </div>
            <Toggle checked={prefs.autoStart} label={t('윈도 시작 시 자동으로 켜기')} onChange={(v) => setPrefs({ autoStart: v })} />
          </div>
          <div className="card row between">
            <div>
              <strong>{t('시작 시 트레이로만 켜기')}</strong>
              <div className="muted small">
                {t(
                  '켜면 로그인할 때 창을 띄우지 않고 트레이에서만 켜집니다. 트레이 아이콘을 누르면 창이 열립니다. 직접 실행할 때는 늘 창이 뜹니다.',
                )}
              </div>
            </div>
            <Toggle
              checked={prefs.autoStartToTray}
              label={t('시작 시 트레이로만 켜기')}
              disabled={!prefs.autoStart}
              onChange={(v) => setPrefs({ autoStartToTray: v })}
            />
          </div>
        </>
      )}
    </div>
  );
}

const CREW_LEVELS: Array<[NotifyCrew, string]> = [
  ['all', t('모든 메시지')],
  ['mentions', t('나를 멘션한 메시지만')],
  ['off', t('끄기')],
];

export function NotifyTab() {
  const prefs = useStore((s) => s.prefs);
  const setPrefs = useStore((s) => s.setPrefs);
  return (
    <div className="stack">
      <h3>{t('OS 알림')}</h3>
      <div className="card row between">
        <div>
          <strong>{t('새 메시지 알림')}</strong>
          <div className="muted small">
            {t(
              '1대1 대화는 모든 메시지를, 크루는 아래 설정대로 알립니다. 창이 앞에 있어도 다른 채널의 메시지는 알리고, 지금 보고 있는 채널은 알리지 않습니다. 알림을 누르면 그 채널이 열립니다. 창을 트레이로 내린 동안에도 알리고, 앱을 끝내면 알림도 멈춥니다.',
            )}
          </div>
        </div>
        <Toggle checked={prefs.notify} label={t('OS 알림')} onChange={(v) => setPrefs({ notify: v })} />
      </div>
      <div className="card">
        <strong>{t('크루 메시지')}</strong>
        <div className="muted small">
          {t(
            '글타래 답글은 "모든 메시지"에서도 나를 멘션했을 때만 알립니다. 멘션은 입력창에서 @를 쓰고 목록에서 사람을 골라야 멘션이 됩니다. "많이 멘션하기" 권한이 있는 사람의 @everyone(모두)·@here(그때 접속한 사람)도 멘션으로 칩니다. 크루나 채널을 오른쪽 클릭하면 그 크루·채널만 알림 수준을 바꾸거나 잠시 끌 수 있습니다(끄는 동안은 멘션만 알림).',
          )}
        </div>
        <div className="seg" role="radiogroup" aria-label={t('크루 메시지 알림')}>
          {CREW_LEVELS.map(([id, label]) => (
            <button
              key={id}
              type="button"
              role="radio"
              aria-checked={prefs.notifyCrew === id}
              className={prefs.notifyCrew === id ? 'active' : ''}
              disabled={!prefs.notify}
              onClick={() => setPrefs({ notifyCrew: id })}
            >
              {label}
            </button>
          ))}
        </div>
      </div>
      <div className="card row between">
        <div>
          <strong>{t('알림에 메시지 내용 미리보기')}</strong>
          <div className="muted small">
            {t(
              '끄면 보낸 사람·채널만 보입니다. 윈도 알림 센터는 알림 내용을 디스크에 남깁니다. 휘발성 채널은 이 설정과 상관없이 내용 없이 "새 메시지"만 띄웁니다.',
            )}
          </div>
        </div>
        <Toggle
          checked={prefs.notifyPreview}
          label={t('내용 미리보기')}
          disabled={!prefs.notify}
          onChange={(v) => setPrefs({ notifyPreview: v })}
        />
      </div>
    </div>
  );
}

function TokensTab() {
  const client = useStore((s) => s.client);
  const call = useStore((s) => s.call);
  const spaces = useStore((s) => s.spaces);
  const toast = useStore((s) => s.toast);
  const [list, setList] = useState<TokenView[] | null>(null);
  const [name, setName] = useState('');
  const [scopes, setScopes] = useState<Scope[]>(['read']);
  const [limit, setLimit] = useState<string[]>([]);
  const [automation, setAutomation] = useState(true);
  const [created, setCreated] = useState<string | null>(null);
  const load = useCallback(() => client?.call('token.list', {}).then(setList).catch(() => setList([])), [client]);
  useEffect(() => {
    void load();
  }, [load]);
  const SCOPE_LABEL: Record<Scope, string> = {
    read: t('읽기'),
    write: t('쓰기'),
    admin: t('관리'),
    settings: t('설정'),
    owner: t('소유자(전부)'),
  };
  return (
    <div className="stack">
      <h3>{t('API 토큰')}</h3>
      <p className="muted small">
        {t('CLI·MCP(AI 비서)·스크립트가 로컬 API를 쓸 때 필요합니다. 자동화 토큰의 파괴적 작업(추방·삭제 등)은 사람 확인을 거칩니다.')}
      </p>
      <div className="card stack">
        <div className="grid2">
          <Field label={t('이름')}>
            <input className="input" value={name} maxLength={64} placeholder={t('예: 요약봇')} onChange={(e) => setName(e.target.value)} />
          </Field>
          <Field label={t('자동화(AI) 토큰')} hint={t('켜면 메시지에 AI 표시가 붙고 위험한 작업은 확인을 기다립니다.')}>
            <Toggle checked={automation} onChange={setAutomation} label={t('자동화')} />
          </Field>
        </div>
        <Field label={t('범위')}>
          <div className="chips">
            {SCOPES.map((s) => (
              <button key={s} type="button" className={cx('chip', scopes.includes(s) && 'active')} onClick={() => setScopes(scopes.includes(s) ? scopes.filter((x) => x !== s) : [...scopes, s])}>
                {SCOPE_LABEL[s]}
              </button>
            ))}
          </div>
        </Field>
        <Field label={t('공간 제한(고르지 않으면 전부)')}>
          <div className="chips">
            {spaces.map((s) => (
              <button key={s.space} type="button" className={cx('chip', limit.includes(s.space) && 'active')} onClick={() => setLimit(limit.includes(s.space) ? limit.filter((x) => x !== s.space) : [...limit, s.space])}>
                {s.name}
              </button>
            ))}
          </div>
        </Field>
        <button
          type="button"
          className="btn primary"
          disabled={!name.trim() || scopes.length === 0}
          onClick={async () => {
            if (!client) return;
            const r = await call(client.call('token.create', { name: name.trim(), scopes, spaces: limit.length ? limit : undefined, automation }));
            if (r) {
              setCreated(r.token);
              setName('');
              void load();
            }
          }}
        >
          {t('토큰 만들기')}
        </button>
        {created && (
          <div className="warn-box">
            <AlertTriangle size={16} />
            <div className="grow">
              <div>{t('이 토큰은 지금 한 번만 보여 줍니다. 안전한 곳에 복사해 두세요.')}</div>
              <code className="mono small break">{created}</code>
            </div>
            <button type="button" className="btn small" onClick={async () => (await copyText(created)) && toast('success', t('복사했습니다'))}>
              <Copy size={13} /> {t('복사')}
            </button>
            <button type="button" className="icon-btn small" onClick={() => setCreated(null)}>
              <X size={13} />
            </button>
          </div>
        )}
      </div>
      <div className="table">
        {list === null && <Spinner />}
        {list?.map((tk) => (
          <div key={tk.id} className="tr">
            <KeyRound size={18} />
            <div className="grow">
              <div className="row gap-s">
                <strong>{tk.name}</strong>
                {tk.automation && <span className="tag bot">AI</span>}
                {tk.scopes.map((s) => (
                  <span key={s} className="tag">
                    {SCOPE_LABEL[s]}
                  </span>
                ))}
              </div>
              <div className="muted small">
                {t('{when} 만듦', { when: formatRelative(tk.created_at) })} ·{' '}
                {tk.last_used_at ? t('{when} 사용', { when: formatRelative(tk.last_used_at) }) : t('사용 기록 없음')}
                {tk.spaces.length > 0 && ` · ${tk.spaces.map((s) => spaces.find((x) => x.space === s)?.name ?? '?').join(', ')}`}
              </div>
            </div>
            <button
              type="button"
              className="icon-btn danger"
              title={t('폐기')}
              onClick={async () => {
                if (!client) return;
                await call(client.call('token.revoke', { id: tk.id }), { ok: t('토큰을 폐기했습니다') });
                void load();
              }}
            >
              <Trash2 size={16} />
            </button>
          </div>
        ))}
      </div>
    </div>
  );
}

function ConfirmTab() {
  const list = useStore((s) => s.confirmations);
  const client = useStore((s) => s.client);
  const call = useStore((s) => s.call);
  const refresh = useStore((s) => s.refreshConfirmations);
  useEffect(() => {
    void refresh();
  }, [refresh]);
  const act = async (id: string, approve: boolean) => {
    if (!client) return;
    await call<unknown>(approve ? client.call('confirm.approve', { id }) : client.call('confirm.deny', { id }), {
      ok: approve ? t('승인해 실행했습니다') : t('거절했습니다'),
    });
    await refresh();
  };
  return (
    <div className="stack">
      <h3>{t('확인 대기')}</h3>
      <p className="muted small">{t('AI·자동화 토큰이 요청한 파괴적 작업입니다. 사람이 승인해야 실행됩니다.')}</p>
      {list.length === 0 && <p className="muted">{t('기다리는 요청이 없습니다.')}</p>}
      {list.map((c) => (
        <div key={c.id} className="card confirm-card">
          <div className="row between">
            <strong>{c.summary}</strong>
            <span className="tag bot">{c.token_name}</span>
          </div>
          <div className="muted small">
            <code>{c.method}</code> · {t('{when} 요청', { when: formatRelative(c.requested_at) })}
            {c.expires_at ? ' · ' + t('{when} 만료', { when: formatFull(c.expires_at) }) : ''}
          </div>
          <pre className="params">{JSON.stringify(c.params, null, 2).slice(0, 2000)}</pre>
          <div className="row gap">
            <button type="button" className="btn danger" onClick={() => void act(c.id, true)}>
              {t('승인하고 실행')}
            </button>
            <button type="button" className="btn ghost" onClick={() => void act(c.id, false)}>
              {t('거절')}
            </button>
          </div>
        </div>
      ))}
    </div>
  );
}

function AuditTab() {
  const client = useStore((s) => s.client);
  const [list, setList] = useState<AuditEntry[] | null>(null);
  const [autoOnly, setAutoOnly] = useState(false);
  useEffect(() => {
    void client?.call('audit.list', { limit: 200 }).then(setList).catch(() => setList([]));
  }, [client]);
  const shown = (list ?? []).filter((a) => !autoOnly || a.automation);
  return (
    <div className="stack">
      <div className="row between">
        <h3>{t('감사 기록')}</h3>
        <label className="check small">
          <input type="checkbox" checked={autoOnly} onChange={(e) => setAutoOnly(e.target.checked)} /> {t('자동화만')}
        </label>
      </div>
      <p className="muted small">{t('로컬 API 호출 기록(이 기기에만 저장, 내용 없이 메서드 이름만).')}</p>
      {list === null ? (
        <Spinner />
      ) : (
        <div className="audit">
          <div className="audit-row head">
            <span>{t('시각')}</span>
            <span>{t('토큰')}</span>
            <span>{t('메서드')}</span>
            <span>{t('결과')}</span>
          </div>
          {shown.map((a) => (
            <div key={a.id} className="audit-row">
              <span className="small">{formatFull(a.at)}</span>
              <span className="small">
                {a.token_name} {a.automation && <span className="tag bot">AI</span>}
              </span>
              <code className="small">{a.method}</code>
              <span className={cx('small', a.ok ? 'ok' : 'warn')}>{a.ok ? t('성공') : a.code === 1006 ? t('확인 대기') : t('실패 {code}', { code: a.code ?? '' })}</span>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}

function DiagTab() {
  const client = useStore((s) => s.client);
  const call = useStore((s) => s.call);
  const [m, setM] = useState<DiagMetrics | null>(null);
  const [leak, setLeak] = useState<{ clean: boolean; findings: string[] } | null>(null);
  useEffect(() => {
    if (!client) return;
    let stale = false;
    const load = () =>
      client
        .call('diag.metrics', {})
        .then((x) => !stale && setM(x))
        .catch(() => undefined);
    void load();
    const timer = setInterval(load, 2000);
    return () => {
      stale = true;
      clearInterval(timer);
    };
  }, [client]);
  return (
    <div className="stack">
      <h3>{t('진단')}</h3>
      <div className="note">
        <Shield size={16} />
        <div>{t('진단 값은 이 기기에서만 보며 어디에도 보내지 않습니다.')}</div>
      </div>
      {!m ? (
        <Spinner />
      ) : (
        <div className="metrics">
          <div className="metric">
            <span className="mv">{m.delivery_ms_p50}ms</span>
            <span className="ml">{t('전달 지연 p50')}</span>
          </div>
          <div className="metric">
            <span className="mv">{m.delivery_ms_p95}ms</span>
            <span className="ml">{t('전달 지연 p95')}</span>
          </div>
          <div className="metric">
            <span className="mv">{formatBytes(m.memory_bytes)}</span>
            <span className="ml">{t('데몬 메모리')}</span>
          </div>
          <div className="metric">
            <span className="mv">{m.cpu_percent.toFixed(1)}%</span>
            <span className="ml">CPU</span>
          </div>
          <div className="metric">
            <span className="mv">{m.events_per_sec.toFixed(1)}</span>
            <span className="ml">{t('이벤트/초')}</span>
          </div>
          {Object.entries(m.queues).map(([k, v]) => (
            <div key={k} className="metric">
              <span className="mv">{v}</span>
              <span className="ml">{t('대기열: {name}', { name: k })}</span>
            </div>
          ))}
        </div>
      )}
      <h3>{t('휘발성 누출 검사')}</h3>
      <p className="muted small">{t('임의 표식을 휘발성 경로에 흘려보낸 뒤 디스크·로그·캐시에 남았는지 데몬이 찾아봅니다.')}</p>
      <button
        type="button"
        className="btn"
        onClick={async () => {
          if (!client) return;
          const marker = `leak-${crypto.getRandomValues(new Uint32Array(2)).join('')}`;
          const r = await call(client.call('diag.leakcheck', { marker }));
          if (r) setLeak(r);
        }}
      >
        {t('검사하기')}
      </button>
      {leak && (
        <div className={leak.clean ? 'ok-box' : 'warn-box'}>
          {leak.clean ? <Check size={16} /> : <AlertTriangle size={16} />}
          {leak.clean ? t('깨끗합니다: 표식이 디스크·로그에 없습니다.') : t('발견: {findings}', { findings: leak.findings.join(', ') })}
        </div>
      )}
    </div>
  );
}

/** 원본 저장소. 고친 판은 자기 소스 주소를 더하되 이 주소와 저작권 표시는 남겨야 합니다(NOTICE.md, AGPL 7조 b). */
const SOURCE_URL = 'https://github.com/dhtpdud/YANGPA';

/** AGPL의 '적절한 법적 고지': 저작권, 보증 없음, 라이선스 보는 법, 소스 받는 곳(13조). */
function AboutTab() {
  const client = useStore((s) => s.client);
  const open = (path = '') => void client?.transport.openExternal(SOURCE_URL + path);
  return (
    <div className="stack">
      <h3>{t('정보')}</h3>
      <p>YANGPA © 2026 Oh Seyoung</p>
      <p className="muted small">{t('YANGPA는 자유 소프트웨어입니다. GNU Affero 일반 공중 사용 허가서(AGPL) 3판 또는 그 이후 판에 따라 고치고 다시 배포할 수 있습니다.')}</p>
      <p className="muted small">{t('이 프로그램은 어떤 보증도 없이 제공됩니다. 자세한 내용은 라이선스 전문을 보세요.')}</p>
      <p className="muted small">{t('후원 기능(후원 확인·라이브러리 탭·콘텐츠 뷰어·후원자 표시)의 코드는 AGPL이 아니라 YANGPA 후원 기능 라이선스를 따릅니다.')}</p>
      <div className="row gap">
        <button type="button" className="btn" onClick={() => open()}>
          {t('소스 코드')}
        </button>
        <button type="button" className="btn" onClick={() => open('/blob/main/LICENSE')}>
          {t('라이선스 전문')}
        </button>
        <button type="button" className="btn" onClick={() => open('/blob/main/LICENSE-SUPPORTER.md')}>
          {t('후원 기능 라이선스')}
        </button>
      </div>
    </div>
  );
}
