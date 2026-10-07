package android.media;
import android.graphics.Bitmap;
public final class MediaMetadataRetriever {
    public static final int METADATA_KEY_VIDEO_WIDTH = 18, METADATA_KEY_VIDEO_HEIGHT = 19, OPTION_CLOSEST_SYNC = 2;
    public static int width = 1920, height = 1080, released, frameRequests;
    public static long delay;
    public static boolean malformed;
    public void setDataSource(MediaDataSource source) {
        // JNI range authentication/budgets are exercised by the Rust fixture tests.
        if (malformed) throw new IllegalArgumentException("malformed source");
    }
    public String extractMetadata(int key) { return Integer.toString(key == METADATA_KEY_VIDEO_WIDTH ? width : height); }
    public Bitmap getScaledFrameAtTime(long time, int option, int w, int h) {
        if (time != 0 || option != OPTION_CLOSEST_SYNC || w > 384 || h > 384) throw new AssertionError("first frame/dimension contract");
        frameRequests++;
        try { Thread.sleep(delay); } catch (InterruptedException e) { throw new IllegalStateException(e); }
        return new Bitmap(w, h);
    }
    public void release() { released++; }
}
