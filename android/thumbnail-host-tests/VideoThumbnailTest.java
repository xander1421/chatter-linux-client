import com.chatter.client.VideoThumbnail;
import android.graphics.Bitmap;
import android.media.MediaMetadataRetriever;
import android.os.Build;

/** Runs production Kotlin against deterministic framework doubles; no device/media access. */
public final class VideoThumbnailTest {
    private static void check(boolean value, String message) { if (!value) throw new AssertionError(message); }
    public static void main(String[] args) throws Exception {
        VideoThumbnail helper = new VideoThumbnail();
        byte[] frame = helper.extract(1);
        check(frame != null && frame.length == 100, "bounded frame output");
        check(Bitmap.recycled == 1 && MediaMetadataRetriever.released == 1, "release successful decoder and bitmap");
        Bitmap.encodedBytes = 256 * 1024 + 1;
        check(helper.extract(2) == null, "encoded byte budget");
        check(Bitmap.recycled == 2 && MediaMetadataRetriever.released == 2, "release failed compression");
        Bitmap.encodedBytes = 100;
        MediaMetadataRetriever.width = 8193;
        check(helper.extract(3) == null, "dimension budget");
        check(MediaMetadataRetriever.frameRequests == 2, "reject before frame allocation");
        MediaMetadataRetriever.width = 8192;
        MediaMetadataRetriever.height = 8192;
        check(helper.extract(4) == null, "source pixel budget");
        MediaMetadataRetriever.width = 1920;
        MediaMetadataRetriever.height = 1080;
        MediaMetadataRetriever.malformed = true;
        check(helper.extract(5) == null, "malformed fallback");
        MediaMetadataRetriever.malformed = false;
        Build.VERSION.SDK_INT = 26;
        check(helper.extract(6) == null, "unsupported API safe fallback");
        Build.VERSION.SDK_INT = 35;
        MediaMetadataRetriever.delay = 5500;
        long start = System.nanoTime();
        check(helper.extract(7) == null, "decoder timeout fallback");
        check((System.nanoTime() - start) / 1_000_000 < 5400, "caller bounded by timeout");
        start = System.nanoTime();
        check(helper.extract(8) == null, "timed out decoder retains single work permit");
        check((System.nanoTime() - start) / 1_000_000 < 200, "busy decoder does not queue another job");
        Thread.sleep(650);
        MediaMetadataRetriever.delay = 0;
        check(helper.extract(9) != null, "decoder completion restores permit");
        System.out.println("VideoThumbnail: 9 generated framework scenarios passed");
    }
}
