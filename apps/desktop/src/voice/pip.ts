// 화면공유 PiP(작은 창으로 보기). 통화 화면의 타일은 다른 채널로 가면 사라지므로(PiP도 함께 닫힘) React 밖의 영상 요소
// 하나로 띄웁니다. 웹뷰는 사용자 동작(누름) 뒤 몇 초 안에만 PiP를 열어 줍니다. PiP 창은 앱 창의 캡처 차단을 받지 않으므로
// 휘발성 통화 채널에서는 부르는 쪽이 띄우지 않습니다.

let video: HTMLVideoElement | null = null;
/** 지금 PiP로 보는 화면의 기기. */
let device: string | null = null;
/** 저절로 띄운 PiP인지(통화 화면으로 돌아오면 닫음). 직접 누른 것은 사용자가 닫을 때까지 둡니다. */
let auto = false;

export function pipSupported(): boolean {
  return typeof document !== 'undefined' && document.pictureInPictureEnabled === true;
}

function element(): HTMLVideoElement {
  if (!video) {
    const v = document.createElement('video');
    v.muted = true;
    v.playsInline = true;
    // 화면 밖에 둡니다(display:none이면 영상을 읽지 않는 웹뷰가 있음).
    v.style.cssText = 'position:fixed;left:-9999px;top:0;width:2px;height:2px;pointer-events:none';
    v.setAttribute('aria-hidden', 'true');
    v.addEventListener('leavepictureinpicture', () => {
      device = null;
      auto = false;
      v.srcObject = null;
    });
    document.body.append(v);
    video = v;
  }
  return video;
}

export function pipOpen(): boolean {
  return !!video && document.pictureInPictureElement === video;
}

/** PiP로 보는 화면의 기기(열려 있지 않으면 null). */
export function pipDevice(): string | null {
  return pipOpen() ? device : null;
}

/** 이 화면을 PiP로 띄웁니다(이미 열렸으면 화면만 바꿈). 못 열면 false. */
export async function openPip(dev: string, stream: MediaStream, isAuto = false): Promise<boolean> {
  if (!pipSupported()) return false;
  const v = element();
  const wasOpen = pipOpen();
  try {
    if (v.srcObject !== stream) v.srcObject = stream;
    // 영상 크기를 알아야 PiP가 열립니다. 화면이 아직 오지 않으면 사용자 동작이 식기 전에 그만둡니다.
    await Promise.race([v.play(), new Promise((_, reject) => setTimeout(() => reject(new Error('timeout')), 3000))]);
    if (!pipOpen()) await v.requestPictureInPicture();
  } catch {
    if (!pipOpen()) v.srcObject = null;
    return false;
  }
  device = dev;
  if (!wasOpen) auto = isAuto;
  return true;
}

/** 열린 PiP의 화면을 바꿉니다(받는 길이 바뀌어 스트림이 새로 생김). */
export function updatePip(stream: MediaStream) {
  if (!pipOpen() || !video || video.srcObject === stream) return;
  video.srcObject = stream;
  void video.play().catch(() => undefined);
}

/** PiP를 닫습니다. `onlyAuto`면 저절로 띄운 것만. */
export function closePip(onlyAuto = false) {
  if (!pipOpen() || (onlyAuto && !auto)) return;
  void document.exitPictureInPicture().catch(() => undefined);
}
