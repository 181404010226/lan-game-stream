package dev.lanstream.mobile;

final class FrameClock {
    static final long DROP = -1;
    private static final long UNSET = Long.MIN_VALUE;
    private final long interval;
    private long offset = UNSET;
    private long last = UNSET;

    FrameClock(int fps) { interval = 1_000_000_000L / fps; }

    void reset() { offset = last = UNSET; }

    long target(long ptsUs, long now) {
        long pts = ptsUs * 1000;
        if (offset == UNSET) offset = now + interval - pts;
        long due = offset + pts;
        if (due < now - interval) {
            if (last != UNSET && now - last < interval / 2) return DROP;
            offset = now + interval / 2 - pts;
            due = offset + pts;
        } else if (due > now + interval * 2) {
            if (last != UNSET && last > now + interval) return DROP;
            offset = now + interval - pts;
            due = offset + pts;
        }
        due = Math.max(now, due);
        if (last != UNSET && due <= last) due = last + interval / 2;
        if (due > now + interval * 2) return DROP;
        last = due;
        return due;
    }
}
