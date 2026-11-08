// 받기 속도 어림: blob.progress(받은 조각 수)가 올 때마다 시각을 적어 최근 구간의 평균을 냅니다.
// Tor로 받으면 조각이 몇 초에 하나씩 와서 두 알림 사이만 보면 들쭉날쭉하므로 최근 20초(적어도 표본 둘)를 봅니다.

export interface RateSample {
  at: number;
  have: number;
}

/** 평균을 내는 최근 구간. */
export const RATE_WINDOW_MS = 20_000;
/** 들고 있는 표본 수 상한. */
export const RATE_MAX_SAMPLES = 16;
/** 마지막 조각 뒤 이만큼(또는 평소 간격의 세 배) 넘게 조용하면 멈춘 것으로 보고 속도를 숨깁니다(데몬 STALL_MS와 같음). */
export const RATE_STALE_MS = 15_000;
/** 이보다 오래 끊겼다가 다시 오면(다시 시도) 처음부터 잽니다. */
export const RATE_RESET_MS = 60_000;

/** 표본 하나를 더합니다. 같은 조각 수는 무시하고(같은 첨부를 여러 줄이 그려도 한 번만), 줄었거나 오래 끊겼으면 새로 시작합니다. */
export function addSample(samples: readonly RateSample[], at: number, have: number): RateSample[] {
  const last = samples.at(-1);
  if (!last || have < last.have || at - last.at > RATE_RESET_MS) return [{ at, have }];
  if (have === last.have) return samples as RateSample[];
  const next = [...samples, { at, have }];
  // 아주 느려도 숫자를 낼 수 있게 표본 둘은 남깁니다.
  while (next.length > 2 && (next.length > RATE_MAX_SAMPLES || at - next[0].at > RATE_WINDOW_MS)) next.shift();
  return next;
}

/** 초당 조각 수. 표본이 모자라거나 멈춘 것 같으면 null. */
export function chunkRate(samples: readonly RateSample[], now: number): number | null {
  if (samples.length < 2) return null;
  const first = samples[0];
  const last = samples[samples.length - 1];
  const span = last.at - first.at;
  const moved = last.have - first.have;
  if (span <= 0 || moved <= 0) return null;
  const gap = span / (samples.length - 1);
  if (now - last.at > Math.max(RATE_STALE_MS, 3 * gap)) return null;
  // 다음 조각이 평소 간격의 두 배 넘게 늦으면 지금까지 시간으로 나눠 숫자가 내려가게 합니다.
  const elapsed = now - last.at > 2 * gap ? now - first.at : span;
  return (moved * 1000) / elapsed;
}
