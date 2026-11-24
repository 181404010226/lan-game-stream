package dev.lanstream.mobile;

import android.media.AudioAttributes;
import android.media.AudioFormat;
import android.media.AudioTrack;
import android.media.MediaCodec;
import android.media.MediaCodecInfo;
import android.media.MediaFormat;
import android.os.Build;
import android.os.Handler;
import android.os.HandlerThread;
import android.util.Log;
import android.view.Surface;
import java.io.IOException;
import java.nio.ByteBuffer;
import java.nio.charset.StandardCharsets;
import java.util.Arrays;
import java.util.concurrent.ArrayBlockingQueue;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicBoolean;

final class MediaRunner {
    interface Observer { void ended(String reason, boolean tcpFallback); }
    interface PictureBounds { void updated(float x, float y); }
    private final long session;
    private final Surface surface;
    private final int width, height, fps;
    private final boolean udpVideo;
    private final boolean immediate;
    private final boolean ownsSurface;
    private final Observer observer;
    private final PictureBounds pictureBounds;
    private final ArrayBlockingQueue<byte[]> audio = new ArrayBlockingQueue<>(7);
    private final ArrayBlockingQueue<Picture> video = new ArrayBlockingQueue<>(9);
    private final FrameClock clock;
    private final AtomicBoolean active = new AtomicBoolean(true);
    private volatile boolean muted;
    private AudioTrack liveTrack;
    private long lastStats;
    private int rendered;
    private long previousSequence = -1;
    private boolean resync;
    private long resyncSince;
    private long displayStart, lastDisplay;
    private int displayed, longGaps;
    private final long[] displayIntervals = new long[1024];
    private int intervalCount;
    private long receiveStart, missingFrames;
    private int receivedFrames, queueDrops;

    MediaRunner(long session, Surface surface, int width, int height, int fps, boolean muted, boolean udpVideo, boolean immediate, boolean ownsSurface, Observer observer, PictureBounds pictureBounds) {
        this.session = session; this.surface = surface; this.width = width; this.height = height; this.fps = fps;
        this.observer = observer; this.muted = muted;
        this.pictureBounds = pictureBounds;
        this.udpVideo = udpVideo;
        this.immediate = immediate;
        this.ownsSurface = ownsSurface;
        clock = new FrameClock(fps);
    }

    void start() {
        if (Build.VERSION.SDK_INT >= 30) {
            try { surface.setFrameRate(fps, Surface.FRAME_RATE_COMPATIBILITY_FIXED_SOURCE); } catch (RuntimeException ignored) {}
        }
        new Thread(this::readPackets, "lan-read").start();
        if (udpVideo) new Thread(this::readDatagrams, "lan-udp").start();
        new Thread(this::decodeVideo, "lan-video").start();
    }

    void stop() {
        if (active.getAndSet(false)) NativeBridge.close(session);
    }

    synchronized void mute(boolean value) {
        muted = value;
        if (liveTrack != null) liveTrack.setVolume(muted ? 0f : 1f);
    }

    private void readPackets() {
        long frames = 0;
        long pingAt = 0;
        boolean audioStarted = false;
        try {
            while (active.get()) {
                long now = System.nanoTime();
                if (udpVideo && now >= pingAt) { NativeBridge.ping(session, now); pingAt = now + 1_000_000_000L; }
                byte[] packet = NativeBridge.read(session);
                if (packet == null) break;
                switch (packet[0]) {
                    case 2:
                        if (!udpVideo) submit(packet, 1, frames++);
                        break;
                    case 3:
                        if (!audioStarted) {
                            audioStarted = true;
                            new Thread(this::playAudio, "lan-audio").start();
                        }
                        if (!audio.offer(packet)) { audio.poll(); audio.offer(packet); }
                        break;
                    case 4:
                        end(new String(packet, 1, packet.length - 1, StandardCharsets.UTF_8), false);
                        return;
                    case 7:
                        if (packet.length == 5) {
                            int x = ((packet[1] & 255) << 8) | (packet[2] & 255);
                            int y = ((packet[3] & 255) << 8) | (packet[4] & 255);
                            if (x <= 16384 && y <= 16384) pictureBounds.updated(x / 65535f, y / 65535f);
                        }
                        break;
                    case 11:
                        break;
                    case 12:
                        if (packet.length == 9 && udpVideo) {
                            long sent = ByteBuffer.wrap(packet, 1, 8).getLong();
                            long roundTripUs = (System.nanoTime() - sent) / 1000;
                            if (roundTripUs >= 0 && roundTripUs < 5_000_000L) Log.i("LanStream", "Control RTT us=" + roundTripUs);
                        }
                        break;
                    default:
                        throw new IOException("收到未知的串流封包");
                }
            }
            end("主机已断开，正在重新连接…", false);
        } catch (Exception failure) {
            if (active.get()) Log.i("LanStream", "Connection ended: " + failure.getMessage());
            end("串流中断，正在重新连接…", false);
        }
    }

