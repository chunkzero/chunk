package dev.chunkzero.backend.api;

import dev.chunkzero.generated.BackendTypes;
import java.io.IOException;
import java.nio.charset.StandardCharsets;
import java.util.Arrays;
import org.junit.jupiter.api.Test;
import static org.junit.jupiter.api.Assertions.*;

class GeneratedContractsTest {
    @Test
    void generatedNamesAndScalarCodecsRoundTrip() {
        var value = Codecs.parse("{\"Objects\":\"text\",\"List\":[null],\"class\":\"line\\n\\u0000🌍\",\"ratio\":0.5,\"flag\":true,\"exact\":42,\"fraction\":0.125,\"nothing\":null}");
        var codec = BackendTypes.__proto__$read.arguments();
        assertEquals(value, codec.encode(codec.decode(value)));
        var result = Codecs.parse("{\"_id\":\"profiles:p1\",\"wins\":3}");
        assertEquals(result, BackendTypes.__proto__$read.result().encode(BackendTypes.__proto__$read.result().decode(result)));
        assertEquals(Codecs.parse("true"), BackendTypes.Codecs$.result().encode(BackendTypes.Codecs$.result().decode(Codecs.parse("true"))));
    }

    @Test
    void generatedCodecsPreserveAbsentNullUnionsIdsAndSafeIntegers() throws IOException {
        String input;
        try (var stream = getClass().getResourceAsStream("/values.json")) {
            assertNotNull(stream);
            input = new String(stream.readAllBytes(), StandardCharsets.UTF_8);
        }
        var fixtures = Codecs.parse(input).getAsJsonArray();
        var codec = BackendTypes.shared$profile$record.arguments();
        for (var value : fixtures) assertEquals(value, codec.encode(codec.decode(value)));
        var absent = codec.decode(fixtures.get(0));
        assertInstanceOf(FieldValue.Absent.class, absent.note());
        assertEquals(new Id<>("profiles:p1"), absent.id());
        var present = codec.decode(fixtures.get(1));
        assertInstanceOf(FieldValue.Present.class, present.note());
        assertInstanceOf(BackendTypes.Fn$shared$profile$record$Args$note.V0.class,
            ((FieldValue.Present<?>) present.note()).value());
        var invalid = fixtures.get(0).getAsJsonObject().deepCopy();
        invalid.addProperty("count", 9_007_199_254_740_992L);
        assertThrows(IllegalArgumentException.class, () -> codec.decode(invalid));
        invalid.addProperty("count", 1);
        invalid.addProperty("id", "matches:p1");
        assertThrows(IllegalArgumentException.class, () -> codec.decode(invalid));
        invalid.addProperty("id", "profiles:p1");
        invalid.addProperty("extra", true);
        assertThrows(IllegalArgumentException.class, () -> codec.decode(invalid));
        assertThrows(IllegalArgumentException.class, () -> Codecs.STRING.encode("\ud800"));
        assertThrows(IllegalArgumentException.class, () -> Codecs.INTEGER.decode(Codecs.parse("1.5")));
        assertThrows(IllegalArgumentException.class, () -> Codecs.NUMBER.encode(Double.NaN));
        assertTrue(Arrays.stream(BackendTypes.class.getFields()).noneMatch(field -> field.getName().contains("hidden")));
    }
}
