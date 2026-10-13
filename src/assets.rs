//! Assets: the component library's icons plus the Lucide icons Paseo uses.

use std::borrow::Cow;

use gpui_kit::gpui::{AssetSource, Result, SharedString};

gpui_kit::assets::icon_assets!(
    PaseoIcons,
    [
        Plus,
        ListClock,
        Search,
        CalendarClock,
        FolderPlus,
        Server,
        Import,
        CircleQuestionMark,
        Settings,
        Settings2,
        ChevronDown,
        ChevronFirst,
        ChevronLast,
        ChevronLeft,
        ChevronRight,
        ChevronUp,
        ChevronsDownUp,
        FoldHorizontal,
        EllipsisVertical,
        Folder,
        FolderGit2,
        Monitor,
        CircleAlert,
        GitBranch,
        SquareTerminal,
        Eye,
        Pencil,
        Bot,
        CornerDownRight,
        Maximize2,
        Minimize2,
        Wrench,
        Brain,
        Sparkles,
        MicVocal,
        TriangleAlert,
        X,
        ArrowUp,
        ArrowLeft,
        Square,
        Copy,
        Check,
        PanelLeft,
        LayoutGrid,
        Kanban,
        Pin,
        PinOff,
        Archive,
        Tag,
        SquarePen,
        SquareCheck,
        CircleDot,
        Info,
        Globe,
        Clock,
        CircleCheck,
        CircleX,
        GitPullRequest,
        GitMerge,
        FileDiff,
        RefreshCw,
        Plug,
        Unplug,
        Activity,
        MousePointer,
        Layers,
        Circle,
        ListChecks,
        GripVertical,
        Aperture,
        Sparkle,
        Menu,
    ]
);

pub struct AppAssets;

impl AssetSource for AppAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if let Some(bytes) = PaseoIcons.load(path)? {
            return Ok(Some(bytes));
        }
        gpui_kit::assets::Assets.load(path)
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut out = PaseoIcons.list(path)?;
        out.extend(gpui_kit::assets::Assets.list(path)?);
        out.sort();
        out.dedup();
        Ok(out)
    }
}
