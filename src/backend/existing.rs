//! @file existing.rs
//! @brief 已有应用的范围绑定观察与后台能力门控
//! @author modolet <y@xxyx.io>
//! @date 2026-09-07

use super::{
    Backend, Cancellation,
    accessibility::{Accessibility, Candidate},
    portal::Portal,
};
use crate::model::*;
use uuid::Uuid;

pub struct Existing {
    accessibility: Accessibility,
    portal: Option<Portal>,
    target: Target,
    niri: std::path::PathBuf,
    process: super::process::ProcessIdentity,
    window_id: u64,
}
impl Existing {
    pub fn bind(candidate: Candidate, mut portal: Option<Portal>) -> Result<Self> {
        let target = Target {
            id: Uuid::new_v4().to_string(),
            label: candidate.label.clone(),
            width: 0,
            height: 0,
            scale: 1.0,
        };
        if let Some(portal) = portal.as_mut() {
            portal.bind_window(candidate.window_id)?;
        }
        let process = candidate.process.clone();
        let window_id = candidate.window_id;
        let niri = std::path::PathBuf::from(
            std::env::var_os("NIRI_SOCKET")
                .ok_or_else(|| Fault::unavailable("缺少 NIRI_SOCKET"))?,
        );
        Ok(Self {
            accessibility: Accessibility::bind(candidate)?,
            portal,
            target,
            niri,
            process,
            window_id,
        })
    }
}
impl Backend for Existing {
    fn detach(&mut self) {
        self.portal.take();
    }
    fn capabilities(&self) -> Vec<String> {
        let mut capabilities = vec!["accessibility_tree".into()];
        if self.portal.is_some() {
            capabilities.push("screenshot".into());
        }
        capabilities.extend(self.accessibility.mutation_capabilities());
        capabilities
    }
    fn targets(&mut self) -> Result<Vec<Target>> {
        if !self.alive() {
            return Err(Fault::stale("应用窗口已关闭"));
        }
        Ok(vec![self.target.clone()])
    }
    fn observe(&mut self, target: Option<&str>, cancel: &Cancellation) -> Result<Observation> {
        if target.is_some_and(|id| id != self.target.id) {
            return Err(Fault::denied("目标不属于获准应用"));
        }
        let nodes = self.accessibility.read(cancel)?;
        let png = if let Some(portal) = &self.portal {
            let (png, width, height) = portal.capture(cancel)?;
            self.target.width = width;
            self.target.height = height;
            Some(png)
        } else {
            None
        };
        if !self.alive() {
            return Err(Fault::stale("采集期间应用已退出"));
        }
        cancel.check()?;
        Ok(Observation {
            observation_id: Uuid::new_v4().to_string(),
            target: self.target.clone(),
            png_base64: png,
            nodes,
            windows: vec![],
        })
    }
    fn act(
        &mut self,
        observation: &Observation,
        action: &Action,
        cancel: &Cancellation,
    ) -> Result<()> {
        if observation.target.id != self.target.id || !self.alive() {
            return Err(Fault::stale("应用引用失效"));
        }
        self.accessibility.act(action, cancel)
    }
    fn alive(&mut self) -> bool {
        if !self.accessibility.alive() {
            return false;
        }
        matches!(super::desktop::niri_request(&self.niri,niri_ipc::Request::Windows),Ok(niri_ipc::Response::Windows(windows)) if windows.iter().any(|w|w.id==self.window_id && w.pid==Some(self.process.pid as i32)))
    }
}
