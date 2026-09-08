use std::{env, io, path::Path};

fn main() -> io::Result<()> {
    let args: Vec<_> = env::args().skip(1).collect();
    if args.len() != 3 {
        return Err(io::Error::other("usage: chunk-codegen CONTRACT OUTPUT JAVA_PACKAGE"));
    }
    chunk_build::generate(Path::new(&args[0]), Path::new(&args[1]), &args[2])
}
