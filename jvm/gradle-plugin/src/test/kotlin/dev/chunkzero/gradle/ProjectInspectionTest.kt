package dev.chunkzero.gradle

import org.junit.jupiter.api.Assertions.assertEquals
import org.junit.jupiter.api.Assertions.assertThrows
import org.junit.jupiter.api.Test
import org.junit.jupiter.api.io.TempDir
import java.nio.file.Path

class ProjectInspectionTest {
    @TempDir
    lateinit var directory: Path

    @Test
    fun `stable app identity can use a nested directory with a different name`() {
        directory.resolve("apps/games/duels").toFile().mkdirs()
        val apps =
            readApps(
                """{"version":1,"apps":[{"id":"arena","directory":"apps/games/duels","gradle_project":":apps:games:duels"}]}""",
                directory.toFile(),
            )
        assertEquals(AppMetadata("arena", "apps/games/duels", ":apps:games:duels"), apps.single())
    }

    @Test
    fun `app inventory rejects mismatched mappings and duplicate physical projects`() {
        directory.resolve("apps/duels").toFile().mkdirs()
        for (entries in listOf(
            """{"id":"arena","directory":"apps/../duels","gradle_project":":apps:duels"}""",
            """{"id":"arena","directory":"apps/duels","gradle_project":":apps:arena"}""",
            """{"id":"arena","directory":"apps/duels","gradle_project":":apps:duels"},{"id":"other","directory":"apps/duels","gradle_project":":apps:duels"}""",
        )) {
            assertThrows(IllegalArgumentException::class.java) {
                readApps("""{"version":1,"apps":[$entries]}""", directory.toFile())
            }
        }
    }
}
