// 화면공유 PiP 관리(앱에 하나): 보던 화면을 통화 화면 밖에서도 이어 봅니다(voice/pip.ts).
// - 화면을 보다가 다른 채널로 가거나 창을 최소화하면(윈도, src-tauri/src/platform.rs가 알림) 저절로 PiP로 띄우고, 통화 화면으로
//   돌아오면 저절로 띄운 PiP를 닫습니다.
// - 보던 화면이 끝나거나 통화에서 나가면 닫고, 받는 스트림이 바뀌면 따라갑니다.
// - 휘발성 통화 채널은 PiP 창이 캡처 차단을 받지 않아 띄우지 않습니다.

import { useEffect, useRef } from 'react';
import { findChannel, useStore, type State } from '../store/store';
import { closePip, openPip, pipDevice, pipOpen, updatePip } from '../voice/pip';

const callVolatile = (s: State) => !!s.voice && findChannel(s.views, s.voice.space, s.voice.channel)?.settings.security === 'volatile';

/** 보는 화면(첫째)을 저절로 PiP로 띄웁니다. */
function openWatched() {
  const s = useStore.getState();
  const d = s.voice?.watching[0];
  const stream = d && !callVolatile(s) ? s.voiceSession()?.remoteScreen(d) : undefined;
  if (d && stream) void openPip(d, stream, true);
}

export function ScreenPip() {
  const voice = useStore((s) => s.voice);
  const voiceTick = useStore((s) => s.voiceTick);
  const selected = useStore((s) => s.selected);
  const volatile = useStore(callVolatile);
  const viewing = !!voice && selected.space === voice.space && selected.channel === voice.channel;
  const viewingRef = useRef(viewing);
  const was = useRef(viewing);

  useEffect(() => {
    const d = pipDevice();
    if (!d) return;
    const stream = !volatile && voice?.watching.includes(d) ? useStore.getState().voiceSession()?.remoteScreen(d) : undefined;
    if (stream) updatePip(stream);
    else closePip();
  }, [voice, voiceTick, volatile]);

  useEffect(() => {
    viewingRef.current = viewing;
    const left = was.current && !viewing;
    was.current = viewing;
    // 채널을 누른 직후라 웹뷰가 PiP를 열어 줍니다.
    if (viewing) closePip(true);
    else if (left && !pipOpen()) openWatched();
  }, [viewing]);

  useEffect(() => {
    const onMinimized = () => {
      if (!pipOpen()) openWatched();
    };
    // 창으로 돌아왔을 때 통화 화면이면 저절로 띄운 PiP는 닫습니다(타일에 그대로 보임).
    const onFocus = () => {
      if (viewingRef.current) closePip(true);
    };
    window.addEventListener('yangpa-minimized', onMinimized);
    window.addEventListener('focus', onFocus);
    return () => {
      window.removeEventListener('yangpa-minimized', onMinimized);
      window.removeEventListener('focus', onFocus);
    };
  }, []);
  return null;
}
