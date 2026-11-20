use nauvim_core::{Config, Engine, EngineEvent, Error};

#[tokio::main]
async fn main() -> Result<(), Error> {
    let (config, ()) = Config::load(None, "org.example.nauvim.events", |_opts| Ok(()))?;
    let mut engine = Engine::new(config)?;
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
