package dev.chunkzero.backend.api;

import static org.junit.jupiter.api.Assertions.*;

import dev.chunkzero.generated.BackendTypes;
import dev.chunkzero.generated.BackendTypes.Shared.Profile.RecordArgs.State;
import dev.chunkzero.generated.SessionMethods;

import org.junit.jupiter.api.Test;

import tools.jackson.core.type.TypeReference;
import tools.jackson.databind.JsonNode;
import tools.jackson.databind.node.ObjectNode;

import java.io.IOException;
import java.nio.charset.StandardCharsets;
import java.util.Arrays;
import java.util.List;
import java.util.Objects;

class GeneratedContractsTest {
    @Test
    void nestedModelsAndReservedNamesRetainTheirWireShape() {
        var input =
                "{\"when\":\"now\",\"CODEC\":\"text\",\"payload\":{\"items\":[{\"value\":\"one\"}]}}";
        var reference = BackendTypes.Shared.Names.class_;
        var args = reference.arguments().read(input);
        assertEquals("now", args.when_());
        assertEquals("text", args.CODEC());
        BackendTypes.Shared.Names.ClassArgs.Payload.ItemsItem item =
                args.payload().items().getFirst();
        assertEquals("one", item.value());
        assertEquals(json(input), json(reference.arguments().write(args)));
        assertEquals(List.of("one"), reference.result().read("[\"one\"]"));
        assertEquals("shared/names/class", reference.path());
        assertEquals("same/same/read", BackendTypes.Same.Same_.read.path());
        assertReadableNames(BackendTypes.class);
    }

    @Test
    void scalarsLiteralsAndDocumentIdsRoundTrip() {
        var input =
                "{\"Objects\":\"text\",\"List\":[null],\"class\":\"line\\n"
                    + "\\u0000🌍\","
                    + "\"ratio\":0.5,\"flag\":true,\"exact\":42,\"fraction\":0.125,\"nothing\":null}";
        var type = BackendTypes.Proto.read.arguments();
        assertEquals(json(input), json(type.write(type.read(input))));
        for (var ratio : List.of("9007199254740991.0", "-9007199254740991.0", "0.125"))
            assertEquals(Double.parseDouble(ratio), type.read(input.replace("0.5", ratio)).ratio());
        var result = "{\"_id\":\"profiles:p1\",\"wins\":3}";
        assertEquals(
                json(result),
                json(
                        BackendTypes.Proto.read
                                .result()
                                .write(BackendTypes.Proto.read.result().read(result))));
        var choices = BackendTypes.codecs.result();
        assertEquals("\"allow\"", choices.write(choices.read("\"allow\"")));
        for (var invalid :
                List.of("0", "\"unknown\"", "null", "\" allow\"", "\"allow \"", "\"allow\\n\""))
            assertThrows(RuntimeException.class, () -> choices.read(invalid));
        for (var invalid :
                List.of(
                        input.replace("42", "43"),
                        input.replace("[null]", "[1]"),
                        input.replace("0.5", "\"0.5\""),
                        input.replace("0.5", "9007199254740992.0"),
                        input.replace("0.5", "-9007199254740992.0"),
                        input.replace("0.5", "1e20"),
                        input.replace("0.5", "-1e20")))
            assertThrows(RuntimeException.class, () -> type.read(invalid));
    }

    @Test
    void taggedUnionsAndOptionalNullUseStandardRecordBindings() throws IOException {
        var fixtures = fixtures();
        var type = BackendTypes.Shared.Profile.record_.arguments();
        for (var value : fixtures) {
            var expected = (ObjectNode) value.deepCopy();
            if (expected.path("note").isNull()) expected.remove("note");
            assertEquals(expected, json(type.write(type.read(value.toString()))));
        }
        var absent = type.read(fixtures.get(0).toString());
        assertNull(absent.note());
        assertEquals(new BackendTypes.Ids.Profiles("profiles:p1"), absent.id());
        assertInstanceOf(BackendTypes.Shared.Profile.RecordArgs.State.Ready.class, absent.state());
        var present = type.read(fixtures.get(1).toString());
        assertNull(present.note());
        assertInstanceOf(
                BackendTypes.Shared.Profile.RecordArgs.State.Waiting.class, present.state());
        assertThrows(UnsupportedOperationException.class, () -> absent.labels().add("another"));
        assertTrue(
                Arrays.stream(BackendTypes.Shared.Profile.class.getFields())
                        .noneMatch(field -> field.getName().contains("hidden")));
    }

