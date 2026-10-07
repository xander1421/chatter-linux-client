package android.graphics;
public final class Bitmap {
    public enum CompressFormat { JPEG }
    private final int width, height;
    public static int recycled;
    public static int encodedBytes = 100;
    public Bitmap(int width, int height) { this.width = width; this.height = height; }
    public int getWidth() { return width; }
    public int getHeight() { return height; }
    public boolean compress(CompressFormat format, int quality, java.io.OutputStream output) {
        try { output.write(new byte[encodedBytes]); return true; }
        catch (java.io.IOException e) { throw new IllegalStateException(e); }
    }
    public void recycle() { recycled++; }
}