    private void readDatagrams() {
        android.os.Process.setThreadPriority(android.os.Process.THREAD_PRIORITY_DISPLAY);
        try {
            while (active.get()) {
                byte[] packet = NativeBridge.readPicture(session);
                if (packet == null) break;
                if (packet.length > 4) {
                    long sequence = ((packet[0] & 255L) << 24) | ((packet[1] & 255L) << 16)
                        | ((packet[2] & 255L) << 8) | (packet[3] & 255L);
                    submit(packet, 4, sequence);
                }
            }
        } catch (IOException lost) {
            if (active.get()) Log.i("LanStream", "UDP video lost: " + lost.getMessage());
        }
        end("正在切换到可靠视频连接…", true);
    }

    private void submit(byte[] packet, int offset, long sequence) {
        long submittedAt = System.nanoTime();
        if (receiveStart == 0) receiveStart = submittedAt;
        receivedFrames++;
        if (sequence != previousSequence + 1) {
            missingFrames += Math.max(0, sequence - previousSequence - 1);
            video.clear();
            resync = true;
        }
        previousSequence = sequence;
        long timestamp = sequence * 1_000_000L / fps;
        if (!resync && !video.offer(new Picture(packet, offset, timestamp, false))) {
            queueDrops++;
            video.clear();
            resync = true;
        }
        if (resync && isIdr(packet, offset)) {
            video.offer(new Picture(packet, offset, timestamp, true));
            resync = false;
        }
        if (submittedAt - receiveStart >= 5_000_000_000L) {
            int rate = (int)Math.round(receivedFrames * 1_000_000_000.0 / (submittedAt - receiveStart));
            Log.i("LanStream", "Receive fps " + rate + " missing=" + missingFrames + " queue-overruns=" + queueDrops);
            receiveStart = submittedAt;
            missingFrames = receivedFrames = queueDrops = 0;
        }
        if (!resync) { resyncSince = 0; return; }
        if (resyncSince == 0) resyncSince = submittedAt;
        else if (udpVideo && submittedAt - resyncSince > 950_000_000L) end("正在切换到可靠视频连接…", true);
    }

    void displayedFrame(long now) {
        if (now <= lastDisplay) return;
        if (displayStart == 0) displayStart = now;
        if (lastDisplay != 0) {
            long interval = now - lastDisplay;
            if (interval > 1_500_000_000L / fps) longGaps++;
            if (intervalCount < displayIntervals.length) displayIntervals[intervalCount++] = interval;
        }
        lastDisplay = now;
        displayed++;
        if (now - displayStart >= 5_000_000_000L) {
            Arrays.sort(displayIntervals, 0, intervalCount);
            long p95Us = intervalCount == 0 ? 0 : displayIntervals[(intervalCount * 95 - 1) / 100] / 1000;
            Log.i("LanStream", "Display fps " + Math.round(displayed * 1_000_000_000.0 / (now - displayStart))
                + " gaps=" + longGaps + " p95-gap-us=" + p95Us);
            displayStart = now;
            displayed = longGaps = 0;
            intervalCount = 0;
        }
    }

