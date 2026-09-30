use std::io;

#[tokio::main]
async fn main() -> io::Result<()> {
    chunk_environment::logging();
    let config = chunk_environment::Config::from_env()?;
    chunk_service::run(|stop| chunk_environment::run(config, stop)).await
}
