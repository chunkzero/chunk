package dev.chunkzero.backend.api;

import tools.jackson.core.JsonParser;
import tools.jackson.core.JsonToken;
import tools.jackson.databind.BeanDescription;
import tools.jackson.databind.DeserializationConfig;
import tools.jackson.databind.DeserializationContext;
import tools.jackson.databind.JavaType;
import tools.jackson.databind.ValueDeserializer;
import tools.jackson.databind.deser.ValueDeserializerModifier;
import tools.jackson.databind.deser.std.DelegatingDeserializer;

final class StrictEnums extends ValueDeserializerModifier {
    private static final long serialVersionUID = 1L;

    @Override
    public ValueDeserializer<?> modifyEnumDeserializer(
            DeserializationConfig config,
            JavaType type,
            BeanDescription.Supplier description,
            ValueDeserializer<?> deserializer) {
        return new ExactString(deserializer);
    }

    private static final class ExactString extends DelegatingDeserializer {
        private ExactString(ValueDeserializer<?> delegate) {
            super(delegate);
        }

        @Override
        protected ValueDeserializer<?> newDelegatingInstance(ValueDeserializer<?> delegate) {
            return new ExactString(delegate);
        }

        @Override
        public Object deserialize(JsonParser parser, DeserializationContext context) {
            if (!parser.hasToken(JsonToken.VALUE_STRING))
                throw new IllegalArgumentException("Expected an enum string");
            var value = parser.getString();
            // Enum names are schema identifiers; Jackson must not trim invalid input into one.
            if (!value.equals(value.trim()))
                throw new IllegalArgumentException("Unexpected whitespace in enum value");
            return super.deserialize(parser, context);
        }
    }
}