    private void decodeVideo() {
        android.os.Process.setThreadPriority(android.os.Process.THREAD_PRIORITY_DISPLAY);
        MediaCodec decoder = null;
        MediaCodec.BufferInfo output = new MediaCodec.BufferInfo();
        HandlerThread rendererEvents = null;
        try {
            if (!ownsSurface) {
                rendererEvents = new HandlerThread("lan-display-stats");
                rendererEvents.start();
            }
            while (active.get()) {
                Picture picture = video.poll(decoder == null ? 40 : immediate ? 2 : 4, TimeUnit.MILLISECONDS);
                if (picture == null) {
                    if (decoder != null) drain(decoder, output);
                    continue;
                }
                if (decoder == null) {
                    decoder = openDecoder(picture.packet, picture.offset);
                    if (rendererEvents != null) decoder.setOnFrameRenderedListener((codec, pts, time) -> displayedFrame(time), new Handler(rendererEvents.getLooper()));
                }
                else if (picture.reset) { decoder.flush(); clock.reset(); }
                int slot;
                do {
                    slot = decoder.dequeueInputBuffer(8000);
                    if (slot < 0) drain(decoder, output);
                } while (slot < 0 && active.get());
                if (slot < 0 || !active.get()) break;
                ByteBuffer input = decoder.getInputBuffer(slot);
                int length = picture.packet.length - picture.offset;
                if (input == null || input.capacity() < length) throw new IOException("解码缓冲区不足");
                input.clear();
                input.put(picture.packet, picture.offset, length);
                decoder.queueInputBuffer(slot, 0, length, picture.timestamp, 0);
                drain(decoder, output);
            }
        } catch (Exception failure) {
            if (active.get()) Log.w("LanStream", "Decoder ended", failure);
            end("手机解码中断，正在重新连接…", udpVideo && (width > 1920 || height > 1080));
        } finally {
            if (decoder != null) {
                try { decoder.stop(); } catch (IllegalStateException ignored) {}
                decoder.release();
            }
            if (rendererEvents != null) rendererEvents.quitSafely();
            if (ownsSurface) surface.release();
        }
    }

    private void end(String reason, boolean tcpFallback) {
        if (!active.getAndSet(false)) return;
        NativeBridge.close(session);
        observer.ended(reason, tcpFallback);
    }

    private static final class Picture {
        final byte[] packet;
        final int offset;
        final long timestamp;
        final boolean reset;

        Picture(byte[] packet, int offset, long timestamp, boolean reset) {
            this.packet = packet; this.offset = offset; this.timestamp = timestamp; this.reset = reset;
        }
    }

    private static boolean isIdr(byte[] frame, int offset) {
        int nalu = start(frame, offset);
        while (nalu >= 0) {
            int header = nalu + (frame[nalu + 2] == 1 ? 3 : 4);
            if (header >= frame.length) return false;
            if ((frame[header] & 31) == 5) return true;
            nalu = start(frame, header + 1);
        }
        return false;
    }

    private MediaCodec openDecoder(byte[] first, int offset) throws IOException {
        MediaCodec codec = MediaCodec.createDecoderByType(MediaFormat.MIMETYPE_VIDEO_AVC);
        try {
            MediaFormat format = MediaFormat.createVideoFormat(MediaFormat.MIMETYPE_VIDEO_AVC, width, height);
            format.setInteger(MediaFormat.KEY_MAX_INPUT_SIZE, 2 * 1024 * 1024);
            format.setInteger(MediaFormat.KEY_PRIORITY, 0);
            format.setInteger(MediaFormat.KEY_OPERATING_RATE, fps);
            if (Build.VERSION.SDK_INT >= 30 && codec.getCodecInfo().getCapabilitiesForType(MediaFormat.MIMETYPE_VIDEO_AVC)
                .isFeatureSupported(MediaCodecInfo.CodecCapabilities.FEATURE_LowLatency)) {
                format.setInteger(MediaFormat.KEY_LOW_LATENCY, 1);
            }
            int nalu = start(first, offset);
            while (nalu >= 0) {
                int header = nalu + (first[nalu + 2] == 1 ? 3 : 4);
                if (header == first.length) break;
                int next = start(first, header + 1);
                int type = first[header] & 31;
                if (type == 7 || type == 8) {
                    format.setByteBuffer(type == 7 ? "csd-0" : "csd-1",
                        ByteBuffer.wrap(Arrays.copyOfRange(first, nalu, next < 0 ? first.length : next)));
                }
                nalu = next;
            }
            codec.configure(format, surface, null, 0);
            codec.start();
            Log.i("LanStream", "AVC decoder " + codec.getName() + " " + width + "x" + height);
            return codec;
        } catch (RuntimeException failure) {
            codec.release();
            throw failure;
        }
    }

