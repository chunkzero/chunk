package dev.chunkzero.gradle

import org.junit.jupiter.api.Assertions.assertEquals
import org.junit.jupiter.api.Assertions.assertThrows
import org.junit.jupiter.api.Assertions.assertTrue
import org.junit.jupiter.api.Test
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
                listOf(factory("A", "PROCESS", "dev/chunkzero/runtime/SessionScope")) to
                    "captures session dependency",
                listOf(factory("dev/chunkzero/backend/client/BackendSession")) to "cannot replace session builtin",
            )
        for ((graph, message) in cases) {
            val error = assertThrows(IllegalArgumentException::class.java) { validateComponents(graph) }
            assertTrue(error.message.orEmpty().contains(message), error.toString())
        }
    }

    @Test
    fun `sessions can use process providers and session builtins without duplicate shared construction`() {
        val graph =
            listOf(
                factory("View", "SESSION", "Clock", "Backend", "dev/chunkzero/runtime/SessionScope"),
                factory("Clock", "PROCESS"),
                factory("Backend", "SESSION", "dev/chunkzero/backend/client/BackendSession"),
            )
        assertEquals(listOf("Backend", "Clock", "View"), validateComponents(graph).map { it.type })
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
