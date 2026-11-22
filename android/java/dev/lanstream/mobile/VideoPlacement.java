package dev.lanstream.mobile;

final class VideoPlacement {
    enum Strategy { FIT, FILL }

    final float left, top, width, height;
    final float pictureLeft, pictureTop, pictureWidth, pictureHeight;

    private VideoPlacement(float left, float top, float width, float height,
                           float pictureLeft, float pictureTop, float pictureWidth, float pictureHeight) {
        this.left = left; this.top = top; this.width = width; this.height = height;
        this.pictureLeft = pictureLeft; this.pictureTop = pictureTop;
        this.pictureWidth = pictureWidth; this.pictureHeight = pictureHeight;
    }

    static VideoPlacement place(int screenWidth, int screenHeight, int sourceWidth, int sourceHeight,
                                float insetX, float insetY, float rail, Strategy strategy) {
        float innerWidth = sourceWidth * (1 - 2 * insetX);
        float innerHeight = sourceHeight * (1 - 2 * insetY);
        float maxX = (screenWidth - 2 * rail) / innerWidth;
        float maxY = screenHeight / innerHeight;
        float scale = strategy == Strategy.FILL ? Math.max(maxX, maxY) : Math.min(maxX, maxY);
        float pictureWidth = innerWidth * scale, pictureHeight = innerHeight * scale;
        float pictureLeft = (screenWidth - pictureWidth) / 2;
        float pictureTop = (screenHeight - pictureHeight) / 2;
        return new VideoPlacement(pictureLeft - sourceWidth * insetX * scale, pictureTop - sourceHeight * insetY * scale,
            sourceWidth * scale, sourceHeight * scale, pictureLeft, pictureTop, pictureWidth, pictureHeight);
    }
}
