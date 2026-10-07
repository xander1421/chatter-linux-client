package android.media;
public abstract class MediaDataSource implements java.io.Closeable {
    public abstract long getSize();
    public abstract int readAt(long position, byte[] buffer, int offset, int size);
    public abstract void close();
}
