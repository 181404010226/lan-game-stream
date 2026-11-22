package dev.lanstream.mobile;

import android.media.AudioAttributes;
import android.media.AudioFormat;
import android.media.AudioTrack;
import android.media.MediaCodec;
import android.media.MediaCodecInfo;
import android.media.MediaFormat;
import android.os.Build;
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
    interface Observer { void ended(String reason); }
    interface PictureBounds { void updated(float x, float y); }
    private final long session;
    private final Surface surface;
    private final int width, height, fps;
    private final Observer observer;
    private final PictureBounds pictureBounds;
    private final ArrayBlockingQueue<byte[]> audio = new ArrayBlockingQueue<>(7);
    private final ArrayBlockingQueue<Picture> video = new ArrayBlockingQueue<>(12);
    private final AtomicBoolean active = new AtomicBoolean(true);
    private volatile boolean muted;
    private AudioTrack liveTrack;
    private long lastStats;
    private int rendered;

    MediaRunner(long session, Surface surface, int width, int height, int fps, boolean muted, Observer observer, PictureBounds pictureBounds) {
        this.session = session; this.surface = surface; this.width = width; this.height = height; this.fps = fps;
        this.observer = observer; this.muted = muted;
        this.pictureBounds = pictureBounds;
    }

    void start() {
        new Thread(this::readPackets, "lan-read").start();
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
        boolean resync = false, audioStarted = false;
        try {
            while (active.get()) {
                byte[] packet = NativeBridge.read(session);
                if (packet == null) break;
                switch (packet[0]) {
                    case 2:
                        long timestamp = frames++ * 1_000_000L / fps;
                        if (!resync && !video.offer(new Picture(packet, timestamp, false))) {
                            video.clear();
                            resync = true;
                        }
                        if (resync && isIdr(packet)) {
                            video.offer(new Picture(packet, timestamp, true));
                            resync = false;
                        }
                        break;
                    case 3:
                        if (!audioStarted) {
                            audioStarted = true;
                            new Thread(this::playAudio, "lan-audio").start();
                        }
                        if (!audio.offer(packet)) { audio.poll(); audio.offer(packet); }
                        break;
                    case 4:
                        end(new String(packet, 1, packet.length - 1, StandardCharsets.UTF_8));
                        return;
                    case 7:
                        if (packet.length == 5) {
                            int x = ((packet[1] & 255) << 8) | (packet[2] & 255);
                            int y = ((packet[3] & 255) << 8) | (packet[4] & 255);
                            if (x <= 16384 && y <= 16384) pictureBounds.updated(x / 65535f, y / 65535f);
                        }
                        break;
                    default:
                        throw new IOException("收到未知的串流封包");
                }
            }
            end("主机已断开，正在重新连接…");
        } catch (Exception failure) {
            if (active.get()) Log.i("LanStream", "Connection ended: " + failure.getMessage());
            end("串流中断，正在重新连接…");
        }
    }

    private void decodeVideo() {
        MediaCodec decoder = null;
        MediaCodec.BufferInfo output = new MediaCodec.BufferInfo();
        try {
            while (active.get()) {
                Picture picture = video.poll(40, TimeUnit.MILLISECONDS);
                if (picture == null) {
                    if (decoder != null) drain(decoder, output);
                    continue;
                }
                if (decoder == null) decoder = openDecoder(picture.packet);
                else if (picture.reset) decoder.flush();
                int slot;
                do {
                    slot = decoder.dequeueInputBuffer(8000);
                    if (slot < 0) drain(decoder, output);
                } while (slot < 0 && active.get());
                if (slot < 0 || !active.get()) break;
                ByteBuffer input = decoder.getInputBuffer(slot);
                if (input == null || input.capacity() < picture.packet.length - 1) throw new IOException("解码缓冲区不足");
                input.clear();
                input.put(picture.packet, 1, picture.packet.length - 1);
                decoder.queueInputBuffer(slot, 0, picture.packet.length - 1, picture.timestamp, 0);
                drain(decoder, output);
            }
        } catch (Exception failure) {
            if (active.get()) Log.w("LanStream", "Decoder ended", failure);
            end("手机解码中断，正在重新连接…");
        } finally {
            if (decoder != null) {
                try { decoder.stop(); } catch (IllegalStateException ignored) {}
                decoder.release();
            }
            surface.release();
        }
    }

    private void end(String reason) {
        if (!active.getAndSet(false)) return;
        NativeBridge.close(session);
        observer.ended(reason);
    }

    private static final class Picture {
        final byte[] packet;
        final long timestamp;
        final boolean reset;

        Picture(byte[] packet, long timestamp, boolean reset) {
            this.packet = packet; this.timestamp = timestamp; this.reset = reset;
        }
    }

    private static boolean isIdr(byte[] frame) {
        int nalu = start(frame, 1);
        while (nalu >= 0) {
            int header = nalu + (frame[nalu + 2] == 1 ? 3 : 4);
            if (header >= frame.length) return false;
            if ((frame[header] & 31) == 5) return true;
            nalu = start(frame, header + 1);
        }
        return false;
    }

    private MediaCodec openDecoder(byte[] first) throws IOException {
        MediaCodec codec = MediaCodec.createDecoderByType(MediaFormat.MIMETYPE_VIDEO_AVC);
        try {
            MediaFormat format = MediaFormat.createVideoFormat(MediaFormat.MIMETYPE_VIDEO_AVC, width, height);
            format.setInteger(MediaFormat.KEY_MAX_INPUT_SIZE, 2 * 1024 * 1024);
            if (Build.VERSION.SDK_INT >= 30 && codec.getCodecInfo().getCapabilitiesForType(MediaFormat.MIMETYPE_VIDEO_AVC)
                .isFeatureSupported(MediaCodecInfo.CodecCapabilities.FEATURE_LowLatency)) {
                format.setInteger(MediaFormat.KEY_LOW_LATENCY, 1);
            }
            int nalu = start(first, 1);
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
            if (newest >= 0) decoder.releaseOutputBuffer(newest, false);
            newest = index;
        }
        if (newest >= 0) {
            decoder.releaseOutputBuffer(newest, true);
            long now = System.nanoTime();
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