    private static int start(byte[] frame, int from) {
        for (int i = from; i + 3 < frame.length; i++) {
            if (frame[i] == 0 && frame[i + 1] == 0 && (frame[i + 2] == 1 || (frame[i + 2] == 0 && frame[i + 3] == 1))) return i;
        }
        return -1;
    }

    private void drain(MediaCodec decoder, MediaCodec.BufferInfo output) {
        int newest = -1;
        while (true) {
            int index = decoder.dequeueOutputBuffer(output, 0);
            if (index == MediaCodec.INFO_OUTPUT_FORMAT_CHANGED) continue;
            if (index < 0) break;
            if ((output.flags & MediaCodec.BUFFER_FLAG_CODEC_CONFIG) != 0) { decoder.releaseOutputBuffer(index, false); continue; }
            if (immediate) {
                if (newest >= 0) decoder.releaseOutputBuffer(newest, false);
                newest = index;
                continue;
            }
            long now = System.nanoTime();
            long due = clock.target(output.presentationTimeUs, now);
            if (due == FrameClock.DROP) { decoder.releaseOutputBuffer(index, false); continue; }
            decoder.releaseOutputBuffer(index, due);
            submitted(now);
        }
        if (newest >= 0) { decoder.releaseOutputBuffer(newest, true); submitted(System.nanoTime()); }
    }

    private void submitted(long now) {
        if (lastStats == 0) {
            lastStats = now;
            Log.i("LanStream", "First frame rendered");
        }
        rendered++;
        if (now - lastStats >= 5_000_000_000L) {
            Log.i("LanStream", "Rendered fps " + Math.round(rendered * 1_000_000_000.0 / (now - lastStats)));
            lastStats = now;
            rendered = 0;
        }
    }

    private void playAudio() {
        AudioTrack track = null;
        try {
            int bytes = AudioTrack.getMinBufferSize(48000, AudioFormat.CHANNEL_OUT_STEREO, AudioFormat.ENCODING_PCM_16BIT);
            if (bytes < 0) return;
            AudioFormat format = new AudioFormat.Builder().setSampleRate(48000).setChannelMask(AudioFormat.CHANNEL_OUT_STEREO)
                .setEncoding(AudioFormat.ENCODING_PCM_16BIT).build();
            track = new AudioTrack.Builder().setAudioAttributes(new AudioAttributes.Builder().setUsage(AudioAttributes.USAGE_GAME)
                .setContentType(AudioAttributes.CONTENT_TYPE_MUSIC).build()).setAudioFormat(format)
                .setBufferSizeInBytes(Math.max(bytes, 5760)).setPerformanceMode(AudioTrack.PERFORMANCE_MODE_LOW_LATENCY)
                .setTransferMode(AudioTrack.MODE_STREAM).build();
            synchronized (this) { liveTrack = track; track.setVolume(muted ? 0f : 1f); }
            track.play();
            Log.i("LanStream", "AudioTrack playing 48000 Hz stereo");
            while (active.get()) {
                byte[] packet = audio.poll(100, TimeUnit.MILLISECONDS);
                if (packet != null && track.write(packet, 1, packet.length - 1) < 0) break;
            }
        } catch (Exception failure) {
            Log.w("LanStream", "AudioTrack unavailable", failure);
        } finally {
            synchronized (this) { liveTrack = null; }
            if (track != null) track.release();
        }
    }
}
