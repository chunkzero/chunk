package com.chunkzero.chunk.gradle

import org.junit.jupiter.api.Assertions.assertEquals
import org.junit.jupiter.api.Assertions.assertThrows
import org.junit.jupiter.api.Assertions.assertTrue
import org.junit.jupiter.api.Test
import org.objectweb.asm.ClassWriter
import org.objectweb.asm.Opcodes

class ComponentIndexTest {
    @Test
    fun `linking rejects missing duplicate cyclic and transitive session dependencies`() {
        val cases =
            listOf(
                listOf(factory("A", "SESSION", "Missing")) to "Missing component provider",
                listOf(factory("A"), factory("A")) to "Duplicate component identity",
                listOf(factory("A", "SESSION", "B"), factory("B", "SESSION", "A")) to "Component dependency cycle",
                listOf(factory("A", "PROCESS", "B"), factory("B", "PROCESS", "C"), factory("C")) to
                    "captures session dependency",
                listOf(factory("A", "PROCESS", "com/chunkzero/chunk/backend/client/BackendSession")) to
                    "captures session dependency",
                listOf(
                    factory("com/chunkzero/chunk/backend/client/BackendSession"),
                ) to "cannot provide host-supplied type",
            )
        for ((graph, message) in cases) {
            val error = assertThrows(IllegalArgumentException::class.java) { validateComponents(graph, { null }) }
            assertTrue(error.message.orEmpty().contains(message), error.toString())
        }
    }

    @Test
    fun `sessions can use process providers and session builtins without duplicate shared construction`() {
        val graph =
            listOf(
                factory("View", "SESSION", "Clock", "Backend"),
                factory("Clock", "PROCESS"),
                factory("Backend", "SESSION", "com/chunkzero/chunk/backend/client/BackendSession"),
            )
        assertEquals(listOf("Backend", "Clock", "View"), validateComponents(graph, { null }).map { it.type })
    }

    @Test
    fun `supplied types are session dependencies that no factory provides`() {
        val bytes =
            ClassWriter(0)
                .apply {
                    visit(Opcodes.V21, Opcodes.ACC_PUBLIC, "host/Scope", null, "java/lang/Object", null)
                    visitAnnotation("Lcom/chunkzero/chunk/runtime/Component\$Supplied;", true).visitEnd()
                    visitEnd()
                }.toByteArray()
        val supplied = inspectClass(bytes)
        val lookup: ClassLookup = { if (it == "host/Scope") supplied else null }
        assertEquals(
            listOf("View"),
            validateComponents(listOf(factory("View", "SESSION", "host/Scope")), lookup).map {
                it.type
            },
        )
        for ((graph, message) in listOf(
            listOf(factory("View", "PROCESS", "host/Scope")) to "captures session dependency",
            listOf(factory("host/Scope", "SESSION")) to "cannot provide host-supplied type",
        )) {
            val error = assertThrows(IllegalArgumentException::class.java) { validateComponents(graph, lookup) }
            assertTrue(error.message.orEmpty().contains(message), error.toString())
        }
    }

    @Test
    fun `erased generics nonstatic inaccessible and primitive factories are rejected`() {
        val valid =
            ComponentMethod(
                "fixture/Providers",
                Opcodes.ACC_PUBLIC,
                "create",
                "()Ljava/lang/String;",
                9,
                null,
                "PROCESS",
            )
        assertEquals("java/lang/String", componentFactories(listOf(valid)).single().type)
        for (method in listOf(
            valid.copy(signature = "()Ljava/util/List<Ljava/lang/String;>;"),
            valid.copy(access = Opcodes.ACC_PUBLIC),
            valid.copy(ownerAccess = 0),
            valid.copy(descriptor = "()I"),
            valid.copy(descriptor = "([Ljava/lang/String;)Ljava/lang/String;"),
        )) {
            assertThrows(IllegalArgumentException::class.java) { componentFactories(listOf(method)) }
        }
    }

    private fun factory(
        type: String,
        scope: String = "SESSION",
        vararg dependencies: String,
    ) = ComponentFactory("Providers", "create$type", type, dependencies.toList(), scope)
}
