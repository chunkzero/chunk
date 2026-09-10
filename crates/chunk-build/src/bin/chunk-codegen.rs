use std::{env, io, path::Path};

use chunk_build::GenerationTarget;

fn main() -> io::Result<()> {
    let args: Vec<_> = env::args().skip(1).collect();
    let target = match args.as_slice() {
        [target, _, _, package] if target == "java" => GenerationTarget::Java { package },
        [target, _, _, package] if target == "kotlin" => GenerationTarget::Kotlin { package },
        [target, _, _] if target == "typescript" => GenerationTarget::TypeScript,
        _ => {
            return Err(io::Error::other(
                "usage: chunk-codegen java|kotlin CONTRACT OUTPUT JAVA_PACKAGE | chunk-codegen typescript CONTRACT OUTPUT",
            ));
        }
    };
    chunk_build::generate(Path::new(&args[1]), Path::new(&args[2]), target)
}
