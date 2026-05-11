#[derive(Debug, Clone, Copy)]
pub enum Action {
    Insert(char),
    DeletePrev,
    DeleteNext,
    MoveLeft,
    MoveRight,
    MoveUp,
    MoveDown,
    MoveHome,
    MoveEnd,
    PageUp,
    PageDown,
    Save,
    Quit,
    GoToDefinition,
    GoBack,
}
