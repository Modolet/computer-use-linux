//! @file verified.rs
//! @brief 经真实 niri 无干扰测试验证的后台操作名单
//! @author modolet <y@xxyx.io>
//! @date 2026-09-07
use super::accessibility::Candidate;

// Nix store identities pin the complete application and GTK builds, not a
// spoofable app_id or a toolkit version presented by an arbitrary application.
const EDITOR: &str = "/nix/store/kj3j3wb6pyqqn3x6makv05wkam5lf08f-gnome-text-editor-50.1/bin/.gnome-text-editor-wrapped";
const GTK: &str = "/nix/store/4j905cjgk8idkzg4szhyryzjx2q1b6br-gtk4-4.22.4/lib/libgtk-4.so.";

pub fn editor(candidate: &Candidate) -> bool {
    cfg!(target_arch = "x86_64")
        && candidate.process.executable == std::path::Path::new(EDITOR)
        && candidate.toolkit == "GTK"
        && candidate.version == "4.22.4"
        && candidate.process.alive()
        && std::fs::read_to_string(format!("/proc/{}/maps", candidate.process.pid)).is_ok_and(
            |maps| {
                maps.lines().any(|line| {
                    line.split_whitespace()
                        .last()
                        .is_some_and(|path| path.starts_with(GTK))
                })
            },
        )
}

pub fn editable(role: &str, interfaces: &[String], states: &[u32]) -> bool {
    let flags = states.first().copied().unwrap_or(0);
    role == "text box"
        && interfaces
            .iter()
            .any(|i| i == "org.a11y.atspi.EditableText")
        && flags & (1 << 7) != 0
        && flags & (1 << 24) != 0
        && flags & (1 << 17) != 0
        && flags & (1 << 6) == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn noneditable_and_password_controls_are_never_allowed() {
        let interfaces = vec!["org.a11y.atspi.EditableText".into()];
        assert!(editable(
            "text box",
            &interfaces,
            &[(1 << 7) | (1 << 24) | (1 << 17)]
        ));
        assert!(!editable(
            "password text",
            &interfaces,
            &[(1 << 7) | (1 << 24) | (1 << 17)]
        ));
        assert!(!editable("text box", &interfaces, &[1 << 8]));
    }
}
