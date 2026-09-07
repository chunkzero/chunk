# chunk-build-api

Scaffold for chunk's build integration API, separate from Gradle itself.
It will provide the context needed to build the application JAR, generated
clients, immutable assets and deployment manifest. Hook names and the exact
API remain open; no build API is implemented yet. App metadata, file-based
domains, command/hook manifests, and app Gradle module outputs contribute to
one project deployment. Generation must bootstrap before JVM compilation.
