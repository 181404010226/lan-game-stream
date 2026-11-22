package dev.lanstream.mobile;

import java.io.IOException;

final class NativeBridge {
    static {
        System.loadLibrary("lan_stream");
    }

    static native long open(String hostHint, int port, String game, String key, int width, int height, boolean udp) throws IOException;
    static native String info(long session);
    static native byte[] read(long session) throws IOException;
    static native byte[] readPicture(long session) throws IOException;
    static native void ping(long session, long timestamp);
    static native void keys(long session, int[] pressed);
    static native void gamepad(long session, int buttons, int x, int y);
    static native void pointer(long session, int action, int x, int y);
    static native void close(long session);

    private NativeBridge() {}
}
