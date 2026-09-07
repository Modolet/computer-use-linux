//! @file backend/mod.rs
//! @brief 桌面后端接口与可取消操作
//! @author modolet <y@xxyx.io>
//! @date 2026-09-07

use crate::model::*;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

#[derive(Clone)]
pub struct Cancellation {
    generation: Arc<AtomicU64>,
    captured: u64,
}
impl Cancellation {
    pub fn new(generation: Arc<AtomicU64>) -> Self {
        let captured = generation.load(Ordering::SeqCst);
        Self {
            generation,
            captured,
        }
    }
    pub fn check(&self) -> Result<()> {
        if self.generation.load(Ordering::SeqCst) != self.captured {
            Err(Fault::new(
                ErrorCode::Paused,
                "操作已取消，请在本地界面恢复",
            ))
        } else {
            Ok(())
        }
    }
}

pub trait Backend: Send {
    fn capabilities(&self) -> Vec<String>;
    fn targets(&mut self) -> Result<Vec<Target>>;
    fn observe(&mut self, target: Option<&str>, cancel: &Cancellation) -> Result<Observation>;
    /// Must validate live geometry and identity before sending the first event.
    fn act(
        &mut self,
        observation: &Observation,
        action: &Action,
        cancel: &Cancellation,
    ) -> Result<()>;
    fn alive(&mut self) -> bool;
    /// Release input resources; never terminate an application with unsaved work.
    fn detach(&mut self) {}
}
