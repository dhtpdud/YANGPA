// 통화 협상용 작은 도우미(WebRTC 없이 시험할 수 있게 따로 둠).

/**
 * 비동기 작업을 들어온 순서대로 하나씩 돌립니다. 앞 작업이 실패해도 뒤 작업은 돕니다.
 * 협상 신호는 순서가 중요합니다(제안 → 후보). 동시에 보내거나 처리하면 후보가 제안보다 먼저 닿아 버려집니다.
 */
export class SerialQueue {
  private tail: Promise<void> = Promise.resolve();

  run<T>(task: () => Promise<T>): Promise<T> {
    const p = this.tail.then(task);
    this.tail = p.then(
      () => undefined,
      () => undefined,
    );
    return p;
  }
}

/**
 * 원격 설명(제안·응답)이 정해지기 전에 도착한 ICE 후보를 기기별로 잠시 모아 둡니다.
 * 상대가 보낸 값을 무한히 쌓지 않도록 기기 수와 기기당 후보 수에 상한을 둡니다.
 */
export class CandidateBuffer<C> {
  private readonly map = new Map<string, C[]>();

  constructor(
    private readonly maxPerDevice = 64,
    private readonly maxDevices = 64,
  ) {}

  /** 모아 두면 true, 상한에 걸려 버리면 false. */
  push(device: string, candidate: C): boolean {
    let list = this.map.get(device);
    if (!list) {
      if (this.map.size >= this.maxDevices) return false;
      list = [];
      this.map.set(device, list);
    }
    if (list.length >= this.maxPerDevice) return false;
    list.push(candidate);
    return true;
  }

  /** 모아 둔 후보를 꺼내고 비웁니다. */
  take(device: string): C[] {
    const list = this.map.get(device) ?? [];
    this.map.delete(device);
    return list;
  }

  delete(device: string) {
    this.map.delete(device);
  }

  clear() {
    this.map.clear();
  }

  size(device: string): number {
    return this.map.get(device)?.length ?? 0;
  }
}
