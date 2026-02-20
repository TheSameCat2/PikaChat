use thiserror::Error;
use tokio::sync::{broadcast, mpsc};

use crate::types::{BackendCommand, BackendEvent};

pub type EventStream = broadcast::Receiver<BackendEvent>;

#[derive(Debug, Error)]
pub enum BackendChannelError {
    #[error("command channel is closed")]
    CommandChannelClosed,
}

#[derive(Clone, Debug)]
pub struct BackendChannels {
    command_tx: mpsc::Sender<BackendCommand>,
    event_tx: broadcast::Sender<BackendEvent>,
}

impl BackendChannels {
    pub fn new(
        command_buffer: usize,
        event_buffer: usize,
    ) -> (Self, mpsc::Receiver<BackendCommand>) {
        let (command_tx, command_rx) = mpsc::channel(command_buffer.max(1));
        let (event_tx, _) = broadcast::channel(event_buffer.max(1));

        (
            Self {
                command_tx,
                event_tx,
            },
            command_rx,
        )
    }

    pub fn command_sender(&self) -> mpsc::Sender<BackendCommand> {
        self.command_tx.clone()
    }

    pub fn event_sender(&self) -> broadcast::Sender<BackendEvent> {
        self.event_tx.clone()
    }

    pub fn subscribe(&self) -> EventStream {
        self.event_tx.subscribe()
    }

    pub async fn send_command(&self, command: BackendCommand) -> Result<(), BackendChannelError> {
        self.command_tx
            .send(command)
            .await
            .map_err(|_| BackendChannelError::CommandChannelClosed)
    }

    pub fn emit(&self, event: BackendEvent) {
        let _ = self.event_tx.send(event);
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::types::{BackendEvent, BackendLifecycleState};

    #[tokio::test]
    async fn sends_commands_to_receiver() {
        let (channels, mut rx) = BackendChannels::new(8, 8);
        channels
            .send_command(crate::types::BackendCommand::Init {
                homeserver: "https://matrix.example.org".into(),
                data_dir: PathBuf::from("/tmp/store"),
            })
            .await
            .expect("command send should work");

        let cmd = rx.recv().await.expect("receiver should have a command");
        match cmd {
            crate::types::BackendCommand::Init { homeserver, .. } => {
                assert_eq!(homeserver, "https://matrix.example.org")
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[tokio::test]
    async fn fans_out_events_to_subscribers() {
        let (channels, _) = BackendChannels::new(4, 16);
        let mut a = channels.subscribe();
        let mut b = channels.subscribe();

        channels.emit(BackendEvent::StateChanged {
            state: BackendLifecycleState::Configured,
        });

        let event_a = a.recv().await.expect("subscriber a should receive event");
        let event_b = b.recv().await.expect("subscriber b should receive event");
        assert_eq!(event_a, event_b);
    }
}
