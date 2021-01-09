use crate::backend::GenericSocketBackend;
use crate::codec::Message;
use crate::transport::AcceptStopHandle;
use crate::{
    BlockingSend, Endpoint, MultiPeerBackend, Socket, SocketBackend, SocketEvent, SocketType,
    ZmqResult,
};
use async_trait::async_trait;
use futures::channel::mpsc;
use std::collections::hash_map::RandomState;
use std::collections::HashMap;
use std::sync::Arc;

pub struct PushSocket {
    backend: Arc<GenericSocketBackend>,
    binds: HashMap<Endpoint, AcceptStopHandle>,
}

impl Drop for PushSocket {
    fn drop(&mut self) {
        self.backend.shutdown();
    }
}

#[async_trait]
impl Socket for PushSocket {
    fn new() -> Self {
        Self {
            backend: Arc::new(GenericSocketBackend::new(None, SocketType::PUSH)),
            binds: HashMap::new(),
        }
    }

    fn backend(&self) -> Arc<dyn MultiPeerBackend> {
        self.backend.clone()
    }

    fn binds(&mut self) -> &mut HashMap<Endpoint, AcceptStopHandle, RandomState> {
        &mut self.binds
    }

    fn monitor(&mut self) -> mpsc::Receiver<SocketEvent> {
        let (sender, receiver) = mpsc::channel(1024);
        self.backend.socket_monitor.lock().replace(sender);
        receiver
    }
}

#[async_trait]
impl BlockingSend for PushSocket {
    async fn send(&mut self, message: Message) -> ZmqResult<()> {
        self.backend.send_round_robin(message).await?;
        Ok(())
    }
}
