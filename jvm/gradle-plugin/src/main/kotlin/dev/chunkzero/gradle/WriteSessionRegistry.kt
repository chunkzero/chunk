package dev.chunkzero.gradle

import com.google.gson.Gson
import org.gradle.api.DefaultTask
import org.gradle.api.file.ConfigurableFileCollection
import org.gradle.api.file.DirectoryProperty
import org.gradle.api.file.RegularFileProperty
import org.gradle.api.provider.Property
import org.gradle.api.tasks.CacheableTask
import org.gradle.api.tasks.Classpath
import org.gradle.api.tasks.Input
import org.gradle.api.tasks.InputFiles
import org.gradle.api.tasks.OutputDirectory
import org.gradle.api.tasks.OutputFile
import org.gradle.api.tasks.PathSensitive
import org.gradle.api.tasks.PathSensitivity
import org.gradle.api.tasks.TaskAction
import org.objectweb.asm.AnnotationVisitor
import org.objectweb.asm.ClassReader
import org.objectweb.asm.ClassVisitor
import org.objectweb.asm.MethodVisitor
import org.objectweb.asm.Opcodes
import org.objectweb.asm.Type
import java.util.jar.JarFile

@CacheableTask
abstract class WriteSessionRegistry : DefaultTask() {
    @get:Input abstract val app: Property<String>

    @get:Input abstract val mainClass: Property<String>

    @get:InputFiles
    @get:PathSensitive(PathSensitivity.RELATIVE)
    abstract val classes: ConfigurableFileCollection

    @get:Classpath abstract val dependencies: ConfigurableFileCollection

    @get:OutputDirectory abstract val outputDirectory: DirectoryProperty

    @get:InputFiles
    @get:PathSensitive(PathSensitivity.NONE)
    abstract val methodContracts: ConfigurableFileCollection

    @get:InputFiles
    @get:PathSensitive(PathSensitivity.NONE)
    abstract val configurationContracts: ConfigurableFileCollection

    @get:OutputDirectory abstract val bindingSourceDirectory: DirectoryProperty

    @get:OutputFile abstract val catalogFile: RegularFileProperty

    @TaskAction
    fun write() {
        val found = sortedMapOf<String, CompiledClass>()
        classes.files.filter { it.isDirectory }.forEach { directory ->
            directory.walkTopDown().filter { it.isFile && it.extension == "class" }.forEach {
                val type = inspect(it.readBytes())
                found[type.name] = type
            }
        }
        val main = found[mainClass.get().replace('.', '/')]
        require(main?.main == true) { "App requires a compiled public static main(String[]): ${mainClass.get()}" }
        val cache = found.toMutableMap()

        fun lookup(name: String): CompiledClass? =
            cache[name] ?: dependencies.files
                .firstNotNullOfOrNull { file ->
                    if (file.isDirectory) {
                        file
                            .resolve("$name.class")
                            .takeIf { it.isFile }
                            ?.readBytes()
                            ?.let(::inspect)
                    } else if (file.isFile) {
                        JarFile(file).use { jar ->
                            jar.getJarEntry("$name.class")?.let {
                                jar.getInputStream(it).use { input ->
                                    inspect(input.readBytes())
                                }
                            }
                        }
                    } else {
                        null
                    }
                }?.also { cache[name] = it }

        fun provider(
            name: String,
            seen: MutableSet<String> = mutableSetOf(),
        ): Boolean {
            if (name == "dev/chunkzero/runtime/SessionProvider") return true
            if (!seen.add(name)) return false
            val type = lookup(name) ?: return false
            return type.parents.any { provider(it, seen) }
        }
        val sessions = sortedMapOf<String, String>()
        for (type in found.values.filter { it.session != null }) {
            val id = requireNotNull(type.session)
            require(id.matches(Regex("[A-Za-z_][A-Za-z0-9_]{0,127}"))) { "Invalid session type ID: $id" }
            require(type.constructible && provider(type.name)) {
                "Session $id requires a public concrete SessionProvider with a public no-argument constructor"
            }
            require(sessions.put(id, type.name.replace('/', '.')) == null) { "Duplicate session type: $id" }
        }
        require(sessions.size in 1..128) { "App requires 1–128 @SessionType declarations" }
        writeSessionConfigurations(
            app.get(),
            sessions,
            configurationContracts.singleFile,
            ::lookup,
            outputDirectory.get().asFile,
        )
        writeSessionMethods(
            app.get(),
            sessions,
            methodContracts.singleFile,
            ::lookup,
            outputDirectory.get().asFile,
            bindingSourceDirectory.get().asFile,
        )
        val output = outputDirectory.file("META-INF/services/dev.chunkzero.runtime.SessionProvider").get().asFile
        output.parentFile.mkdirs()
        output.writeText(sessions.values.joinToString("\n", postfix = "\n"))
        val catalog = catalogFile.get().asFile
        catalog.parentFile.mkdirs()
        catalog.writeText(Gson().toJson(sessions.keys) + "\n")
    }
}

