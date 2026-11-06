type ProgressSubscriber = {
  reading: boolean;
  poll: (isActive: () => boolean) => Promise<void>;
};
type PollingGroup = {
  subscribers: Set<ProgressSubscriber>;
  timer: NodeJS.Timeout;
};

// Share by cycle: The product has only one 1-second poller by default, and the internal custom cycle does not change the frequency of other tasks.
const pollingGroups = new Map<number, PollingGroup>();

export function subscribeBashOutputProgress(
  intervalMs: number,
  poll: ProgressSubscriber["poll"],
): () => void {
  let group = pollingGroups.get(intervalMs);
  if (!group) {
    const subscribers = new Set<ProgressSubscriber>();
    const timer = setInterval(() => {
      for (const subscriber of subscribers) {
        if (subscriber.reading) continue;
        subscriber.reading = true;
        const isActive = () => subscribers.has(subscriber);
        void Promise.resolve()
          .then(() => {
            if (isActive()) return subscriber.poll(isActive);
          })
// Preview is a best-effort read; failure of a single file or callback cannot stop other Bash's sharing progress.
          .catch(() => undefined)
          .finally(() => {
            subscriber.reading = false;
          });
      }
    }, intervalMs);
    timer.unref();
    group = { subscribers, timer };
    pollingGroups.set(intervalMs, group);
  }
  const subscriber = { reading: false, poll };
  const { subscribers, timer } = group;
  subscribers.add(subscriber);
  return () => {
// New pollers with the same cycle cannot be deleted when old subscriptions are cleaned up repeatedly.
    if (!subscribers.delete(subscriber) || subscribers.size > 0) return;
    clearInterval(timer);
    pollingGroups.delete(intervalMs);
  };
}
