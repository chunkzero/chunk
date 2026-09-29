use std::io;

#[tokio::main]
async fn main() -> io::Result<()> {
    chunk_service::logging();
    let config = chunk_edge::Config::from_env()?;
    let edge = chunk_edge::Edge::bind(config).await?;
    chunk_service::run(|stop| edge.run(stop)).await
}