internal class CompiledClass {
    var name = ""
    var parents = emptyList<String>()
    var publicConcrete = false
    var constructor = false
    var main = false
    var session: String? = null
    var creates: String? = null
    var createsConfigured: String? = null
    val constructible get() = publicConcrete && constructor
}

private fun inspect(bytes: ByteArray): CompiledClass {
    val type = CompiledClass()
    ClassReader(bytes).accept(
        object : ClassVisitor(Opcodes.ASM9) {
            override fun visit(
                version: Int,
                access: Int,
                name: String,
                signature: String?,
                superName: String?,
                interfaces: Array<out String>,
            ) {
                type.name = name
                type.parents = interfaces.toList() + listOfNotNull(superName)
                type.publicConcrete =
                    access and Opcodes.ACC_PUBLIC != 0 &&
                    access and (Opcodes.ACC_ABSTRACT or Opcodes.ACC_INTERFACE) == 0
            }

            override fun visitAnnotation(
                descriptor: String,
                visible: Boolean,
            ): AnnotationVisitor? {
                if (descriptor != "Ldev/chunkzero/runtime/SessionType;") return null
                type.session = ""
                return object : AnnotationVisitor(Opcodes.ASM9) {
                    override fun visit(
                        name: String,
                        value: Any,
                    ) {
                        if (name == "value") type.session = value as? String ?: ""
                    }
                }
            }

            override fun visitMethod(
                access: Int,
                name: String,
                descriptor: String,
                signature: String?,
                exceptions: Array<out String>?,
            ): MethodVisitor? {
                val arguments = Type.getArgumentTypes(descriptor)
                val creation =
                    arguments.size == 1 && arguments[0].descriptor == "Ldev/chunkzero/runtime/SessionCreation;"
                if (name == "create" && (arguments.isEmpty() || creation) &&
                    access and Opcodes.ACC_PUBLIC != 0 &&
                    access and (Opcodes.ACC_STATIC or Opcodes.ACC_BRIDGE) == 0 &&
                    Type.getReturnType(descriptor).sort == Type.OBJECT
                ) {
                    val result = Type.getReturnType(descriptor).internalName
                    if (creation) type.createsConfigured = result else type.creates = result
                }
                if (name == "<init>" && descriptor == "()V" &&
                    access and Opcodes.ACC_PUBLIC != 0
                ) {
                    type.constructor = true
                }
                if (name == "main" && descriptor == "([Ljava/lang/String;)V" &&
                    access and (Opcodes.ACC_PUBLIC or Opcodes.ACC_STATIC) == (Opcodes.ACC_PUBLIC or Opcodes.ACC_STATIC)
                ) {
                    type.main =
                        true
                }
                return null
            }
        },
        ClassReader.SKIP_CODE or ClassReader.SKIP_DEBUG or ClassReader.SKIP_FRAMES,
    )
    return type
}
