# chunk-build-api

The build plugin API that `block-build` and overworld implement: `configure`,
`build` and `dev` hooks over a `BuildContext` that exposes the application
layout, compiled classes, the contract, the manifest under construction, and
diagnostics. Kept separate from the Gradle plugin so implementors depend on a
small, stable API rather than on Gradle.
