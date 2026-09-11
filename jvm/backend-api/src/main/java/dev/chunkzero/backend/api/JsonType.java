package dev.chunkzero.backend.api;

import tools.jackson.core.type.TypeReference;
import tools.jackson.databind.ObjectReader;
import tools.jackson.databind.ObjectWriter;

import java.util.function.Consumer;

/** Jackson binding and value validation for a generated function argument or result. */
public final class JsonType<T> {
    private final ObjectReader reader;
    private final ObjectWriter writer;
    private final Consumer<T> validate;

    private JsonType(TypeReference<T> type, Consumer<T> validate) {
        reader = BackendJson.mapper().readerFor(type);
        writer = BackendJson.mapper().writerFor(type);
        this.validate = validate;
    }

    public static <T> JsonType<T> of(TypeReference<T> type, Consumer<T> validate) {
        return new JsonType<>(type, validate);
    }

    public T read(String json) {
        BackendJson.checkSize(json);
        T value = reader.readValue(json);
        validate.accept(value);
        return BackendValues.copyValue(value);
    }

    public String write(T value) {
        validate.accept(value);
        var json = writer.writeValueAsString(value);
        BackendJson.checkSize(json);
        return json;
    }
}
