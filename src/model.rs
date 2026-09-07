//! @file model.rs
//! @brief MCP 与本地 IPC 共用的请求、能力和错误类型
//! @author modolet <y@xxyx.io>
//! @date 2026-09-07

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    Application,
    Desktop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Isolated,
    Existing,
    Desktop,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionRequest {
    pub scope: Scope,
    pub mode: Mode,
    /// Required for isolated mode: installed .desktop ID or unambiguous application name.
    /// No executable paths, shell commands or launch arguments are accepted.
    #[serde(default)]
    pub application: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionRef {
    pub session_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ObserveRequest {
    pub session_id: String,
    /// Opaque target returned by session_status or the previous observation.
    pub target: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ActRequest {
    pub session_id: String,
    pub observation_id: String,
    pub target: String,
    pub action: Action,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Button {
    Left,
    Middle,
    Right,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Modifier {
    Ctrl,
    Alt,
    Shift,
    Super,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Action {
    Click {
        at: Point,
        button: Button,
    },
    Drag {
        from: Point,
        to: Point,
    },
    Scroll {
        at: Point,
        dx: f64,
        dy: f64,
    },
    Key {
        key: String,
        #[serde(default)]
        modifiers: Vec<Modifier>,
    },
    Text {
        text: String,
    },
    SetText {
        node: String,
        text: String,
    },
    Invoke {
        node: String,
        action: String,
    },
    FocusWindow {
        window: String,
    },
}

impl Action {
    pub fn capability(&self) -> &'static str {
        match self {
            Self::Click { .. } => "click",
            Self::Drag { .. } => "drag",
            Self::Scroll { .. } => "scroll",
            Self::Key { .. } => "key",
            Self::Text { .. } => "text",
            Self::SetText { .. } => "set_text",
            Self::Invoke { .. } => "invoke",
            Self::FocusWindow { .. } => "focus_window",
        }
    }
    pub fn validate(&self, width: u32, height: u32) -> Result<()> {
        let point = |p: &Point| {
            if !p.x.is_finite()
                || !p.y.is_finite()
                || p.x < 0.0
                || p.y < 0.0
                || p.x >= f64::from(width)
                || p.y >= f64::from(height)
            {
                Err(Fault::invalid("坐标超出观察图像"))
            } else {
                Ok(())
            }
        };
        match self {
            Self::Click { at, .. } => point(at),
            Self::Drag { from, to } => {
                point(from)?;
                point(to)
            }
            Self::Scroll { at, dx, dy } => {
                point(at)?;
                if !dx.is_finite() || !dy.is_finite() || dx.abs() > 2000.0 || dy.abs() > 2000.0 {
                    return Err(Fault::invalid("滚动范围无效"));
                }
                Ok(())
            }
            Self::Text { text } | Self::SetText { text, .. } if text.len() > 65536 => {
                Err(Fault::invalid("文本超过 64 KiB"))
            }
            Self::Key { key, modifiers } if key.len() > 64 || modifiers.len() > 4 => {
                Err(Fault::invalid("按键参数无效"))
            }
            _ => Ok(()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Pending,
    Active,
    Paused,
    Denied,
    Closed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Target {
    pub id: String,
    pub label: String,
    pub width: u32,
    pub height: u32,
    pub scale: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionStatus {
    pub session_id: String,
    pub scope: Scope,
    pub mode: Mode,
    pub state: State,
    pub label: Option<String>,
    pub capabilities: Vec<String>,
    pub targets: Vec<Target>,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Node {
    pub id: String,
    pub name: String,
    pub role: String,
    pub text: Option<String>,
    pub actions: Vec<String>,
    pub children: Vec<Node>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Observation {
    pub observation_id: String,
    pub target: Target,
    pub png_base64: Option<String>,
    pub nodes: Vec<Node>,
    pub windows: Vec<Target>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
pub enum Request {
    RequestSession(SessionRequest),
    SessionStatus(SessionRef),
    Observe(ObserveRequest),
    Act(ActRequest),
    CloseSession(SessionRef),
    PauseAll,
    ShowUi,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum Response {
    Status(SessionStatus),
    Observation(Observation),
    Ok,
    Error(Fault),
}

#[derive(Debug, Clone, Serialize, Deserialize, thiserror::Error)]
#[error("{code:?}: {message}")]
pub struct Fault {
    pub code: ErrorCode,
    pub message: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    PermissionDenied,
    Paused,
    Unsupported,
    StaleTarget,
    BackendUnavailable,
    InvalidRequest,
    Busy,
}

pub type Result<T> = std::result::Result<T, Fault>;
impl Fault {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
    pub fn denied(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::PermissionDenied, message)
    }
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::InvalidRequest, message)
    }
    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::BackendUnavailable, message)
    }
    pub fn stale(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::StaleTarget, message)
    }
    pub fn unsupported(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Unsupported, message)
    }
}

impl From<anyhow::Error> for Fault {
    fn from(error: anyhow::Error) -> Self {
        Self::unavailable(format!("{error:#}"))
    }
}
