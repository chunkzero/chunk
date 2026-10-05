package com.chunkzero.chunk.backend.api;

import tools.jackson.core.JsonParser;
import tools.jackson.core.JsonToken;
import tools.jackson.core.StreamReadConstraints;
import tools.jackson.core.json.JsonFactory;
import tools.jackson.databind.DeserializationContext;
import tools.jackson.databind.DeserializationFeature;
import tools.jackson.databind.MapperFeature;
import tools.jackson.databind.ValueDeserializer;
import tools.jackson.databind.cfg.CoercionAction;
import tools.jackson.databind.cfg.CoercionInputShape;
import tools.jackson.databind.cfg.EnumFeature;
import tools.jackson.databind.json.JsonMapper;
import tools.jackson.databind.module.SimpleModule;
import tools.jackson.databind.type.LogicalType;

/** Strict JSON configuration for the JavaScript API boundary. */
public final class BackendJson {
    private static final JsonMapper MAPPER =
            JsonMapper.builder(
                            JsonFactory.builder()
                                    .streamReadConstraints(
                                            StreamReadConstraints.builder()
                                                    .maxNestingDepth(64)
                                                    .build())
                                    .build())
                    .disable(MapperFeature.ALLOW_COERCION_OF_SCALARS)
                    .disable(DeserializationFeature.ACCEPT_FLOAT_AS_INT)
                    .enable(DeserializationFeature.FAIL_ON_UNKNOWN_PROPERTIES)
                    .enable(DeserializationFeature.FAIL_ON_TRAILING_TOKENS)
                    .enable(EnumFeature.FAIL_ON_NUMBERS_FOR_ENUMS)
                    .withCoercionConfig(
                            LogicalType.Textual,
                            config ->
                                    config.setCoercion(
                                                    CoercionInputShape.Integer, CoercionAction.Fail)
                                            .setCoercion(
                                                    CoercionInputShape.Float, CoercionAction.Fail)
                                            .setCoercion(
                                                    CoercionInputShape.Boolean,
                                                    CoercionAction.Fail))
                    .addModule(
                            new SimpleModule()
                                    .setDeserializerModifier(new StrictEnums())
                                    .addDeserializer(
                                            Void.class,
                                            new ValueDeserializer<Void>() {
                                                @Override
                                                public Void deserialize(
                                                        JsonParser parser,
                                                        DeserializationContext context) {
                                                    if (!parser.hasToken(JsonToken.VALUE_NULL))
                                                        throw new IllegalArgumentException(
                                                                "Expected null");
                                                    return null;
                                                }
                                            }))
                    .build();

    private BackendJson() {}

    public static JsonMapper mapper() {
        return MAPPER;
    }

    static void checkSize(String json) {
        if (json.length() > 1024 * 1024) throw new IllegalArgumentException("JSON size limit");
    }
}
