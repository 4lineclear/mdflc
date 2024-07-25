use mdflc::run;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    run().await?;
    crossterm::terminal::disable_raw_mode()?;
    Ok(())
}
