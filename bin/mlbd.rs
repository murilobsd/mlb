use mlb::create_app;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    pretty_env_logger::init();

    create_app().await?;
    Ok(())
}
