package dev.chunkzero.backend.api;

import java.util.List;
import java.util.Objects;
import java.util.function.Consumer;

/** Value constraints shared by generated record constructors and function signatures. */
public final class BackendValues {
    public static final long MAX_SAFE_INTEGER = 9_007_199_254_740_991L;

    private BackendValues() {}

    public static void checkInteger(Long value) {
        require(value != null && value >= -MAX_SAFE_INTEGER && value <= MAX_SAFE_INTEGER);
    }

    public static void checkNumber(Double value) {
        require(
                value != null
                        && Double.isFinite(value)
                        && (value != Math.rint(value) || Math.abs(value) <= MAX_SAFE_INTEGER));
    }

    public static void checkString(String value) {
        Objects.requireNonNull(value);
        for (int i = 0; i < value.length(); i++) {
            char unit = value.charAt(i);
            if (Character.isHighSurrogate(unit)) {
                require(i + 1 < value.length() && Character.isLowSurrogate(value.charAt(++i)));
            } else require(!Character.isLowSurrogate(unit));
        }
    }

    public static void checkNull(Object value) {
        require(value == null);
    }

    public static <T> void checkLiteral(T value, T expected) {
        require(Objects.equals(value, expected));
    }

    public static <T> void checkArray(List<T> value, Consumer<T> validate) {
        value.forEach(validate);
    }

    public static <T> List<T> copyArray(List<T> value) {
        return value.stream().map(BackendValues::copyValue).toList();
    }

    @SuppressWarnings("unchecked")
    static <T> T copyValue(T value) {
        return value instanceof List<?> list ? (T) copyArray(list) : value;
    }

    public static void tableId(String table, String value) {
        require(value != null && value.startsWith(table + ":"));
        PlatformIds.check(value.substring(table.length() + 1));
    }

    private static void require(boolean valid) {
        if (!valid) throw new IllegalArgumentException("Value does not match its backend contract");
    }
}
