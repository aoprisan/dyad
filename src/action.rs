#[derive(Debug, Clone, Copy)]
pub enum Action {
    Insert(char),
    DeletePrev,
    DeleteNext,
    /// Empty the current line's contents, leaving the trailing newline
    /// (if any) in place and parking the cursor at column 0.
    ClearLine,
    MoveLeft,
    MoveRight,
    MoveUp,
    MoveDown,
    MoveWordLeft,
    MoveWordRight,
    MoveHome,
    MoveEnd,
    PageUp,
    PageDown,
    Save,
    Quit,
    GoToDefinition,
    GoToLine,
    GoBack,
    /// Ctrl-G — enter the "go" chord prefix. The next keystroke
    /// resolves to a destination (see `App::resolve_chord`).
    CtrlGPrefix,
    ShowType,
    Rename,
    ToggleTree,
    ToggleGitDiff,
    ToggleHistory,
    ToggleKeysHelp,
    OpenFile,
    OpenTypeSearch,
    OpenTextSearch,
    NewFile,
    ToggleAutosave,
    /// Flip between `Mode::View` (read-only TUI) and `Mode::Edit`. The
    /// agent path is unaffected — MCP edits keep landing in either mode.
    ToggleMode,
    Escape,
}
