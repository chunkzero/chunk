# chunk-runtime (JVM)

Runtime scaffold for chunk's JVM SDK and Minestom server framework. No runtime
client or gameplay framework is implemented yet. Separating the public API
from runtime implementation is a module-boundary decision still to be made.

A JVM belongs to one environment, immutable deployment and machine profile.
It hosts **multiple sessions**, each with scoped players, worlds, tasks and
cleanup. Developers declare session types and requirements; chunk creates,
places and drains sessions automatically. A session is not a process.

The framework will integrate Minestom player transport, generated versioned
backend clients, health reporting and lifecycle commands. Internal create/end
commands come from chunk, not application-managed provisioning. Connections
and relay topology remain open; there is no single-connection requirement.

World templates and deployment assets are immutable; gameplay can modify
worlds in memory. The framework does not save mutable worlds or promise recovery
of live simulation after a JVM crash.
Application-managed exports may use a separate object-storage facility later.
