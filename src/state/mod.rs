//! State actor: a single task owns [`PrinterState`] and is its only writer.
//! Readers get lock-free, internally-consistent snapshots via a `watch` channel.

pub mod eventtime;
pub mod objects;

use std::sync::Arc;

use tokio::sync::{mpsc, watch};

pub use objects::{KlippyState, PrintState, PrinterState, sheet_macro_name};

/// A mutation applied to the state by the actor.
type Update = Box<dyn FnOnce(&mut PrinterState) + Send>;

/// Handle to the state actor. Cheap to clone; share freely.
#[derive(Clone)]
pub struct StateHandle {
    tx: mpsc::UnboundedSender<Update>,
    rx: watch::Receiver<Arc<PrinterState>>,
}

impl StateHandle {
    /// Spawn the state actor and return a handle to it.
    pub fn spawn(initial: PrinterState) -> StateHandle {
        let (tx, rx_cmd) = mpsc::unbounded_channel::<Update>();
        let (tx_watch, rx_watch) = watch::channel(Arc::new(initial.clone()));
        tokio::spawn(run(initial, rx_cmd, tx_watch));
        StateHandle { tx, rx: rx_watch }
    }

    /// Apply a mutation. Fire-and-forget; ordering is preserved.
    pub fn update<F: FnOnce(&mut PrinterState) + Send + 'static>(&self, f: F) {
        let _ = self.tx.send(Box::new(f));
    }

    /// Latest snapshot. Lock-free clone of the current state.
    pub fn snapshot(&self) -> Arc<PrinterState> {
        self.rx.borrow().clone()
    }

    /// A fresh receiver for subscription pushers to await change notifications.
    pub fn subscribe(&self) -> watch::Receiver<Arc<PrinterState>> {
        self.rx.clone()
    }
}

async fn run(
    mut state: PrinterState,
    mut rx: mpsc::UnboundedReceiver<Update>,
    tx: watch::Sender<Arc<PrinterState>>,
) {
    while let Some(update) = rx.recv().await {
        update(&mut state);
        // Coalesce a burst of pending updates into a single published snapshot
        // (temps can update far faster than subscribers drain).
        while let Ok(next) = rx.try_recv() {
            next(&mut state);
        }
        let _ = tx.send(Arc::new(state.clone()));
    }
}
