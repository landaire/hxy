use std::borrow::Cow;

use gpui::AssetSource;
use gpui::Result;
use gpui::SharedString;
use gpui_component::IconNamed;
use rust_embed::RustEmbed;

/// Embedded app assets (icon SVGs under `assets/icons/`).
///
/// gpui-component ships no SVG files and registers no asset source, so
/// every `IconName` renders blank unless the app serves `icons/*.svg`
/// itself. Registered via `Application::with_assets` in `main`.
#[derive(RustEmbed)]
#[folder = "assets"]
pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        Ok(Self::get(path).map(|file| file.data))
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        Ok(Self::iter().filter(|entry| entry.starts_with(path)).map(SharedString::from).collect())
    }
}

/// Phosphor glyphs (vendored under `assets/icons/hxy/`) for egui icon
/// sites with no lucide `IconName` equivalent. Renders through
/// gpui-component's `Icon` via the `IconNamed` impl.
///
/// Call sites are wired by the M4g Task 4 icon parity sweep.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HxyIcon {
    Lock,
    LockOpen,
    PuzzlePiece,
    House,
    TreeStructure,
    Scroll,
    ImageSquare,
    SquaresFour,
}

impl IconNamed for HxyIcon {
    fn path(self) -> SharedString {
        match self {
            Self::Lock => "icons/hxy/lock.svg",
            Self::LockOpen => "icons/hxy/lock-open.svg",
            Self::PuzzlePiece => "icons/hxy/puzzle-piece.svg",
            Self::House => "icons/hxy/house.svg",
            Self::TreeStructure => "icons/hxy/tree-structure.svg",
            Self::Scroll => "icons/hxy/scroll.svg",
            Self::ImageSquare => "icons/hxy/image-square.svg",
            Self::SquaresFour => "icons/hxy/squares-four.svg",
        }
        .into()
    }
}

#[cfg(test)]
mod tests {
    use gpui_component::IconName;

    use super::*;

    fn assert_loads(path: SharedString) {
        let bytes =
            Assets.load(path.as_ref()).expect("asset source load").unwrap_or_else(|| panic!("missing asset: {path}"));
        assert!(!bytes.is_empty(), "empty asset: {path}");
        // window-restore.svg opens with an XML declaration, so check
        // containment rather than a "<svg" prefix.
        assert!(bytes.windows(4).any(|w| w == b"<svg"), "not an svg: {path}");
    }

    #[test]
    fn every_icon_name_resolves() {
        // Every IconName variant (gpui-component 0.5.1 icon.rs), not just
        // the ones hxy names directly: widgets the app uses (Checkbox,
        // Spinner, Notification, dock tab menus, NumberInput, sortable
        // Table headers) render further variants internally.
        let all = [
            IconName::ALargeSmall,
            IconName::ArrowDown,
            IconName::ArrowLeft,
            IconName::ArrowRight,
            IconName::ArrowUp,
            IconName::Asterisk,
            IconName::Bell,
            IconName::BookOpen,
            IconName::Bot,
            IconName::Building2,
            IconName::Calendar,
            IconName::CaseSensitive,
            IconName::ChartPie,
            IconName::Check,
            IconName::ChevronDown,
            IconName::ChevronLeft,
            IconName::ChevronRight,
            IconName::ChevronsUpDown,
            IconName::ChevronUp,
            IconName::CircleCheck,
            IconName::CircleUser,
            IconName::CircleX,
            IconName::Close,
            IconName::Copy,
            IconName::Dash,
            IconName::Delete,
            IconName::Ellipsis,
            IconName::EllipsisVertical,
            IconName::ExternalLink,
            IconName::Eye,
            IconName::EyeOff,
            IconName::File,
            IconName::Folder,
            IconName::FolderClosed,
            IconName::FolderOpen,
            IconName::Frame,
            IconName::GalleryVerticalEnd,
            IconName::GitHub,
            IconName::Globe,
            IconName::Heart,
            IconName::HeartOff,
            IconName::Inbox,
            IconName::Info,
            IconName::Inspector,
            IconName::LayoutDashboard,
            IconName::Loader,
            IconName::LoaderCircle,
            IconName::Map,
            IconName::Maximize,
            IconName::Menu,
            IconName::Minimize,
            IconName::Minus,
            IconName::Moon,
            IconName::Palette,
            IconName::PanelBottom,
            IconName::PanelBottomOpen,
            IconName::PanelLeft,
            IconName::PanelLeftClose,
            IconName::PanelLeftOpen,
            IconName::PanelRight,
            IconName::PanelRightClose,
            IconName::PanelRightOpen,
            IconName::Plus,
            IconName::Redo,
            IconName::Redo2,
            IconName::Replace,
            IconName::ResizeCorner,
            IconName::Search,
            IconName::Settings,
            IconName::Settings2,
            IconName::SortAscending,
            IconName::SortDescending,
            IconName::SquareTerminal,
            IconName::Star,
            IconName::StarOff,
            IconName::Sun,
            IconName::ThumbsDown,
            IconName::ThumbsUp,
            IconName::TriangleAlert,
            IconName::Undo,
            IconName::Undo2,
            IconName::User,
            IconName::WindowClose,
            IconName::WindowMaximize,
            IconName::WindowMinimize,
            IconName::WindowRestore,
        ];
        assert_eq!(all.len(), 86, "keep in sync with gpui-component icon.rs");
        for icon in all {
            assert_loads(icon.path());
        }
    }

    #[test]
    fn every_hxy_icon_resolves() {
        let all = [
            HxyIcon::Lock,
            HxyIcon::LockOpen,
            HxyIcon::PuzzlePiece,
            HxyIcon::House,
            HxyIcon::TreeStructure,
            HxyIcon::Scroll,
            HxyIcon::ImageSquare,
            HxyIcon::SquaresFour,
        ];
        for icon in all {
            assert_loads(icon.path());
        }
    }
}
