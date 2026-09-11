//! @file applications.rs
//! @brief 从本机桌面条目解析用户待批准的应用，不接受模型命令或参数
//! @author modolet <y@xxyx.io>
//! @date 2026-09-07
use crate::model::*;
use gio_unix::DesktopAppInfo;
use gtk4::{gio, glib, prelude::*};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DesktopApplication {
    pub id: String,
    pub name: String,
    pub program: PathBuf,
    pub args: Vec<String>,
    pub directory: Option<PathBuf>,
    pub desktop_file: PathBuf,
}
impl DesktopApplication {
    pub fn resolve(request: &str) -> Result<Self> {
        // IDs have priority over display names, which may be translated or duplicated.
        if let Some(info) = DesktopAppInfo::new(request)
            .or_else(|| DesktopAppInfo::new(&format!("{request}.desktop")))
        {
            return Self::from_info(&info);
        }
        let matches: Vec<_> = gio::AppInfo::all()
            .into_iter()
            .filter_map(|info| info.downcast::<DesktopAppInfo>().ok())
            .filter(|info| !info.is_hidden() && !info.is_nodisplay())
            .filter(|info| {
                info.display_name().eq_ignore_ascii_case(request)
                    || info
                        .string("Name")
                        .is_some_and(|name| name.eq_ignore_ascii_case(request))
                    || info
                        .executable()
                        .file_name()
                        .is_some_and(|name| name.to_string_lossy().eq_ignore_ascii_case(request))
            })
            .collect();
        if matches.len() != 1 {
            return Err(Fault::invalid(
                "应用名称无法唯一对应本机桌面条目；请提供准确的 .desktop ID",
            ));
        }
        Self::from_info(&matches[0])
    }
    /// Local desktop files are trusted launch definitions; MCP accepts only an ID/name.
    pub fn from_file(path: &Path) -> Result<Self> {
        let info = DesktopAppInfo::from_filename(path)
            .ok_or_else(|| Fault::invalid("桌面应用条目无效"))?;
        Self::from_info(&info)
    }
    fn from_info(info: &DesktopAppInfo) -> Result<Self> {
        if info.is_hidden() || info.boolean("Terminal") {
            return Err(Fault::unsupported(
                "该条目已隐藏或需要外部终端；请申请独立的图形应用",
            ));
        }
        let file = info
            .filename()
            .ok_or_else(|| Fault::unsupported("应用缺少桌面条目文件"))?;
        let name = info.display_name().to_string();
        let exec = info.string("Exec").ok_or_else(|| {
            Fault::unsupported("应用没有可独立启动的 Exec；不使用宿主 D-Bus 激活")
        })?;
        let icon = info.string("Icon");
        let argv = expand_exec(&exec, &name, &file, icon.as_deref())?;
        let program = glib::find_program_in_path(&argv[0])
            .ok_or_else(|| Fault::unavailable("桌面条目的启动程序不存在或不可执行"))?;
        let directory = info
            .string("Path")
            .filter(|p| !p.is_empty())
            .map(PathBuf::from);
        if directory
            .as_ref()
            .is_some_and(|p| !p.is_absolute() || !p.is_dir())
        {
            return Err(Fault::unsupported("桌面条目的工作目录无效"));
        }
        Ok(Self {
            id: info
                .id()
                .map(|s| s.to_string())
                .unwrap_or_else(|| file.display().to_string()),
            name,
            program,
            args: argv.into_iter().skip(1).collect(),
            directory,
            desktop_file: file,
        })
    }
}

// Parse arguments without a shell. Field replacements stay a single argument;
// file/URL placeholders are removed because this request only launches an app.
fn expand_exec(exec: &str, name: &str, file: &Path, icon: Option<&str>) -> Result<Vec<String>> {
    let words = glib::shell_parse_argv(exec).map_err(|e| Fault::invalid(e.to_string()))?;
    let mut args = Vec::new();
    for word in words {
        let word = word
            .to_str()
            .ok_or_else(|| Fault::invalid("启动参数不是 UTF-8"))?;
        if matches!(
            word,
            "%f" | "%F" | "%u" | "%U" | "%d" | "%D" | "%n" | "%N" | "%v" | "%m"
        ) {
            continue;
        }
        if word == "%i" {
            if let Some(icon) = icon.filter(|s| !s.is_empty()) {
                args.extend(["--icon".into(), icon.into()]);
            }
            continue;
        }
        let mut expanded = String::new();
        let mut chars = word.chars();
        while let Some(c) = chars.next() {
            if c != '%' {
                expanded.push(c);
                continue;
            }
            match chars.next() {
                Some('%') => expanded.push('%'),
                Some('c') => expanded.push_str(name),
                Some('k') => expanded.push_str(&file.to_string_lossy()),
                Some('d' | 'D' | 'n' | 'N' | 'v' | 'm') => {}
                _ => return Err(Fault::unsupported("桌面条目包含未知或非独立的文件占位符")),
            }
        }
        args.push(expanded);
    }
    if args.first().is_none_or(|s| s.is_empty() || s.contains('=')) {
        return Err(Fault::invalid("桌面条目没有有效启动程序"));
    }
    Ok(args)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn desktop_arguments_are_expanded_without_shell_interpretation() {
        let file = Path::new("/tmp/an app.desktop");
        assert_eq!(
            expand_exec(
                "/bin/app --name %c %i %k %U \"literal; $(touch nope)\" %%",
                "中文 App",
                file,
                Some("icon name")
            )
            .unwrap(),
            [
                "/bin/app",
                "--name",
                "中文 App",
                "--icon",
                "icon name",
                "/tmp/an app.desktop",
                "literal; $(touch nope)",
                "%"
            ]
        );
        assert!(expand_exec("app %Z", "x", file, None).is_err());
        assert!(expand_exec("app --files=%F", "x", file, None).is_err());
    }
    #[test]
    fn desktop_entry_preserves_arguments_and_avoids_dbus_activation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("custom.desktop");
        std::fs::write(&path, "[Desktop Entry]\nType=Application\nName=自定义应用\nExec=/bin/sh -c \"exit 0\" %U\nDBusActivatable=true\n").unwrap();
        let app = DesktopApplication::from_file(&path).unwrap();
        assert_eq!(app.name, "自定义应用");
        assert_eq!(app.args, ["-c", "exit 0"]);
        std::fs::write(&path, "[Desktop Entry]\nType=Application\nName=Terminal action\nExec=/bin/sh\nTerminal=true\n").unwrap();
        assert!(DesktopApplication::from_file(&path).is_err());
    }
}
