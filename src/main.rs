use log::{warn};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    warn!("Startup Uart2Moon!");

    Ok(())
}
