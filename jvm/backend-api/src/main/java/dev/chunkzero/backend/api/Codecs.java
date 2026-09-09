package dev.chunkzero.backend.api;

import com.google.gson.JsonArray;
import com.google.gson.JsonElement;
import com.google.gson.JsonNull;
import com.google.gson.JsonObject;
import com.google.gson.JsonParser;
import com.google.gson.JsonPrimitive;
import com.google.gson.Strictness;
import com.google.gson.stream.JsonReader;
import com.google.gson.stream.JsonToken;
import java.io.IOException;
import java.io.StringReader;
import java.math.BigDecimal;
import java.util.ArrayList;
import java.util.List;
import java.util.Objects;
import java.util.Set;
import java.util.function.Function;

public final class Codecs {
    private Codecs() {}
    public static final long MAX_SAFE_INTEGER = 9_007_199_254_740_991L;
    public static final Codec<NullValue> NULL = of(value -> {
        require(value.isJsonNull()); return NullValue.INSTANCE;
    }, value -> { Objects.requireNonNull(value); return JsonNull.INSTANCE; });
    public static final Codec<Boolean> BOOLEAN = of(value -> {
        require(value.isJsonPrimitive() && value.getAsJsonPrimitive().isBoolean()); return value.getAsBoolean();
    }, value -> new JsonPrimitive(Objects.requireNonNull(value)));
    public static final Codec<String> STRING = of(value -> {
        require(value.isJsonPrimitive() && value.getAsJsonPrimitive().isString()); return string(value.getAsString());
    }, value -> new JsonPrimitive(string(value)));
    public static final Codec<Long> INTEGER = of(value -> {
        long number;
        try { number = number(value).longValueExact(); }
        catch (ArithmeticException error) { throw new IllegalArgumentException("Expected a safe integer", error); }
        require(number >= -MAX_SAFE_INTEGER && number <= MAX_SAFE_INTEGER); return number;
    }, value -> {
        require(value >= -MAX_SAFE_INTEGER && value <= MAX_SAFE_INTEGER); return new JsonPrimitive(value);
    });
    public static final Codec<Double> NUMBER = of(value -> safeNumber(number(value).doubleValue()),
        value -> new JsonPrimitive(safeNumber(value)));
    public static final Codec<PlayerId> PLAYER = map(STRING, PlayerId::new, PlayerId::value);
    public static final Codec<SessionId> SESSION = map(STRING, SessionId::new, SessionId::value);

    public static <T> Codec<T> of(Function<JsonElement, T> read, Function<T, JsonElement> write) {
        return new Codec<>() {
            public T decode(JsonElement value) { return read.apply(Objects.requireNonNull(value)); }
            public JsonElement encode(T value) { return write.apply(Objects.requireNonNull(value)); }
        };
    }
    public static <A, B> Codec<B> map(Codec<A> codec, Function<A, B> read, Function<B, A> write) {
        return of(value -> read.apply(codec.decode(value)), value -> codec.encode(write.apply(value)));
    }
    public static <T> Codec<Id<T>> id(String table) {
        return map(STRING, value -> { validateId(table, value); return new Id<>(value); },
            value -> { validateId(table, value.value()); return value.value(); });
    }
    public static <T> Codec<T> literal(Codec<T> codec, T expected) {
        return of(value -> { T decoded = codec.decode(value); require(Objects.equals(decoded, expected)); return decoded; },
            value -> { require(Objects.equals(value, expected)); return codec.encode(value); });
    }
    public static <T> Codec<List<T>> array(Codec<T> codec) {
        return of(value -> {
            require(value.isJsonArray()); var result = new ArrayList<T>();
            for (var item : value.getAsJsonArray()) result.add(codec.decode(item));
            return List.copyOf(result);
        }, value -> {
            var result = new JsonArray(); for (var item : value) result.add(codec.encode(item)); return result;
        });
    }
    public static JsonObject object(JsonElement value, Set<String> fields) {
        require(value.isJsonObject()); var object = value.getAsJsonObject();
        require(fields.containsAll(object.keySet())); return object;
    }
    public static <T> T field(JsonObject value, String name, Codec<T> codec) {
        require(value.has(name)); return codec.decode(value.get(name));
    }
    public static <T> FieldValue<T> optional(JsonObject value, String name, Codec<T> codec) {
        return value.has(name) ? FieldValue.present(codec.decode(value.get(name))) : FieldValue.absent();
    }
    public static <T> void optional(JsonObject object, String name, Codec<T> codec, FieldValue<T> value) {
        Objects.requireNonNull(value);
        if (value instanceof FieldValue.Present<T> present) object.add(name, codec.encode(present.value()));
    }
    public static JsonElement parse(String json) {
        if (json.length() > 1024 * 1024) throw new IllegalArgumentException("JSON size limit");
        try (var reader = new JsonReader(new StringReader(json))) {
            reader.setStrictness(Strictness.STRICT); reader.setNestingLimit(64);
            var value = JsonParser.parseReader(reader);
            require(reader.peek() == JsonToken.END_DOCUMENT);
            return value;
        } catch (IOException error) { throw new IllegalArgumentException("Invalid JSON", error); }
    }
    static void platformId(String value) { require(value != null && value.matches("[A-Za-z0-9_-]{1,128}")); }
    private static void validateId(String table, String value) {
        require(value.startsWith(table + ":")); platformId(value.substring(table.length() + 1));
    }
    private static BigDecimal number(JsonElement value) {
        require(value.isJsonPrimitive() && value.getAsJsonPrimitive().isNumber()); return value.getAsBigDecimal();
    }
    private static double safeNumber(double value) {
        require(Double.isFinite(value) && (value != Math.rint(value) || Math.abs(value) <= MAX_SAFE_INTEGER)); return value;
    }
    private static String string(String value) {
        Objects.requireNonNull(value);
        for (int i = 0; i < value.length(); i++) {
            char unit = value.charAt(i);
            if (Character.isHighSurrogate(unit)) {
                require(i + 1 < value.length() && Character.isLowSurrogate(value.charAt(++i)));
            } else require(!Character.isLowSurrogate(unit));
        }
        return value;
    }
    public static void require(boolean valid) { if (!valid) throw new IllegalArgumentException("Value does not match its backend contract"); }
}
