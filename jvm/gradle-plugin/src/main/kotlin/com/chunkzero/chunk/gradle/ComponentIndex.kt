package com.chunkzero.chunk.gradle

import org.objectweb.asm.AnnotationVisitor
import org.objectweb.asm.ClassReader
import org.objectweb.asm.ClassVisitor
import org.objectweb.asm.MethodVisitor
import org.objectweb.asm.Opcodes
import org.objectweb.asm.Type

internal data class ComponentFactory(
    val owner: String,
    val name: String,
    val type: String,
    val dependencies: List<String>,
    val scope: String,
)

internal data class ComponentMethod(
    val owner: String,
    val ownerAccess: Int,
    val name: String,
    val descriptor: String,
    val access: Int,
    val signature: String?,
    var scope: String = "",
) {
    val static get() = access and Opcodes.ACC_STATIC != 0
}

internal fun inspectComponents(bytes: ByteArray): List<ComponentMethod> {
    val methods = mutableListOf<ComponentMethod>()
    ClassReader(bytes).accept(
        object : ClassVisitor(Opcodes.ASM9) {
            var owner = ""
            var ownerAccess = 0

            override fun visit(
                version: Int,
                access: Int,
                name: String,
                signature: String?,
                superName: String?,
                interfaces: Array<out String>,
            ) {
                owner = name
                ownerAccess = access
            }

            override fun visitMethod(
                access: Int,
                name: String,
                descriptor: String,
                signature: String?,
                exceptions: Array<out String>?,
            ): MethodVisitor =
                object : MethodVisitor(Opcodes.ASM9) {
                    override fun visitAnnotation(
                        annotation: String,
                        visible: Boolean,
                    ): AnnotationVisitor? {
                        if (annotation != "Lcom/chunkzero/chunk/runtime/Component;") return null
                        val method = ComponentMethod(owner, ownerAccess, name, descriptor, access, signature)
                        methods.add(method)
                        return object : AnnotationVisitor(Opcodes.ASM9) {
                            override fun visitEnum(
                                name: String,
                                descriptor: String,
                                value: String,
                            ) {
                                if (name == "value" && descriptor == "Lcom/chunkzero/chunk/runtime/Component\$Scope;") {
                                    method.scope = value
                                }
                            }
                        }
                    }
                }
        },
        ClassReader.SKIP_CODE or ClassReader.SKIP_DEBUG or ClassReader.SKIP_FRAMES,
    )
    return methods
}

internal fun componentFactories(methods: List<ComponentMethod>): List<ComponentFactory> =
    methods
        .filterNot { method ->
            !method.static && method.owner.endsWith("\$Companion") &&
                methods.any {
                    it.static && it.owner == method.owner.removeSuffix("\$Companion") &&
                        it.name == method.name && it.descriptor == method.descriptor && it.scope == method.scope
                }
        }.map { method ->
            val identity = "${method.owner}.${method.name}"
            require(
                method.ownerAccess and Opcodes.ACC_PUBLIC != 0 && method.static &&
                    method.access and Opcodes.ACC_PUBLIC != 0 && method.signature == null &&
                    method.name.matches(Regex("[A-Za-z_$][A-Za-z0-9_$]*")),
            ) { "Component factory $identity must be public static and non-generic" }
            require(method.scope in listOf("PROCESS", "SESSION")) { "Component $identity requires an explicit scope" }

            fun reference(type: Type): String {
                require(type.sort == Type.OBJECT) { "Component $identity requires exact non-array reference types" }
                require(type.internalName.matches(Regex("[A-Za-z_$][A-Za-z0-9_$]*(/[A-Za-z_$][A-Za-z0-9_$]*)*"))) {
                    "Component $identity requires Java-compatible type names"
                }
                return type.internalName
            }
            require(method.owner.matches(Regex("[A-Za-z_$][A-Za-z0-9_$]*(/[A-Za-z_$][A-Za-z0-9_$]*)*"))) {
                "Component $identity requires a Java-compatible factory class"
            }
            ComponentFactory(
                method.owner,
                method.name,
                reference(Type.getReturnType(method.descriptor)),
                Type.getArgumentTypes(method.descriptor).map(::reference),
                method.scope,
            )
        }

internal fun validateComponents(factories: List<ComponentFactory>): List<ComponentFactory> {
    require(factories.size <= 256) { "App supports at most 256 component factories" }
    val byType = mutableMapOf<String, ComponentFactory>()
    val builtins =
        setOf("com/chunkzero/chunk/multistom/SessionScope", "com/chunkzero/chunk/backend/client/BackendSession")
    for (factory in factories) {
        require(factory.type !in builtins) { "Component cannot replace session builtin: ${factory.type}" }
        require(byType.put(factory.type, factory) == null) { "Duplicate component identity: ${factory.type}" }
        require(factory.dependencies.size <= 32) { "Component ${factory.type} supports at most 32 dependencies" }
    }
    val visiting = mutableSetOf<String>()
    val visited = mutableSetOf<String>()

    fun visit(type: String) {
        if (type in visited) return
        require(visiting.add(type)) { "Component dependency cycle: ${visiting.joinToString(" -> ")} -> $type" }
        val factory = requireNotNull(byType[type]) { "Missing component provider: $type" }
        for (dependency in factory.dependencies) {
            val target = byType[dependency]
            require(factory.scope != "PROCESS" || (dependency !in builtins && target?.scope != "SESSION")) {
                "Process component ${factory.type} captures session dependency $dependency"
            }
            if (dependency !in builtins) visit(dependency)
        }
        visiting.remove(type)
        visited.add(type)
    }
    byType.keys.sorted().forEach(::visit)
    return factories.sortedBy { it.type }
}