    @Test
    void missingFieldsInvalidValuesAndCoercionsFail() throws IOException {
        var base = (ObjectNode) fixtures().get(0);
        var type = BackendTypes.Shared.Profile.record_.arguments();
        for (var badCount : List.of("null", "\"1\"", "true", "1.5", "1.0", "9007199254740992")) {
            var invalid = base.deepCopy();
            invalid.set("count", json(badCount));
            assertThrows(RuntimeException.class, () -> type.read(invalid.toString()));
        }
        for (var scalar : List.of("42", "true", "0.5")) {
            var invalid = base.deepCopy();
            invalid.set("note", json(scalar));
            assertThrows(RuntimeException.class, () -> type.read(invalid.toString()));
            assertThrows(
                    RuntimeException.class,
                    () -> BackendTypes.Shared.Names.class_.result().read("[" + scalar + "]"));
        }
        for (var field : List.of("count", "id", "labels", "player", "state")) {
            var invalid = base.deepCopy();
            invalid.remove(field);
            assertThrows(RuntimeException.class, () -> type.read(invalid.toString()), field);
            invalid.set(field, json("null"));
            assertThrows(RuntimeException.class, () -> type.read(invalid.toString()), field);
        }
        for (var change :
                List.of(
                        "{\"id\":\"matches:p1\"}",
                        "{\"extra\":true}",
                        "{\"labels\":[null]}",
                        "{\"state\":{\"type\":\"unknown\"}}",
                        "{\"state\":{\"type\":\"waiting\"}}",
                        "{\"state\":{\"type\":true}}",
                        "{\"state\":[\"ready\",{}]}",
                        "{\"state\":[\"waiting\",{\"reason\":\"x\"}]}",
                        "{\"state\":\"ready\"}")) {
            var invalid = base.deepCopy();
            invalid.setAll((ObjectNode) json(change));
            assertThrows(RuntimeException.class, () -> type.read(invalid.toString()));
        }
        assertThrows(RuntimeException.class, () -> type.read(base + " {}"));
        assertThrows(IllegalArgumentException.class, () -> BackendValues.checkString("\ud800"));
        assertThrows(IllegalArgumentException.class, () -> BackendValues.checkNumber(Double.NaN));
        assertThrows(
                IllegalArgumentException.class, () -> new BackendTypes.Ids.Profiles("other:p1"));
        assertThrows(
                IllegalArgumentException.class,
                () ->
                        new BackendTypes.Shared.Profile.RecordArgs(
                                9_007_199_254_740_992L,
                                new BackendTypes.Ids.Profiles("profiles:p1"),
                                List.of(),
                                null,
                                new PlayerId("alex"),
                                new BackendTypes.Shared.Profile.RecordArgs.State.Ready()));
    }

    @Test
    void rootAndListUnionsRequireObjectsWithStringTags() {
        var type = JsonType.of(new TypeReference<State>() {}, Objects::requireNonNull);
        var value = "{\"reason\":\"later\",\"type\":\"waiting\"}";
        assertEquals(json(value), json(type.write(type.read(value))));
        assertEquals("{\"type\":\"true\"}", type.write(type.read("{\"type\":\"true\"}")));
        var list = JsonType.of(new TypeReference<List<State>>() {}, Objects::requireNonNull);
        assertEquals(List.of(new State.Ready()), list.read("[{\"type\":\"ready\"}]"));
        for (var invalid :
                List.of("[\"ready\",{}]", "[\"waiting\",{\"reason\":\"x\"}]", "{\"type\":true}")) {
            assertThrows(RuntimeException.class, () -> type.read(invalid));
            assertThrows(RuntimeException.class, () -> list.read("[" + invalid + "]"));
        }
    }

    @Test
    void nullableResultsAndNestedNullableArraysPreserveNull() {
        var arguments = BackendTypes.Shared.Profile.nullable.arguments();
        assertNull(arguments.read("{\"note\":null}").note());
        assertThrows(RuntimeException.class, () -> arguments.read("{}"));
        assertEquals(
                "{\"note\":null}",
                arguments.write(new BackendTypes.Shared.Profile.NullableArgs(null)));
        var type = BackendTypes.Shared.Profile.nullable.result();
        var json = "[null,[null,\"ok\"]]";
        var result = type.read(json);
        assertEquals(json, type.write(result));
        assertThrows(UnsupportedOperationException.class, () -> result.add(List.of()));
        assertThrows(UnsupportedOperationException.class, () -> result.get(1).add("another"));
        assertNull(BackendTypes.Same.Same_.read.result().read("null"));
        assertThrows(
                RuntimeException.class, () -> BackendTypes.Same.Same_.read.result().read("{}"));
    }

    @Test
    void sessionMethodInterfacesUseTheSameValidatedWireModels() throws IOException {
        var ref = SessionMethods.Duels.Default.Forfeit.REF;
        SessionMethods.Duels.Default.Forfeit implementation = args -> args.count() > 0;
        var input = fixtures().get(0).toString();
        var arguments = ref.arguments().read(input);
        assertEquals("duels", ref.app());
        assertEquals("default", ref.session());
        assertEquals("forfeit", ref.name());
        assertEquals("true", ref.result().write(implementation.forfeit(arguments)));
        assertEquals(new SessionMethods.Ids.Profiles("profiles:p1"), arguments.id());
        assertThrows(RuntimeException.class, () -> ref.arguments().read("{}"));
        assertThrows(RuntimeException.class, () -> ref.result().read("1"));
    }

    private JsonNode fixtures() throws IOException {
        try (var stream = getClass().getResourceAsStream("/values.json")) {
            assertNotNull(stream);
            return json(new String(stream.readAllBytes(), StandardCharsets.UTF_8));
        }
    }

    private static JsonNode json(String value) {
        return BackendJson.mapper().readTree(value);
    }

    private static void assertReadableNames(Class<?> type) {
        assertFalse(type.getSimpleName().contains("$"));
        for (var field : type.getFields()) assertFalse(field.getName().contains("$"));
        for (var method : type.getMethods()) assertFalse(method.getName().contains("$"));
        for (var child : type.getDeclaredClasses()) assertReadableNames(child);
    }
}
