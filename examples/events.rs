use nauvim_core::{Config, Engine, EngineEvent, Error};

#[tokio::main]
async fn main() -> Result<(), Error> {
    let mut engine = Engine::new(Config::load(None)?)?;
    engine.start(100, 30).await?;

    loop {
        match engine.next_event().await {
            EngineEvent::Frame(_frame) => {
                // A GUI application presents the frame through its Backend.
            }
            EngineEvent::Error(error) => eprintln!("{error}"),
            EngineEvent::Welcome { error } => {
                if let Some(error) = error {
                    eprintln!("{error}");
                }
                break;
            }
            EngineEvent::Shutdown => break,
            _ => {}
        }
    }

    engine.shutdown().await
}
