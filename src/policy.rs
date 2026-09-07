//! @file policy.rs
//! @brief 连接绑定授权、撤销与观察生命周期
//! @author modolet <y@xxyx.io>
//! @date 2026-09-07

use crate::{
    backend::{Backend, Cancellation},
    model::*,
};
use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicU64, Ordering},
    },
};
use uuid::Uuid;

pub type BackendHandle = Arc<Mutex<Box<dyn Backend>>>;

pub struct Session {
    pub owner: Uuid,
    pub status: SessionStatus,
    pub generation: Arc<AtomicU64>,
    pub backend: Option<BackendHandle>,
    pub observation: Option<Observation>,
    pub visible_views: usize,
    pub requested_application: Option<String>,
}

#[derive(Default)]
pub struct Policy {
    pub sessions: HashMap<String, Session>,
    pub ui_visible: bool,
    pub ui_epoch: u64,
    pub manual_previews: usize,
    pub retained: Vec<(String, BackendHandle)>,
    clients: HashSet<Uuid>,
    input_backends: Vec<Weak<Mutex<Box<dyn Backend>>>>,
}

pub struct Permit {
    pub backend: BackendHandle,
    pub cancel: Cancellation,
    pub observation: Option<Observation>,
}

impl Policy {
    pub fn register(&mut self, owner: Uuid) {
        self.clients.insert(owner);
    }
    /// Includes a disconnected desktop while its last input worker is cleaning up.
    /// Weak references preserve no application or control authority themselves.
    pub fn input_handles(&self) -> Vec<BackendHandle> {
        self.input_backends
            .iter()
            .filter_map(Weak::upgrade)
            .collect()
    }
    pub fn open_ui(&mut self) {
        self.pause_all();
        self.ui_visible = true;
        self.ui_epoch = self.ui_epoch.wrapping_add(1);
    }
    pub fn request(&mut self, owner: Uuid, request: SessionRequest) -> Result<SessionStatus> {
        if !self.clients.contains(&owner) {
            return Err(Fault::denied("客户端连接已断开"));
        }
        if !matches!(
            (request.scope, request.mode),
            (Scope::Application, Mode::Isolated) | (Scope::Desktop, Mode::Desktop)
        ) {
            return Err(Fault::invalid("授权范围和模式不匹配"));
        }
        match (request.mode, request.application.as_deref()) {
            (Mode::Isolated, Some(name))
                if !name.is_empty()
                    && name.len() <= 256
                    && name.trim() == name
                    && !name
                        .chars()
                        .any(|c| c.is_control() || c == '/' || c == '\\') => {}
            (Mode::Isolated, _) => {
                return Err(Fault::invalid(
                    "独立实例必须指定 application：已安装应用的名称或 .desktop ID，不能传命令或路径",
                ));
            }
            (_, Some(_)) => return Err(Fault::invalid("application 参数仅适用于独立实例")),
            (_, None) => {}
        }
        if self
            .sessions
            .values()
            .filter(|s| {
                s.owner == owner && !matches!(s.status.state, State::Closed | State::Denied)
            })
            .count()
            >= 8
        {
            return Err(Fault::new(
                ErrorCode::Busy,
                "每个连接最多八个活动或待处理会话",
            ));
        }
        self.open_ui();
        let id = Uuid::new_v4().to_string();
        let status = SessionStatus {
            session_id: id.clone(),
            scope: request.scope,
            mode: request.mode,
            state: State::Pending,
            label: None,
            capabilities: vec![],
            targets: vec![],
            message: Some("等待本地用户授权".into()),
        };
        self.sessions.insert(
            id,
            Session {
                owner,
                status: status.clone(),
                generation: Arc::new(AtomicU64::new(0)),
                backend: None,
                observation: None,
                visible_views: 0,
                requested_application: request.application,
            },
        );
        Ok(status)
    }
    fn owned(&self, owner: Uuid, id: &str) -> Result<&Session> {
        self.sessions
            .get(id)
            .filter(|s| s.owner == owner)
            .ok_or_else(|| Fault::denied("会话不存在或不属于当前连接"))
    }
    pub fn status(&self, owner: Uuid, id: &str) -> Result<SessionStatus> {
        Ok(self.owned(owner, id)?.status.clone())
    }
    /// Called only by the in-process GTK controller, never exposed over IPC.
    pub fn grant(&mut self, id: &str, label: String, mut backend: Box<dyn Backend>) -> Result<()> {
        let session = self
            .sessions
            .get(id)
            .ok_or_else(|| Fault::stale("申请已结束"))?;
        if session.status.state != State::Pending {
            return Err(Fault::stale("申请已结束"));
        }
        let caps = backend.capabilities();
        let targets = backend.targets()?;
        let session = self.sessions.get_mut(id).unwrap();
        session.status.label = Some(label);
        session.status.capabilities = caps;
        session.status.targets = targets;
        session.status.state = State::Paused;
        session.status.message = Some("已授权；关闭授权窗口后可在本地恢复".into());
        let handle = Arc::new(Mutex::new(backend));
        self.input_backends.retain(|b| b.strong_count() > 0);
        self.input_backends.push(Arc::downgrade(&handle));
        session.backend = Some(handle);
        Ok(())
    }
    pub fn deny(&mut self, id: &str, message: String) {
        if let Some(s) = self.sessions.get_mut(id) {
            Self::stop(s, State::Denied);
            s.status.message = Some(message);
        }
    }
    pub fn resume(&mut self, id: &str) -> Result<()> {
        if self
            .sessions
            .get(id)
            .is_some_and(|s| s.status.mode == Mode::Isolated && s.visible_views == 0)
        {
            return Err(Fault::new(
                ErrorCode::Paused,
                "请先打开应用窗口；不允许隐藏运行",
            ));
        }
        let scope = self
            .sessions
            .get(id)
            .ok_or_else(|| Fault::stale("会话不存在"))?
            .status
            .scope;
        if self.ui_visible || self.manual_previews > 0 {
            return Err(Fault::new(
                ErrorCode::Paused,
                "请先关闭授权界面和手动预览窗口",
            ));
        }
        if scope == Scope::Desktop {
            for (other, s) in &mut self.sessions {
                if other != id
                    && s.status.scope == Scope::Desktop
                    && s.status.state == State::Active
                {
                    Self::stop(s, State::Paused);
                }
            }
        }
        let s = self.sessions.get_mut(id).unwrap();
        if s.status.state != State::Paused || s.backend.is_none() {
            return Err(Fault::denied("当前会话不能恢复"));
        }
        s.status.state = State::Active;
        s.status.message = None;
        Ok(())
    }
    fn stop(s: &mut Session, state: State) {
        s.generation.fetch_add(1, Ordering::SeqCst);
        s.observation = None;
        s.status.state = state;
    }
    pub fn pause(&mut self, id: &str) {
        if let Some(s) = self.sessions.get_mut(id)
            && s.status.state == State::Active
        {
            Self::stop(s, State::Paused);
        }
    }
    pub fn view_opened(&mut self, id: &str) {
        if let Some(s) = self.sessions.get_mut(id) {
            s.visible_views += 1;
        }
    }
    pub fn view_closed(&mut self, id: &str) {
        if let Some(s) = self.sessions.get_mut(id) {
            s.visible_views = s.visible_views.saturating_sub(1);
            if s.visible_views == 0 && s.status.state == State::Active {
                Self::stop(s, State::Paused);
            }
        }
    }
    pub fn action_permit(&mut self, owner: Uuid, request: &ActRequest) -> Result<Permit> {
        let permit = self.permit(owner, &request.session_id, Some(request))?;
        self.sessions
            .get_mut(&request.session_id)
            .unwrap()
            .observation = None;
        Ok(permit)
    }
    pub fn pause_all(&mut self) {
        for s in self.sessions.values_mut() {
            if s.status.state == State::Active {
                Self::stop(s, State::Paused);
            }
        }
    }
    pub fn close(&mut self, owner: Uuid, id: &str) -> Result<Option<BackendHandle>> {
        self.owned(owner, id)?;
        Ok(self.close_local(id))
    }
    pub fn close_local(&mut self, id: &str) -> Option<BackendHandle> {
        let s = self.sessions.get_mut(id)?;
        Self::stop(s, State::Closed);
        s.status.targets.clear();
        s.status.capabilities.clear();
        let backend = s.backend.take();
        if s.status.mode == Mode::Isolated
            && let Some(handle) = &backend
        {
            self.retained.push((
                s.status.label.clone().unwrap_or_else(|| "独立应用".into()),
                handle.clone(),
            ));
        }
        backend
    }
    pub fn disconnect(&mut self, owner: Uuid) -> Vec<BackendHandle> {
        self.clients.remove(&owner);
        let ids: Vec<_> = self
            .sessions
            .iter()
            .filter(|(_, s)| s.owner == owner)
            .map(|(id, _)| id.clone())
            .collect();
        let handles = ids.iter().filter_map(|id| self.close_local(id)).collect();
        self.sessions.retain(|_, s| s.owner != owner);
        handles
    }
    pub fn permit(&self, owner: Uuid, id: &str, action: Option<&ActRequest>) -> Result<Permit> {
        let s = self.owned(owner, id)?;
        if !matches!(s.status.state, State::Active | State::Paused) {
            return Err(Fault::denied("尚未获得有效授权"));
        }
        if action.is_some()
            && (s.status.state != State::Active || self.ui_visible || self.manual_previews > 0)
        {
            return Err(Fault::new(ErrorCode::Paused, "自动输入已暂停"));
        }
        if let Some(a) = action {
            if !s
                .status
                .capabilities
                .iter()
                .any(|c| c == a.action.capability())
            {
                return Err(Fault::unsupported("该后端未开放此操作；不会切换到整机输入"));
            }
            let o = s
                .observation
                .as_ref()
                .ok_or_else(|| Fault::stale("请先重新观察"))?;
            if o.observation_id != a.observation_id || o.target.id != a.target {
                return Err(Fault::stale("观察或目标已经失效"));
            }
            a.action.validate(o.target.width, o.target.height)?;
        }
        Ok(Permit {
            backend: s
                .backend
                .as_ref()
                .ok_or_else(|| Fault::unavailable("后端未连接"))?
                .clone(),
            cancel: Cancellation::new(s.generation.clone()),
            observation: s.observation.clone(),
        })
    }
    pub fn remember(
        &mut self,
        owner: Uuid,
        id: &str,
        observation: Observation,
        cancel: &Cancellation,
    ) -> Result<()> {
        self.owned(owner, id)?;
        cancel.check()?;
        let s = self.sessions.get_mut(id).unwrap();
        if !matches!(s.status.state, State::Active | State::Paused) {
            return Err(Fault::denied("授权已撤销"));
        }
        s.observation = Some(observation);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fake;
    impl Backend for Fake {
        fn capabilities(&self) -> Vec<String> {
            vec!["click".into()]
        }
        fn targets(&mut self) -> Result<Vec<Target>> {
            Ok(vec![])
        }
        fn observe(&mut self, _: Option<&str>, _: &Cancellation) -> Result<Observation> {
            unreachable!()
        }
        fn act(&mut self, _: &Observation, _: &Action, _: &Cancellation) -> Result<()> {
            unreachable!()
        }
        fn alive(&mut self) -> bool {
            true
        }
    }
    fn granted(scope: Scope, mode: Mode) -> (Policy, Uuid, String) {
        let mut p = Policy::default();
        let owner = Uuid::new_v4();
        p.register(owner);
        let id = p
            .request(
                owner,
                SessionRequest {
                    scope,
                    mode,
                    application: (mode == Mode::Isolated).then(|| "firefox".into()),
                },
            )
            .unwrap()
            .session_id;
        p.grant(&id, "test".into(), Box::new(Fake)).unwrap();
        p.view_opened(&id);
        p.ui_visible = false;
        p.resume(&id).unwrap();
        (p, owner, id)
    }
    #[test]
    fn disconnected_client_cannot_create_late_request() {
        let mut p = Policy::default();
        let owner = Uuid::new_v4();
        p.register(owner);
        p.disconnect(owner);
        assert!(
            p.request(
                owner,
                SessionRequest {
                    scope: Scope::Application,
                    mode: Mode::Isolated,
                    application: Some("gnome-text-editor".into()),
                }
            )
            .is_err()
        );
    }
    #[test]
    fn disconnected_isolated_application_remains_available_only_locally() {
        let (mut p, owner, id) = granted(Scope::Application, Mode::Isolated);
        p.disconnect(owner);
        assert_eq!(p.retained.len(), 1);
        assert!(p.status(owner, &id).is_err());
        assert!(p.permit(owner, &id, None).is_err());
    }
    #[test]
    fn visible_application_is_required_but_passive_view_does_not_pause() {
        let (mut p, owner, id) = granted(Scope::Application, Mode::Isolated);
        p.view_opened(&id);
        assert_eq!(p.status(owner, &id).unwrap().state, State::Active);
        p.view_closed(&id);
        assert_eq!(p.status(owner, &id).unwrap().state, State::Active);
        p.view_closed(&id);
        assert_eq!(p.status(owner, &id).unwrap().state, State::Paused);
        assert!(p.resume(&id).is_err());
        p.view_opened(&id);
        p.resume(&id).unwrap();
    }
    #[test]
    fn isolated_request_requires_an_application_without_command_paths() {
        let mut p = Policy::default();
        let owner = Uuid::new_v4();
        p.register(owner);
        for application in [
            None,
            Some("".into()),
            Some("/bin/sh".into()),
            Some("firefox\n".into()),
        ] {
            assert!(
                p.request(
                    owner,
                    SessionRequest {
                        scope: Scope::Application,
                        mode: Mode::Isolated,
                        application
                    }
                )
                .is_err()
            );
        }
        let status = p
            .request(
                owner,
                SessionRequest {
                    scope: Scope::Application,
                    mode: Mode::Isolated,
                    application: Some("kitty.desktop".into()),
                },
            )
            .unwrap();
        assert_eq!(
            p.sessions[&status.session_id]
                .requested_application
                .as_deref(),
            Some("kitty.desktop")
        );
        assert!(
            status.label.is_none() && status.capabilities.is_empty() && status.targets.is_empty()
        );
        assert!(p.permit(owner, &status.session_id, None).is_err());
    }

    #[test]
    fn pending_reveals_no_targets_and_cannot_observe() {
        let mut p = Policy::default();
        let owner = Uuid::new_v4();
        p.register(owner);
        let s = p
            .request(
                owner,
                SessionRequest {
                    scope: Scope::Application,
                    mode: Mode::Isolated,
                    application: Some("gnome-text-editor".into()),
                },
            )
            .unwrap();
        assert!(s.targets.is_empty() && s.label.is_none());
        assert!(p.permit(owner, &s.session_id, None).is_err());
    }
    #[test]
    fn ownership_and_disconnect_invalidate_authority() {
        let (mut p, owner, id) = granted(Scope::Application, Mode::Isolated);
        assert_eq!(
            p.status(Uuid::new_v4(), &id).unwrap_err().code,
            ErrorCode::PermissionDenied
        );
        let permit = p.permit(owner, &id, None).unwrap();
        p.disconnect(owner);
        assert!(permit.cancel.check().is_err());
        assert!(p.permit(owner, &id, None).is_err());
    }
    #[test]
    fn disconnected_input_is_included_in_authorization_barrier() {
        let (mut p, owner, id) = granted(Scope::Desktop, Mode::Desktop);
        let permit = p.permit(owner, &id, None).unwrap();
        drop(p.disconnect(owner));
        assert!(permit.cancel.check().is_err());
        assert_eq!(p.input_handles().len(), 1);
        drop(permit);
        assert!(p.input_handles().is_empty());
    }
    #[test]
    fn opening_approval_cancels_inflight_and_never_auto_resumes() {
        let (mut p, owner, id) = granted(Scope::Application, Mode::Isolated);
        let permit = p.permit(owner, &id, None).unwrap();
        p.request(
            owner,
            SessionRequest {
                scope: Scope::Desktop,
                mode: Mode::Desktop,
                application: None,
            },
        )
        .unwrap();
        assert!(permit.cancel.check().is_err());
        assert_eq!(p.status(owner, &id).unwrap().state, State::Paused);
        assert!(p.resume(&id).is_err());
        p.ui_visible = false;
        assert_eq!(p.status(owner, &id).unwrap().state, State::Paused);
    }
    #[test]
    fn desktop_input_has_single_owner() {
        let (mut p, owner, first) = granted(Scope::Desktop, Mode::Desktop);
        let second = p
            .request(
                owner,
                SessionRequest {
                    scope: Scope::Desktop,
                    mode: Mode::Desktop,
                    application: None,
                },
            )
            .unwrap()
            .session_id;
        p.grant(&second, "second".into(), Box::new(Fake)).unwrap();
        p.ui_visible = false;
        p.resume(&first).unwrap();
        p.resume(&second).unwrap();
        assert_eq!(p.status(owner, &first).unwrap().state, State::Paused);
    }
    #[test]
    fn revoke_during_observation_discards_result() {
        let (mut p, owner, id) = granted(Scope::Application, Mode::Isolated);
        let permit = p.permit(owner, &id, None).unwrap();
        p.close(owner, &id).unwrap();
        let o = Observation {
            observation_id: "o".into(),
            target: Target {
                id: "x".into(),
                label: "x".into(),
                width: 10,
                height: 10,
                scale: 1.0,
            },
            png_base64: None,
            nodes: vec![],
            windows: vec![],
        };
        assert!(p.remember(owner, &id, o, &permit.cancel).is_err());
    }
    #[test]
    fn rejects_stale_and_unsupported_actions_without_backend_fallback() {
        let (mut p, owner, id) = granted(Scope::Application, Mode::Isolated);
        let permit = p.permit(owner, &id, None).unwrap();
        let o = Observation {
            observation_id: "current".into(),
            target: Target {
                id: "target".into(),
                label: "x".into(),
                width: 10,
                height: 10,
                scale: 1.0,
            },
            png_base64: None,
            nodes: vec![],
            windows: vec![],
        };
        p.remember(owner, &id, o, &permit.cancel).unwrap();
        let mut a = ActRequest {
            session_id: id.clone(),
            observation_id: "old".into(),
            target: "target".into(),
            action: Action::Click {
                at: Point { x: 1.0, y: 1.0 },
                button: Button::Left,
            },
        };
        assert!(matches!(
            p.permit(owner, &id, Some(&a)),
            Err(Fault {
                code: ErrorCode::StaleTarget,
                ..
            })
        ));
        a.observation_id = "current".into();
        a.action = Action::FocusWindow {
            window: "other".into(),
        };
        assert!(matches!(
            p.permit(owner, &id, Some(&a)),
            Err(Fault {
                code: ErrorCode::Unsupported,
                ..
            })
        ));
        a.action = Action::Click {
            at: Point {
                x: f64::NAN,
                y: 1.0,
            },
            button: Button::Left,
        };
        assert!(p.permit(owner, &id, Some(&a)).is_err());
    }
}
