package dev.chunkzero.backend.api;

import tools.jackson.core.JsonParser;
import tools.jackson.core.JsonToken;
import tools.jackson.databind.BeanProperty;
import tools.jackson.databind.DeserializationContext;
import tools.jackson.databind.JavaType;
import tools.jackson.databind.jsontype.NamedType;
import tools.jackson.databind.jsontype.TypeDeserializer;
import tools.jackson.databind.jsontype.impl.AsPropertyTypeDeserializer;
import tools.jackson.databind.jsontype.impl.StdTypeResolverBuilder;
import tools.jackson.databind.util.TokenBuffer;

import java.util.Collection;

/** Jackson's named subtype binding restricted to objects with a string discriminator. */
public final class TaggedUnionResolver extends StdTypeResolverBuilder {
    public TaggedUnionResolver() {}

    private TaggedUnionResolver(TaggedUnionResolver source, Class<?> defaultImpl) {
        super(source, defaultImpl);
    }

    @Override
    public TaggedUnionResolver withDefaultImpl(Class<?> defaultImpl) {
        return new TaggedUnionResolver(this, defaultImpl);
    }

    @Override
    public TypeDeserializer buildTypeDeserializer(
            DeserializationContext context, JavaType type, Collection<NamedType> subtypes) {
        var delegate =
                (AsPropertyTypeDeserializer) super.buildTypeDeserializer(context, type, subtypes);
        return new ObjectDeserializer(delegate, null);
    }

    private static final class ObjectDeserializer extends AsPropertyTypeDeserializer {
        private ObjectDeserializer(AsPropertyTypeDeserializer source, BeanProperty property) {
            super(source, property);
        }

        @Override
        public TypeDeserializer forProperty(BeanProperty property) {
            return new ObjectDeserializer(this, property);
        }

        @Override
        protected Object _deserialize(JsonParser parser, DeserializationContext context) {
            // The superclass uses this path for wrapper arrays and scalars.
            throw new IllegalArgumentException("Expected a tagged union object");
        }

        @Override
        protected Object _deserializeTypedForId(
                JsonParser parser, DeserializationContext context, TokenBuffer buffer, String id) {
            if (!parser.hasToken(JsonToken.VALUE_STRING))
                throw new IllegalArgumentException("Expected a string union discriminator");
            return super._deserializeTypedForId(parser, context, buffer, id);
        }
    }
}
