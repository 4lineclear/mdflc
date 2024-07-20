use mdflc::run;

#[tokio::main]
async fn main() -> miette::Result<()> {
    run().await
}
