package com.chunkzero.chunk.gradle

import com.google.gson.JsonObject
import org.objectweb.asm.AnnotationVisitor
import org.objectweb.asm.ClassReader
import org.objectweb.asm.ClassVisitor
import org.objectweb.asm.MethodVisitor
import org.objectweb.asm.Opcodes
import org.objectweb.asm.Type
import java.io.File
import java.util.jar.JarFile

internal typealias ClassLookup = (String) -> CompiledClass?

internal class CompiledClass {
    var name = ""
    var parents = emptyList<String>()
    var publicConcrete = false
    var constructor = false
    var main = false
    var session: String? = null
    var supplied = false
    var creates: String? = null
    var createsConfigured: String? = null
    val constructible get() = publicConcrete && constructor
}

/** Looks up classes in [preloaded], then in the class directories and JARs of [classpath]. */
internal fun classLookup(
    classpath: Collection<File>,
    preloaded: Map<String, CompiledClass> = emptyMap(),
): ClassLookup {
    val cache = preloaded.toMutableMap()
    return { name ->
        cache[name] ?: classpath
            .firstNotNullOfOrNull { readClass(it, name) }
            ?.let(::inspectClass)
            ?.also { cache[name] = it }
    }
}

internal fun readClass(
    file: File,
    name: String,
): ByteArray? =
    if (file.isDirectory) {
        file.resolve("$name.class").takeIf { it.isFile }?.readBytes()
    } else if (file.isFile) {
        JarFile(file).use { jar ->
            jar.getJarEntry("$name.class")?.let { jar.getInputStream(it).use { input -> input.readAllBytes() } }
        }
    } else {
        null
    }

internal fun ClassLookup.inherits(
    name: String,
    target: String,
    seen: MutableSet<String> = mutableSetOf(),
): Boolean = name == target || (seen.add(name) && this(name)?.parents.orEmpty().any { inherits(it, target, seen) })

/** Whether [type] implements a contract declared for a session other than [app]/[session]. */
internal fun ClassLookup.implementsForeign(
    type: String,
    app: String,
    session: String,
    declarations: List<JsonObject>,
): Boolean =
    declarations.any {
        (it["app"].asString != app || it["session"].asString != session) &&
            inherits(type, it["binary_interface"].asString.replace('.', '/'))
    }

internal fun inspectClass(bytes: ByteArray): CompiledClass {
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
                if (descriptor == "Lcom/chunkzero/chunk/runtime/Component\$Supplied;" && visible) type.supplied = true
                if (descriptor != "Lcom/chunkzero/chunk/runtime/SessionType;") return null
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
                    arguments.size == 1 && arguments[0].descriptor == "Lcom/chunkzero/chunk/runtime/SessionCreation;"
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
