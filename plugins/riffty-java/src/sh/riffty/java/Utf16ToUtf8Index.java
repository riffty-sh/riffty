package sh.riffty.java;

import java.util.Objects;

/**
 * Exact O(N) UTF-16 char-index to UTF-8 byte-offset lookup table (RFC §4.6).
 */
public final class Utf16ToUtf8Index {
    private final int[] utf8Offset;

    public Utf16ToUtf8Index(String source) {
        Objects.requireNonNull(source, "source");
        int n = source.length();
        this.utf8Offset = new int[n + 1];
        int bytePos = 0;
        for (int i = 0; i < n; ) {
            utf8Offset[i] = bytePos;
            int cp = source.codePointAt(i);
            int charCount = Character.charCount(cp);
            int utf8Len = utf8ByteLength(cp);
            if (charCount == 2) {
                utf8Offset[i + 1] = bytePos;
            }
            bytePos += utf8Len;
            i += charCount;
        }
        utf8Offset[n] = bytePos;
    }

    public int toByteOffset(int utf16Pos) {
        return utf8Offset[Math.clamp(utf16Pos, 0, utf8Offset.length - 1)];
    }

    private static int utf8ByteLength(int cp) {
        if (cp <= 0x7F) return 1;
        if (cp <= 0x7FF) return 2;
        if (cp <= 0xFFFF) return 3;
        return 4;
    }
}

