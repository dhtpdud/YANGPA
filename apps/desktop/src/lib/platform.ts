/** Android 앱인지. 데스크톱 전용 기능과 Play가 막는 외부 결제 안내를 뺄 때 씁니다. */
export function isAndroid(): boolean {
  return typeof navigator !== 'undefined' && navigator.userAgent.includes('Android');
}

/** 한 칸씩 보여 주는 좁은 화면인지(app.css의 `max-width: 720px`와 같게). */
export function isNarrow(): boolean {
  return typeof matchMedia === 'function' && matchMedia('(max-width: 720px)').matches;
}

/** 손가락으로 쓰는 기기인지(보내기 단추를 쓰고 Enter는 줄바꿈). */
export function isTouch(): boolean {
  return typeof matchMedia === 'function' && matchMedia('(pointer: coarse)').matches;
}
