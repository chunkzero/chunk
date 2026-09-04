# chunk-runtime (JVM)

The session-process end of the runtime SPI. Started by chunk with an address
and a token, it dials that address once, registers the process's session
types, and exposes to `block-core`:

- the command stream: create session, end session, call, deliver, withdraw,
  prepare, stop, each answered by id
- one frame stream per delivered player, which block plugs into Minestom as a
  custom player connection; the JVM never sees a socket
- `EdgeCall`: invoke and subscribe, which the generated `Edge` client is built
  over
- events and health back to chunk

It ships inside apps under chunk's license and is the only chunk code that
runs in a session process. It knows nothing about Minestom.
