use std::{env, io, path::Path};

fn main() -> io::Result<()> {
    let args: Vec<_> = env::args().skip(1).collect();
    if args.len() != 2 {
        return Err(io::Error::other("usage: chunk-compile PROJECT OUTPUT"));
    }
    chunk_build::compile(Path::new(&args[0]), Path::new(&args[1]))
}
